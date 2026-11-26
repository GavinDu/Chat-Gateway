import unittest,json
from attempt_collector import safe_sample,safe_timings,safe_header,safe_network,project_attempt,collect

class ProjectionTests(unittest.TestCase):
	def test_sample(self):
		b={'count':0,'populated_count':0,'text_bytes':0,'text_present':False,'matches_baseline':False,'streaming':False,'post_answer_affordance':False,'challenge':False,'text':'PRIVATE','url':'PRIVATE','token':'PRIVATE'}
		o=safe_sample(b)
		self.assertEqual(set(o),{'count','populated_count','text_bytes','text_present','matches_baseline','streaming','post_answer_affordance','challenge'})
		self.assertNotIn('PRIVATE',json.dumps(o))
		self.assertIsNot(o,b)
		for k,v in [('count',True),('count',1.0),('count',-1),('count',4097),('streaming',1)]:
			x=dict(b);x[k]=v;self.assertIsNone(safe_sample(x))
		x=dict(b);del x['challenge'];self.assertIsNone(safe_sample(x))
	def test_timings(self):
		a=frozenset({'submission'})
		self.assertEqual(safe_timings({},a),{})
		b={'submission':{'count':1,'sum_us':10,'max_us':5,'token':'PRIVATE'}}
		o=safe_timings(b,a)
		self.assertEqual(o,{'submission':{'count':1,'sum_us':10,'max_us':5}})
		self.assertNotIn('PRIVATE',json.dumps(o))
		self.assertIsNone(safe_timings({'other':{'count':1,'sum_us':1,'max_us':1}},a))
		for r in [{'count':True,'sum_us':1,'max_us':1},{'count':1,'sum_us':5,'max_us':6},{'count':0,'sum_us':1,'max_us':0},{'count':1,'sum_us':-1,'max_us':0}]:
			self.assertIsNone(safe_timings({'submission':r},a))


class FullProjectionTests(unittest.TestCase):
    def fixture(self):
        sample=dict(count=0,populated_count=0,text_bytes=0,text_present=False,matches_baseline=False,streaming=False,post_answer_affordance=False,challenge=False)
        return dict(schema_version=1,provider='chatgpt',scope='browser_attempt',sequence=1,sequence_available=True,future_returned=True,observation_incomplete=False,dropped_steps=0,dropped_updates=0,consumer_cancelled=False,submission_returned=False,first_content_seen=False,terminal_attempted=False,poll_streaming_seen=False,poll_affordance_seen=False,poll_nonbaseline_text_seen=False,elapsed_us=0,command_queue_us=0,polls=0,max_poll_response_count=0,max_poll_populated_count=0,last_stage='first_content',content_evidence='none',timing_semantics='scope_lifetime_including_cancellation',phases={},steps={},baseline=None,recent_samples=[sample],url='PRIVATE',token='PRIVATE')
    def test_header_gap_and_fallback_are_explicit(self):
        r=self.fixture();self.assertIsNotNone(safe_header(r));del r['sequence'];self.assertIsNone(safe_header(r))
        r=self.fixture();r.update(sequence=None,sequence_available=False);self.assertIsNone(safe_header(r));r['observation_incomplete']=True;self.assertIsNotNone(safe_header(r))
        r.update(reporting_error='state_borrowed',baseline='PRIVATE');self.assertNotIn('PRIVATE',json.dumps(project_attempt(r)));r['reporting_error']='PRIVATE';self.assertIsNone(project_attempt(r))
    def test_unknown_fields_cannot_leave_full_record(self):
        r=self.fixture();out=project_attempt(r);self.assertIsNotNone(out);self.assertNotIn('PRIVATE',json.dumps(out))
        r['steps']={'PRIVATE':dict(count=1,sum_us=2,max_us=2)};self.assertIsNone(project_attempt(r))
        r=self.fixture();r['recent_samples']*=9;self.assertIsNone(project_attempt(r))
        r=self.fixture();r['first_content_seen']=True;self.assertIsNone(project_attempt(r))
    def test_network_hosts_and_extra_fields_never_exported(self):
        r=dict(provider='chatgpt',host='private-account.example',resource_type='Fetch',status=200,response_bytes_observed=0,response_bytes_truncated=False,token='PRIVATE')
        out=safe_network(r);self.assertEqual(out['host_kind'],'other');self.assertNotIn('private-account',json.dumps(out));self.assertNotIn('PRIVATE',json.dumps(out))
        for host,expected in [('chatgpt.com','first_party'),('api.chatgpt.com','first_party'),('evilchatgpt.com','other')]:
            r['host']=host;self.assertEqual(safe_network(r)['host_kind'],expected)
        r['host']='https://private.example/?token=PRIVATE';self.assertIsNone(safe_network(r));r['host']='chatgpt.com';r['status']=True;self.assertIsNone(safe_network(r))
    def test_text_log_collection_is_bounded_and_fail_closed(self):
        r=self.fixture();line='2026-11-26T12:00:00.123456Z INFO ChatGPT browser attempt observation (content-free) observation='+json.dumps(r)
        net='2026-11-26T12:00:00Z DEBUG provider network response observed (metadata only) provider="chatgpt" host=private.example resource_type=Fetch status=429 response_bytes_observed=10 response_bytes_truncated=false'
        out=collect((line+'\n'+net+'\n').encode());self.assertEqual(len(out['records']),1);self.assertEqual(len(out['network']),1);self.assertEqual(out['network'][0]['status'],429);self.assertNotIn('PRIVATE',json.dumps(out));self.assertNotIn('private.example',json.dumps(out))
        out=collect((line.replace('observation={','observation=bad{')+'\n').encode());self.assertEqual(out['rejected_observations'],1)
        self.assertTrue(collect(b'x'*(2*1024*1024+1))['input_truncated'])

import unittest,json
from attempt_collector import safe_dispatch,project_network,collect
class NetworkTests(unittest.TestCase):
    def test_dispatch_bounds_and_no_secret_fields(self):
        row=dict(provider='chatgpt',scope='page_request',observation_limit=256,observation_sequence=1,method_kind='post',endpoint_kind='conversation_submit',url='PRIVATE',headers='PRIVATE')
        out=safe_dispatch(row);self.assertEqual(len(out),6);self.assertNotIn('PRIVATE',json.dumps(out));self.assertIsNot(out,row)
        for key,value in [('observation_sequence',0),('observation_sequence',257),('observation_sequence',True),('observation_limit',True),('scope','attempt'),('endpoint_kind','PRIVATE'),('method_kind','PRIVATE')]:
            bad=dict(row);bad[key]=value;self.assertIsNone(safe_dispatch(bad))
    def test_roles_are_optional_only_for_old_logs(self):
        row=dict(provider='chatgpt',host='chatgpt.com',resource_type='Fetch',status=200,response_bytes_observed=10,response_bytes_truncated=False,url='PRIVATE')
        self.assertFalse(project_network(row)['extended_metadata_available']);row['scope']='page_response';self.assertIsNone(project_network(row))
        row.update(request_endpoint_kind='conversation_submit',response_endpoint_kind='account',method_kind='post',content_type_kind='event_stream')
        out=project_network(row);self.assertTrue(out['extended_metadata_available']);self.assertEqual(out['response_endpoint_kind'],'account');self.assertNotIn('PRIVATE',json.dumps(out))
        row['response_endpoint_kind']='PRIVATE';self.assertIsNone(project_network(row))
    def test_parser_rejects_duplicate_keys_and_caps_events(self):
        line='2026-11-26T00:00:00Z DEBUG ChatGPT request dispatched (fixed metadata only) provider="chatgpt" scope="page_request" observation_limit=256 observation_sequence=1 method_kind="post" endpoint_kind="conversation_submit" url="PRIVATE"'
        out=collect(line.encode());self.assertEqual(len(out['dispatch']),1);self.assertNotIn('PRIVATE',json.dumps(out))
        out=collect((line+' provider="PRIVATE"').encode());self.assertEqual(out['rejected_dispatch_metadata'],1)
        out=collect(('\n'.join([line]*257)).encode());self.assertTrue(out['input_truncated']);self.assertEqual(len(out['dispatch']),256)
if __name__=='__main__':unittest.main()

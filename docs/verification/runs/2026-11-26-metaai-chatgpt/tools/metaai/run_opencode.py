import json,os,pathlib,re,signal,subprocess,sys,threading,time,urllib.request,urllib.error
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from telemetry_identity import docker_image_identity

ROOT=pathlib.Path(__file__).parent
mode=sys.argv[1]
trial=ROOT/mode
trial.mkdir(exist_ok=True,mode=0o700)
records=[]
rejected_posts=0
request_lock=threading.Lock()
request_finished=threading.Event()

def snapshot(name):
    began=time.monotonic()
    request=urllib.request.Request('http://127.0.0.1:8080/metrics')
    try:
        with urllib.request.urlopen(request,timeout=8) as response: data=response.read(2_000_001)
        if len(data)>2_000_000: raise ValueError()
        (telemetry/name).write_bytes(data)
    except Exception:
        (telemetry/(name+'.unavailable')).write_text('metrics_unavailable\n')
    return round(time.monotonic()-began,6)

prompt=(trial/'prompt.txt').read_text()
if len(prompt.encode()) > 40000: raise SystemExit('prompt_bundle_too_large')
telemetry=trial/'telemetry';telemetry.mkdir(exist_ok=True)

def stream(body,record,sink=None):
    index=record['index'];record['metrics_before_collection_s']=snapshot(f'request-{index}.before.prom')
    start=time.monotonic();record['_wire_start']=start;record['started_utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())
    headers={'Content-Type':'application/json'}
    if sink and sink.headers.get('Authorization'):headers['Authorization']=sink.headers['Authorization']
    if sink and sink.headers.get('X-Chat-Gateway-Request-Kind'):
        headers['X-Chat-Gateway-Request-Kind']=sink.headers['X-Chat-Gateway-Request-Kind']
    req=urllib.request.Request('http://127.0.0.1:8080/v1/chat/completions',data=json.dumps(body).encode(),headers=headers)
    try: response=urllib.request.urlopen(req,timeout=150)
    except urllib.error.HTTPError as error: response=error
    record.update(status=response.status,headers_s=round(time.monotonic()-start,3),request_id=response.headers.get('x-request-id'),response_provider_verified=response.headers.get('x-gateway-provider')=='metaai')
    if sink:
        sink.send_response(response.status)
        sink.send_header('Content-Type',response.headers.get('Content-Type','application/json'))
        sink.end_headers()
    previous_content=None
    with response:
        for line in response:
            now=round(time.monotonic()-start,3)
            if sink:
                try:sink.wfile.write(line);sink.wfile.flush()
                except (BrokenPipeError,ConnectionResetError):record['client_closed']=True;break
            if line.strip()==b'data: [DONE]': record['done_s']=now
            elif line.startswith(b'data: '):
                try: event=json.loads(line[6:])
                except ValueError:continue
                if 'error' in event:record['stream_error']=True
                if 'model' in event and isinstance(event.get('choices'),list):
                    record['response_model_observed']=True
                    record['response_model_verified']=record.get('response_model_verified',True) and event['model']=='metaai/meta-ai'
                if event.get('usage'):record['estimated_usage']=event['usage'];record['usage_estimated']=event.get('x_usage_estimated',True)
                for choice in event.get('choices',[]):
                    if choice.get('finish_reason') is not None:record['finish_reason']=choice['finish_reason']
                    content=choice.get('delta',{}).get('content','')
                    if content:
                        if previous_content is not None:record['max_inter_delta_gap_s']=max(record.get('max_inter_delta_gap_s',0),round(now-previous_content,3))
                        previous_content=now
                        record['content_deltas']=record.get('content_deltas',0)+1
                        record.setdefault('first_content_s',now)
                        record['last_content_s']=now
                        record['output_chars']=record.get('output_chars',0)+len(content)
    record['total_s']=round(time.monotonic()-start,3)
    record['metrics_after_collection_s']=snapshot(f'request-{index}.after.prom')
    if 'last_content_s' in record and 'done_s' in record:
        record['tail_wait_s']=round(record['done_s']-record['last_content_s'],3)

class Proxy(BaseHTTPRequestHandler):
    def log_message(self,*args):pass
    def do_POST(self):
        global rejected_posts
        if self.path!='/v1/chat/completions':self.send_error(404);return
        payload=self.rfile.read(int(self.headers['Content-Length']))
        body=json.loads(payload)
        with request_lock:
            if records:
                rejected_posts+=1
                self.send_error(429,'one_provider_call_per_job');return
            record={'index':len(records)+1,'kind':self.headers.get('X-CG-Bench-Kind','unknown'),
                'reported_gateway_kind':self.headers.get('X-Chat-Gateway-Request-Kind'),
                'input_chars':sum(len(m.get('content','')) if isinstance(m.get('content'),str) else 0 for m in body['messages']),
                'message_count':len(body['messages']),'stream':body.get('stream'),
                'input_payload_json_bytes':len(payload),'input_chars_scope':'string_message_content_only',
                'non_string_content_seen':any(not isinstance(m.get('content'),str) for m in body['messages'])}
            records.append(record)
        try:
            stream(body,record,self)
        except Exception as error:
            record['transport_failure']='upstream_timeout' if isinstance(error,(TimeoutError,)) else 'upstream_transport_error'
            record.setdefault('total_s',round(time.monotonic()-record.get('_wire_start',time.monotonic()),3))
        finally:
            if 'metrics_after_collection_s' not in record:
                record['metrics_after_collection_s']=snapshot(f"request-{record['index']}.after.prom")
            record.pop('_wire_start',None)
            request_finished.set()

if mode=='direct':
    record={'kind':'direct','input_chars':len(prompt),'message_count':1,'stream':True};records.append(record)
    stream({'model':'metaai/meta-ai','stream':True,'messages':[{'role':'user','content':prompt}]},record)
    identity_after_started=time.monotonic()
    try:
        with urllib.request.urlopen('http://127.0.0.1:8080/health',timeout=8) as identity_response:after_health=json.load(identity_response)
        runtime_identity_after={'build':after_health.get('build'),'docker':docker_image_identity()}
    except Exception:runtime_identity_after={'available':False,'reason':'identity_unavailable'}
    identity_after_elapsed=round(time.monotonic()-identity_after_started,6)
    result={'mode':mode,'requests':records}
else:
    server=ThreadingHTTPServer(('127.0.0.1',0),Proxy)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    for d in ['config','data','cache','state','xdg-config','workspace','profiler']:(trial/d).mkdir(exist_ok=True)
    env=dict(os.environ)
    for k,d in [('OPENCODE_CONFIG_DIR','config'),('XDG_DATA_HOME','data'),('XDG_CACHE_HOME','cache'),('XDG_STATE_HOME','state'),('XDG_CONFIG_HOME','xdg-config')]:env[k]=str(trial/d)
    env.update(OPENCODE_DISABLE_MODELS_FETCH='1',OPENCODE_CONFIG_PROJECT_DISABLE='1',OPENCODE_DISABLE_CLAUDE_CODE='1',OPENCODE_DISABLE_FILEWATCHER='1')
    config=json.loads(re.sub(r'(?m)^\s*//[^\n]*(?:\n|$)','',pathlib.Path('/Users/fangdu/.config/opencode/opencode.jsonc').read_text()))
    config['providers']['chat-gateway']['settings']['timeout']=150000
    config['providers']['chat-gateway']['settings']['baseURL']=f'http://127.0.0.1:{server.server_port}/v1'
    plugin=trial/'profiler'
    (plugin/'package.json').write_text(json.dumps({'name':'cg-perf-profiler','type':'module','exports':'./index.js'}))
    hook="""import {writeFileSync} from 'node:fs';
    export default {id:'test.cg-perf', async setup(ctx){
      await ctx.session.hook('http.request', e=>{e.request.headers.set('X-CG-Bench-Kind',e.kind);},{providerID:'chat-gateway'});
      await ctx.session.hook('retry',e=>{e.decision={retry:false};},{providerID:'chat-gateway'});
    REPLACE_TITLE
    }};
    """
    title_hook="await ctx.session.hook('title', e=>{e.result='Performance test';},{providerID:'chat-gateway'});" if mode=='local-title' else ''
    if mode=='installed':
        title_hook="await ctx.session.hook('title',e=>{writeFileSync("+json.dumps(str(trial/'local-title.json'))+",JSON.stringify({resolved_locally:e.result!==undefined,matches_first_message:e.result==="+json.dumps(prompt)+"}));},{providerID:'chat-gateway'});"
    hook=hook.replace('REPLACE_TITLE',title_hook)
    (plugin/'index.js').write_text(hook)
    config.setdefault('plugins',[]).append(str(plugin))
    config['agents']={'patch':{'mode':'primary','steps':4,'system':'You implement the scoped software task supplied by the user. You have no tools. Use only supplied source. Return exactly the machine-readable format requested by the user. Do not summarize unfinished work. Preserve all repository instructions supplied in the task. Never invent test results or read credentials.','permissions':[{'action':'*','resource':'*','effect':'deny'}]}}
    config['default_agent']='patch'
    (trial/'config'/'opencode.json').write_text(json.dumps(config,indent=2))
    (trial/'config'/'opencode.json').chmod(0o600)
    with urllib.request.urlopen('http://127.0.0.1:8080/health',timeout=8) as identity_response:identity=json.load(identity_response)
    runtime_identity={'build':identity.get('build'),'enabled_providers':[p['name'] for p in identity.get('providers',[]) if p.get('enabled')],'docker':docker_image_identity()}
    cmd=['/Users/fangdu/.opencode/bin/opencode','run','--standalone','--archived','--model','chat-gateway/metaai','--agent','patch','--format','json',prompt]
    start=time.monotonic()
    p=subprocess.Popen(cmd,cwd=trial/'workspace',env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
    resource_file=telemetry/'resources.jsonl'
    resource_file.touch(mode=0o600)
    resource_stop=threading.Event()
    resource_process=subprocess.Popen(['docker','stats','--format','{{json .}}','chat-gateway'],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True)
    def resources():
        with resource_file.open('a') as out:
            for line in resource_process.stdout:
                try:
                    line=re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]','',line)
                    row=json.loads(line);safe={k:row.get(k) for k in ['CPUPerc','MemUsage','NetIO','BlockIO','PIDs']};safe['monotonic_offset_s']=round(time.monotonic()-start,3);out.write(json.dumps(safe)+'\n');out.flush()
                except (ValueError,TypeError):pass
    resource_thread=threading.Thread(target=resources,daemon=True);resource_thread.start()
    timeout=False
    try:stdout,stderr=p.communicate(timeout=240)
    except subprocess.TimeoutExpired:
        timeout=True;os.killpg(p.pid,signal.SIGTERM);stdout,stderr=p.communicate(timeout=10)
    cli_elapsed=round(time.monotonic()-start,3)
    collector_finished=request_finished.wait(timeout=35) if records else True
    resource_process.terminate()
    try:resource_process.wait(timeout=5)
    except subprocess.TimeoutExpired:resource_process.kill();resource_process.wait()
    resource_thread.join(timeout=2)
    for name,value in [('stdout.log',stdout),('stderr.log',stderr)]:
        f=trial/name;f.write_text(value);f.chmod(0o600)
    text_parts=[]
    types=[]
    for line in stdout.splitlines():
        try:
            event=json.loads(line);types.append(event.get('type'))
            if event.get('type')=='text' and isinstance(event.get('part',{}).get('text'),str):text_parts.append(event['part']['text'])
        except ValueError:pass
    generated=trial/'response.txt';generated.write_text(''.join(text_parts));generated.chmod(0o600)
    identity_after_started=time.monotonic()
    try:
        with urllib.request.urlopen('http://127.0.0.1:8080/health',timeout=8) as identity_response:after_health=json.load(identity_response)
        runtime_identity_after={'build':after_health.get('build'),'docker':docker_image_identity()}
    except Exception:runtime_identity_after={'available':False,'reason':'identity_unavailable'}
    identity_after_elapsed=round(time.monotonic()-identity_after_started,6)
    result={'collector_schema_version':3,'proxy_rejected_budget_posts':rejected_posts,'proxy_forwarded_gateway_attempts':len(records),'single_after_snapshot':True,'atomic_one_attempt_admission':True,'mode':mode,'workflow':'opencode_metaai_scoped_patch','runtime_identity':runtime_identity,'runtime_identity_after':runtime_identity_after,'identity_after_collection_s':identity_after_elapsed,'native_tool_calls':False,'response_chars':sum(map(len,text_parts)),'collector_request_finished':collector_finished,'cli_seconds':cli_elapsed,'exit_code':p.returncode,'timeout':timeout,'events':types,'requests':records}
    before_docker=runtime_identity.get('docker',{});after_docker=runtime_identity_after.get('docker',{})
    result['image_identity_observed_stable']=bool(before_docker.get('available') and after_docker.get('available') and before_docker.get('image_id')==after_docker.get('image_id') and before_docker.get('source_sha')==after_docker.get('source_sha')==runtime_identity.get('build',{}).get('source_sha')==runtime_identity_after.get('build',{}).get('source_sha'))
    server.shutdown()
    if (trial/'local-title.json').exists(): result['local_title']=json.loads((trial/'local-title.json').read_text())
(trial/'result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))

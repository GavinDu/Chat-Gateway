"""Closed projection of content-free ChatGPT attempt records; raw logs stay private."""
import re

def safe_sample(row):
    if not isinstance(row, dict):
        return None
    if 'count' not in row or 'populated_count' not in row or 'text_bytes' not in row or ('text_present' not in row) or ('matches_baseline' not in row) or ('streaming' not in row) or ('post_answer_affordance' not in row) or ('challenge' not in row):
        return None
    c = row['count']
    p = row['populated_count']
    b = row['text_bytes']
    t = row['text_present']
    m = row['matches_baseline']
    s = row['streaming']
    a = row['post_answer_affordance']
    h = row['challenge']
    if type(c) is not int or c < 0 or c > 4096:
        return None
    if type(p) is not int or p < 0 or p > 4096:
        return None
    if type(b) is not int or b < 0 or b > 1048576:
        return None
    if type(t) is not bool or type(m) is not bool or type(s) is not bool or (type(a) is not bool) or (type(h) is not bool):
        return None
    return {'count': c, 'populated_count': p, 'text_bytes': b, 'text_present': t, 'matches_baseline': m, 'streaming': s, 'post_answer_affordance': a, 'challenge': h}

def safe_timings(value, allowed_labels):
    if type(value) is not dict:
        return None
    if len(value) > len(allowed_labels):
        return None
    out = {}
    for (k, row) in value.items():
        if type(k) is not str or k not in allowed_labels:
            return None
        if type(row) is not dict:
            return None
        if 'count' not in row or 'sum_us' not in row or 'max_us' not in row:
            return None
        c = row['count']
        s = row['sum_us']
        m = row['max_us']
        if type(c) is not int or type(s) is not int or type(m) is not int:
            return None
        if c < 0 or c > 1000000 or s < 0 or (s > 1800000000) or (m < 0) or (m > 1800000000):
            return None
        if m > s or (c == 0 and (s != 0 or m != 0)):
            return None
        out[k] = {'count': c, 'sum_us': s, 'max_us': m}
    return out

def safe_header(row):
    if type(row) is not dict:
        return None
    if 'sequence' not in row:
        return None
    if 'schema_version' not in row:
        return None
    if 'provider' not in row:
        return None
    if 'scope' not in row:
        return None
    if 'sequence_available' not in row:
        return None
    if 'future_returned' not in row:
        return None
    if 'observation_incomplete' not in row:
        return None
    if 'dropped_steps' not in row:
        return None
    if 'dropped_updates' not in row:
        return None
    v = row['schema_version']
    if type(v) is not int:
        return None
    if v != 1:
        return None
    if row['provider'] != 'chatgpt':
        return None
    if row['scope'] != 'browser_attempt':
        return None
    a = row['sequence_available']
    if type(a) is not bool:
        return None
    b = row['future_returned']
    if type(b) is not bool:
        return None
    c = row['observation_incomplete']
    if type(c) is not bool:
        return None
    d = row['dropped_steps']
    if type(d) is not int:
        return None
    if d < 0:
        return None
    if d > 1000000:
        return None
    e = row['dropped_updates']
    if type(e) is not int:
        return None
    if e < 0:
        return None
    if e > 1000000:
        return None
    if 'sequence' in row:
        q = row['sequence']
    else:
        q = None
    if q is not None:
        if type(q) is not int:
            return None
        if q < 1:
            return None
        if q > 18446744073709551615:
            return None
    if a != (q is not None):
        return None
    if q is None:
        if c is not True:
            return None
    if d > 0:
        if c is not True:
            return None
    if e > 0:
        if c is not True:
            return None
    o = {}
    o['schema_version'] = 1
    o['provider'] = 'chatgpt'
    o['scope'] = 'browser_attempt'
    o['sequence'] = q
    o['sequence_available'] = a
    o['future_returned'] = b
    o['observation_incomplete'] = c
    o['dropped_steps'] = d
    o['dropped_updates'] = e
    if 'reporting_error' in row:
        r = row['reporting_error']
        if r != 'state_borrowed':
            return None
        if c is not True:
            return None
        o['reporting_error'] = 'state_borrowed'
    return o
PHASE_LABELS = frozenset(['account_readiness', 'after_content', 'baseline', 'composer_ready', 'entry_guards', 'first_content', 'model_selection', 'navigation', 'operation_admission', 'page_restore', 'session_snapshot', 'submission'])
STEP_LABELS = frozenset(['ack_probe', 'ack_settle', 'ack_sleep', 'after_content_probe', 'after_content_processing', 'after_content_settle', 'after_content_sleep', 'baseline_probe', 'browser_settle', 'browser_sleep', 'command_queue', 'composer_probe', 'composer_ready', 'composer_settle', 'composer_sleep', 'composer_wait', 'draft_validation', 'entry_guards', 'final_probe', 'first_content_probe', 'first_content_processing', 'first_content_settle', 'first_content_sleep', 'input_fill', 'input_retention', 'input_settle', 'input_settle_engine', 'input_settle_sleep', 'model_selection', 'navigation', 'navigation_after_response', 'navigation_before_request', 'navigation_fetch', 'navigation_unobserved', 'operation_gate', 'page_ensure', 'paste_dispatch', 'retention_probe', 'retention_settle', 'retention_sleep', 'send_dispatch', 'session_snapshot', 'submission', 'submission_ack'])
MAX_US = 1800000000
MAX_COUNT = 1000000

def bounded_int(value, maximum):
    return type(value) is int and 0 <= value <= maximum

def project_attempt(row):
    out = safe_header(row)
    if out is None:
        return None
    if out.get('reporting_error') == 'state_borrowed':
        return out
    if row.get('timing_semantics') != 'scope_lifetime_including_cancellation':
        return None
    stage = row.get('last_stage')
    if type(stage) is not str or stage not in PHASE_LABELS:
        return None
    evidence = row.get('content_evidence')
    if type(evidence) is not str or evidence not in {'none', 'fresh_delta', 'verified_identical_reply'}:
        return None
    flags = ['consumer_cancelled', 'submission_returned', 'first_content_seen', 'terminal_attempted', 'poll_streaming_seen', 'poll_affordance_seen', 'poll_nonbaseline_text_seen']
    if any((type(row.get(k)) is not bool for k in flags)):
        return None
    if row['first_content_seen'] != (evidence != 'none'):
        return None
    bounds = {'elapsed_us': MAX_US, 'command_queue_us': MAX_US, 'polls': MAX_COUNT, 'max_poll_response_count': 4096, 'max_poll_populated_count': 4096}
    if any((not bounded_int(row.get(k), v) for (k, v) in bounds.items())):
        return None
    phases = safe_timings(row.get('phases'), PHASE_LABELS)
    steps = safe_timings(row.get('steps'), STEP_LABELS)
    if phases is None or steps is None:
        return None
    if 'baseline' not in row:
        return None
    baseline = None if row['baseline'] is None else safe_sample(row['baseline'])
    if row['baseline'] is not None and baseline is None:
        return None
    recent = row.get('recent_samples')
    if type(recent) is not list or len(recent) > 8:
        return None
    samples = [safe_sample(r) for r in recent]
    if any((s is None for s in samples)):
        return None
    out.update({k: row[k] for k in flags})
    out.update({k: row[k] for k in bounds})
    out.update(last_stage=stage, content_evidence=evidence, timing_semantics='scope_lifetime_including_cancellation', phases=phases, steps=steps, baseline=baseline, recent_samples=samples)
    return out

def event_time(line):
    import datetime
    stamp=re.match(r'^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,9})?Z)',line)
    try:
        return datetime.datetime.fromisoformat(stamp[1].replace('Z','+00:00')).isoformat().replace('+00:00','Z') if stamp else None
    except ValueError:
        return None

def collect(data):
    import json
    maximum=2*1024*1024
    truncated=len(data)>maximum
    records=[];network=[];dispatch=[];rejected=0;network_rejected=0;dispatch_rejected=0
    for raw in data[:maximum].decode('utf-8','replace').splitlines():
        line=re.sub(r'\x1b\[[0-9;]*m','',raw)
        if 'ChatGPT request dispatched (fixed metadata only)' in line:
            if len(dispatch)>=256:
                truncated=True
                continue
            try:
                fields=parse_fields(line,['provider','scope','observation_limit','observation_sequence','method_kind','endpoint_kind'])
                row=safe_dispatch(fields)
            except (ValueError,TypeError):row=None
            if row is None:dispatch_rejected+=1
            else:row['observed_at_utc']=event_time(line);dispatch.append(row)
            continue
        if 'provider network response observed (metadata only)' in line:
            if len(network)>=512:
                truncated=True
                continue
            fields={}
            try:
                for key in ['provider','host','resource_type','status','response_bytes_observed','response_bytes_truncated']:
                    matches=re.findall(r'\b'+key+r'=("[^"\n]*"|[^\s]+)',line)
                    if len(matches)!=1:raise ValueError('ambiguous_metadata')
                    token=matches[0]
                    try:fields[key]=json.loads(token)
                    except ValueError:fields[key]=token
                optional=['scope','request_endpoint_kind','response_endpoint_kind','method_kind','content_type_kind']
                present=[k for k in optional if re.search(r'\b'+k+r'=',line)]
                if present:
                    fields.update(parse_fields(line,optional))
                row=project_network(fields)
            except (ValueError,TypeError,KeyError):
                row=None
            if row is None:network_rejected+=1
            else:row['observed_at_utc']=event_time(line);network.append(row)
            continue
        if 'ChatGPT browser attempt observation (content-free)' not in line:
            continue
        if len(records)>=64:
            truncated=True
            continue
        match=re.search(r'\bobservation=(\{.*\})\s*$',line)
        try:
            row=project_attempt(json.loads(match[1])) if match else None
        except (ValueError,TypeError,KeyError,RecursionError,OverflowError):
            row=None
        if row is None:rejected+=1
        else:row['observed_at_utc']=event_time(line);records.append(row)
    return {'schema_version':1,'records':records,'network':network,'dispatch':dispatch,'rejected_dispatch_metadata':dispatch_rejected,'rejected_observations':rejected,'rejected_network_metadata':network_rejected,'input_truncated':truncated}


def safe_network(row):
    if not isinstance(row, dict):
        return None
    if row.get('provider') != 'chatgpt':
        return None
    host = row.get('host')
    if type(host) is not str or not 1 <= len(host) <= 128 or re.fullmatch('[A-Za-z0-9.-]+', host) is None:
        return None
    rt = row.get('resource_type')
    if rt not in ('Fetch', 'Xhr'):
        return None
    status = row.get('status')
    if type(status) is not int or not 100 <= status <= 599:
        return None
    obs = row.get('response_bytes_observed')
    if type(obs) is not int or not 0 <= obs <= 1048576:
        return None
    trunc = row.get('response_bytes_truncated')
    if type(trunc) is not bool:
        return None
    h = host.lower()
    kind = 'first_party' if h == 'chatgpt.com' or h.endswith('.chatgpt.com') else 'other'
    return {'provider': 'chatgpt', 'resource_type': rt.lower(), 'host_kind': kind, 'status': status, 'response_bytes_observed': obs, 'response_bytes_truncated': trunc}
def safe_dispatch(row):
    if not isinstance(row, dict): return None
    if row.get("provider") != "chatgpt": return None
    if row.get("scope") != "page_request": return None
    limit = row.get("observation_limit")
    if type(limit) is not int or limit != 256: return None
    seq = row.get("observation_sequence")
    if type(seq) is not int or seq < 1 or seq > 256: return None
    method = row.get("method_kind")
    if method not in ("get", "post", "other"): return None
    endpoint = row.get("endpoint_kind")
    if endpoint not in ("other", "auth_session", "account", "conversation_submit", "conversation_prepare", "conversation_resume", "websocket_bridge", "static_asset", "first_party_other"): return None
    return {"provider": "chatgpt", "scope": "page_request", "observation_limit": 256, "observation_sequence": seq, "method_kind": method, "endpoint_kind": endpoint}


ENDPOINT_KINDS=frozenset(['other','auth_session','account','conversation_submit','conversation_prepare','conversation_resume','websocket_bridge','static_asset','first_party_other'])
def parse_fields(line,keys):
    import json
    fields={}
    for key in keys:
        matches=re.findall(r'\b'+key+r'=("[^"\n]*"|[^\s]+)',line)
        if len(matches)!=1:raise ValueError('ambiguous_metadata')
        try:fields[key]=json.loads(matches[0])
        except ValueError:fields[key]=matches[0]
    return fields

def project_network(fields):
    out=safe_network(fields)
    if out is None:return None
    names=['scope','request_endpoint_kind','response_endpoint_kind','method_kind','content_type_kind']
    if not any(k in fields for k in names):
        out['extended_metadata_available']=False
        return out
    if fields.get('scope')!='page_response':return None
    if any(type(fields.get(k)) is not str or fields[k] not in ENDPOINT_KINDS for k in names[1:3]):return None
    if fields.get('method_kind') not in ('get','post','other'):return None
    if fields.get('content_type_kind') not in ('absent','event_stream','json','html','javascript','other'):return None
    out.update({k:fields[k] for k in names});out['extended_metadata_available']=True
    return out

if __name__ == '__main__':
    import sys, json
    result = collect(sys.stdin.buffer.read(2 * 1024 * 1024 + 1))
    print(json.dumps(result, sort_keys=True, allow_nan=False))

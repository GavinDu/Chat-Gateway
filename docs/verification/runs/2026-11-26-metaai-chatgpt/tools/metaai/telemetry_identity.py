import json,re,subprocess

def docker_image_identity():
    try:
        r = subprocess.run(['docker', 'inspect', 'chat-gateway', '--format', '{"image_id": {{json .Image}}, "source_sha": {{json (index .Config.Labels "org.opencontainers.image.revision")}}, "ports": {{json (index .NetworkSettings.Ports "8080/tcp")}}}'], capture_output=True, text=True, timeout=2, check=False)
        if r.returncode != 0 or len(r.stdout) > 4096:
            return {'available': False, 'reason': 'identity_unavailable'}
        d = json.loads(r.stdout)
        if not isinstance(d, dict):
            return {'available': False, 'reason': 'identity_unavailable'}
        i = d.get('image_id')
        s = d.get('source_sha')
        p = d.get('ports')
        if not isinstance(i, str) or not re.fullmatch('sha256:[0-9a-f]{64}', i):
            return {'available': False, 'reason': 'identity_unavailable'}
        if not isinstance(s, str) or not re.fullmatch('[0-9a-f]{40}', s):
            return {'available': False, 'reason': 'identity_unavailable'}
        if not isinstance(p, list) or not any((isinstance(x, dict) and x.get('HostIp') == '127.0.0.1' and (x.get('HostPort') == '8080') for x in p)):
            return {'available': False, 'reason': 'identity_unavailable'}
        return {'available': True, 'image_id': i, 'source_sha': s, 'loopback_8080_confirmed': True}
    except Exception:
        return {'available': False, 'reason': 'identity_unavailable'}

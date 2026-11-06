import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import time
import urllib.request

print('Test workspace:', os.getcwd(), flush=True)
def output(*args):
    return subprocess.check_output(args, text=True).strip()
assert output('git', 'branch', '--show-current') == 'main'
assert not output('git', 'status', '--porcelain')
commit = output('git', 'rev-parse', 'HEAD')
name = 'chat-gateway-pairing-mcp-e2e'
volume = name + '-key'
network = next(iter(json.loads(output('docker', 'inspect', 'mcp-sandbox'))[0]['NetworkSettings']['Networks']))
subprocess.run(['docker', 'volume', 'create', volume], check=True, stdout=subprocess.DEVNULL)
subprocess.run(['docker', 'run', '--rm', '-i', '--mount', f'type=volume,source={volume},target=/vault',
                'alpine:3.22', 'sh', '-eu', '-c',
                'umask 077; cat > /vault/pairing-admin.key; chown 65532:65532 /vault/pairing-admin.key'],
               input=secrets.token_hex(32), text=True, check=True)
args = ['docker', 'run', '--detach', '--name', name, '--network', network,
        '--publish', '127.0.0.1::8080', '--log-driver', 'none',
        '--mount', f'type=volume,source={volume},target=/run/secrets,readonly']
for path in ['data', 'sessions', 'browser', 'home']:
    args += ['--tmpfs', f'/app/{path}:rw,nosuid,nodev,noexec,uid=65532,gid=65532,mode=0700']
for value in ['HOME=/app/home', 'PORT=8080', 'TOKIO_WORKER_THREADS=2',
              'CHAT_GATEWAY_DOCKER_LOCAL_BIND=1', 'CHAT_GATEWAY_SESSION_STORE=vault',
              'CHAT_GATEWAY_VAULT_KEY=' + secrets.token_hex(32),
              'CHAT_GATEWAY_MEMORY_PATH=/app/data/conversations.sqlite3',
              'CHAT_GATEWAY_EXTENSION_ID=abcdefghijklmnopabcdefghijklmnop']:
    args += ['--env', value]
args += ['chat-gateway-rs:local', '--config', '/config.example.yaml']
subprocess.run(args, check=True, stdout=subprocess.DEVNULL)
binding = output('docker', 'port', name, '8080/tcp')
assert re.fullmatch(r'127\.0\.0\.1:[0-9]+', binding)
for attempt in range(30):
    try:
        with urllib.request.urlopen('http://' + binding + '/health', timeout=2) as response:
            assert response.status == 200
        break
    except OSError:
        if attempt == 29:
            raise
        time.sleep(1)
env = {**os.environ, 'CHAT_GATEWAY_CONTAINER': name, 'CHAT_GATEWAY_KEY_VOLUME': volume,
       'PORT': binding.rsplit(':', 1)[1]}
codes = []
for _ in range(2):
    print('Test workspace:', os.getcwd(), flush=True)
    result = subprocess.run(['bash', 'scripts/docker-run.sh', 'pairing-code'],
                            env=env, text=True, capture_output=True, check=True)
    assert re.fullmatch(r'[0-9]{8}\n', result.stdout)
    codes.append(result.stdout.strip())
print('PASS real main pairing-code command returned two eight-digit codes (values withheld)')
address = json.loads(output('docker', 'inspect', name))[0]['NetworkSettings']['Networks'][network]['IPAddress']
meta = {'commit': commit, 'address': address, 'old_code': codes[0], 'new_code': codes[1]}
path = Path('/Users/fangdu/projects/mcp-sandbox/chat-gateway-build-cache/release-link-report/pairing-test-network.json')
path.write_text(json.dumps(meta) + '\n')
path.chmod(0o600)

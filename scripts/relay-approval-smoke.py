#!/usr/bin/env python3
"""Accept the local approval exchange against a disposable release gateway."""
import json
import re
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

base = sys.argv[1].rstrip('/')
extension = 'chrome-extension://' + sys.argv[2]
assert urllib.parse.urlsplit(base).hostname in ('127.0.0.1', 'localhost', '::1')

def request(path, body=None, origin=extension, headers=None, form=False):
    values = {'Origin': origin} if origin else {}
    values.update(headers or {})
    if body is not None:
        values['Content-Type'] = 'application/x-www-form-urlencoded' if form else 'application/json'
        body = urllib.parse.urlencode(body).encode() if form else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=body, headers=values)
    try:
        response = urllib.request.urlopen(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        content = response.read().decode()
        return response.status, response.headers, content

status, _, content = request('/relay/v1/connect/request', {})
assert status == 200, 'could not initiate local approval'
capability = json.loads(content)
path = '/relay/approve/' + capability['request_id']
assert request('/relay/v1/connect/poll', capability)[0] == 202
assert request('/relay/v1/connect/poll', {**capability, 'poll_secret': '0' * 64})[0] == 410
assert request(path, origin=None)[0] == 403
assert request(path, origin=None, headers={'Sec-Fetch-Mode': 'cors', 'Sec-Fetch-Dest': 'empty'})[0] == 403
assert request(path, origin=None, headers={'Sec-Fetch-Mode': 'navigate', 'Sec-Fetch-Dest': 'iframe'})[0] == 403
status, headers, html = request(path, origin=None, headers={'Sec-Fetch-Mode': 'navigate', 'Sec-Fetch-Dest': 'document'})
assert status == 200
assert headers.get('Cache-Control') == 'no-store'
assert headers.get('Referrer-Policy') == 'same-origin'
assert "frame-ancestors 'none'" in headers['Content-Security-Policy']
csrf = re.search(r'name="csrf" value="([a-f0-9]{64})"', html).group(1)
assert request(path, {'csrf': csrf}, origin=None, form=True)[0] == 403
assert request(path, {'csrf': csrf}, origin='https://evil.invalid', form=True)[0] == 403
assert request(path, {'csrf': '0' * 64}, origin=base, form=True)[0] == 403
assert request(path, {'csrf': csrf}, origin=base, form=True)[0] == 200
assert request(path, {'csrf': csrf}, origin=base, form=True)[0] == 410
status, _, content = request('/relay/v1/connect/poll', capability)
assert status == 200
assert request('/relay/v1/connect/poll', capability)[0] == 410
token = json.loads(content)['token']
status, _, content = request('/relay/v1/status', headers={
    'Authorization': 'Bearer ' + token,
    'X-Relay-Timestamp': str(int(time.time())),
    'X-Relay-Nonce': secrets.token_hex(16),
})
assert status == 200 and len(json.loads(content)['providers']) == 8
print('Local approval smoke passed: navigation, Origin, CSRF, pre-approval polling, replay rejection and authenticated provider status.')

status, _, content = request('/relay/v1/status', {}, headers={
    'Authorization': 'Bearer ' + token,
    'X-Relay-Timestamp': str(int(time.time())),
    'X-Relay-Nonce': secrets.token_hex(16),
})
assert status == 200 and len(json.loads(content)['providers']) == 8
print('Origin-bound status POST and legacy GET both passed.')

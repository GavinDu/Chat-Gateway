const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const sources = {
  chrome: fs.readFileSync('extension/credential_probe.js', 'utf8') + '\nprobeActiveChatgptAccount()',
  operator: fs.readFileSync('scripts/provider_credential_probe.js', 'utf8'),
  obscura: fs.readFileSync('src/providers/chatgpt/session_readiness.js', 'utf8')
    .replaceAll('__ATTEMPT__', '"synthetic-freshness"')
    .replaceAll('__ORIGIN__', '"https://chatgpt.com"')
    .replaceAll('__TIMEOUT_MS__', '2500')
};
const jwt = payload => 'e30.' + Buffer.from(JSON.stringify(payload)).toString('base64url') + '.synthetic-signature';
const session = accessToken => ({user:{email:'private@example.test'}, accessToken,
  expires:new Date(Date.now() + 60_000).toISOString()});

async function verdict(context, body, status = 200) {
  const realm = {
    Date, atob, AbortController, setTimeout, clearTimeout,
    location:{protocol:'https:', hostname:'chatgpt.com', port:'',
      origin:'https://chatgpt.com', href:'https://chatgpt.com/'},
    fetch:async () => ({status, ok:status===200, redirected:false,
      url:'https://chatgpt.com/api/auth/session', json:async () => body})
  };
  vm.createContext(realm);
  const initial = await vm.runInContext(sources[context], realm);
  await new Promise(resolve => setImmediate(resolve));
  const result = context === 'obscura'
    ? vm.runInContext('({state:__chatGatewayChatgptSessionReadiness.state,http:__chatGatewayChatgptSessionReadiness.http})', realm)
    : initial;
  const authentication = context === 'obscura'
    ? ({verified:'verified',needs_login:'unauthenticated',unavailable:'unknown'})[result.state]
    : result.authentication;
  const output = JSON.stringify(result);
  assert(!output.includes('private@example.test'));
  assert(!output.includes('synthetic-signature'));
  assert(!output.includes('RefreshAccessTokenError'));
  return authentication;
}

test('HTTP 200 with a failed refresh never verifies any ChatGPT account context', async () => {
  for (const context of Object.keys(sources)) {
    for (const error of ['RefreshAccessTokenError', {private:'upstream details'}]) {
      assert.equal(await verdict(context, {...session(jwt({exp:Date.now()/1000+60})),error}), 'unknown', context);
    }
  }
});

test('expired or malformed access-token evidence cannot borrow the outer session expiry', async () => {
  for (const context of Object.keys(sources)) {
    for (const token of [jwt({exp:1}), jwt({exp:0}), jwt({exp:'2100000000'}),
      jwt({exp:null}), 'e30.not-json.synthetic-signature']) {
      assert.equal(await verdict(context, session(token)), 'unknown', context);
    }
  }
});

test('fresh JWT and existing opaque tokens remain supported without using claims as account proof', async () => {
  for (const context of Object.keys(sources)) {
    for (const token of [jwt({exp:Math.floor(Date.now()/1000)+60}), 'synthetic-opaque-token']) {
      assert.equal(await verdict(context, session(token)), 'verified', context);
      assert.equal(await verdict(context, {...session(token),user:{}}), 'unknown', context);
      assert.equal(await verdict(context, {...session(token),expires:'2000-01-01'}), 'unknown', context);
      assert.equal(await verdict(context, session(token), 401), 'unauthenticated', context);
    }
  }
});

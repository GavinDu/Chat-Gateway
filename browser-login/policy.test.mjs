import { test } from 'node:test';
import assert from 'node:assert/strict';
import { allowed, allowedDocument, safeAddress, dimensions, providerDomains, vaultDomains, domainMatches } from './policy.mjs';

test('every supported provider has a bounded navigation policy', () => {
  assert.equal(Object.keys(providerDomains).length, 8);
  for (const [provider, hosts] of Object.entries(providerDomains)) {
    assert(allowed(provider, 'https://' + hosts[0] + '/login'));
    for (const bad of ['http://chatgpt.com', 'https://127.0.0.1',
      'https://localhost', 'file:///tmp/a', 'javascript:alert(1)',
      'https://chatgpt.com.evil.test', 'https://evil.test',
      'https://user:pass@' + hosts[0], 'https://' + hosts[0] + ':8443',
      'https://' + hosts[0] + '.']) {
      assert.equal(allowed(provider, bad), false);
    }
  }
});
test('providers cannot navigate into each other', () => {
  assert(!allowed('chatgpt', 'https://claude.ai'));
  assert(!allowed('claude', 'https://chatgpt.com'));
  assert(allowed('chatgpt', 'https://accounts.google.com/signin'));
  assert(!allowed('unknown', 'https://accounts.google.com'));
});
test('bounded integral resize rejects nonfinite and oversized dimensions', () => {
  assert.deepEqual(dimensions(1000, 700), { width: 1000, height: 700 });
  for (const args of [[0, 700], [1000, 0], [1921, 700], [1000, 1081],
    [NaN, 700], [1000.1, 700]]) assert.throws(() => dimensions(...args));
});
test('approved SSO navigation does not authorize saving SSO cookies', () => {
  assert(allowed('chatgpt', 'https://accounts.google.com'));
  assert(!domainMatches('accounts.google.com', vaultDomains.chatgpt));
  assert(!domainMatches('auth0.com', vaultDomains.chatgpt));
  assert(domainMatches('chatgpt.com', vaultDomains.chatgpt));
});
test('CAPTCHA documents are narrowly allowed in child frames, never top-level', () => {
  for (const url of ['https://challenges.cloudflare.com/widget',
    'https://newassets.hcaptcha.com/captcha/v1/test',
    'https://www.google.com/recaptcha/api2/anchor',
    'https://www.recaptcha.net/recaptcha/api2/anchor']) {
    assert(allowedDocument('chatgpt', url, false));
    assert(!allowedDocument('chatgpt', url, true));
  }
  for (const url of ['http://challenges.cloudflare.com/widget',
    'https://challenges.cloudflare.com.evil.test', 'https://www.google.com/search',
    'https://user:password@challenges.cloudflare.com', 'https://127.0.0.1/private'])
    assert(!allowedDocument('chatgpt', url, false));
});
test('visible browser addresses exclude query and fragment credentials', () => {
  assert.equal(safeAddress('https://chatgpt.com/callback?code=dummy#state'), 'https://chatgpt.com/callback');
  assert.equal(safeAddress('file:///tmp/private'), 'about:blank');
});

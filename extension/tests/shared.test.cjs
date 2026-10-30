const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");

function loadShared(cookieFixtures = [], hasAllPermissions = true, storageValues = {}) {
  let cookieReads = 0;
  const fetchCalls = [];
  const context = {
    URL,
    Headers,
    crypto: webcrypto,
    fetch: async (url, options) => { fetchCalls.push({url, options}); return {ok:true, status:200}; },
    chrome: {
      cookies: {
        // Return the full fixture set to simulate a permissive/misbehaving
        // browser API; providerCookieSnapshot must still enforce its own scope.
        getAll: async () => { cookieReads += 1; return cookieFixtures; }
      },
      permissions: {
        contains: async () => hasAllPermissions
      },
      storage: { local: { get: async () => storageValues } }
    }
  };
  vm.createContext(context);
  const source = fs.readFileSync(require.resolve("../shared.js"), "utf8");
  vm.runInContext(source + "\nglobalThis.__testApi = { normalizeGateway, providerCookieSnapshot, relayFetch, syncProvider, PROVIDERS };", context);
  return { ...context.__testApi, getCookieReads: () => cookieReads, fetchCalls };
}

test("gateway normalization accepts only local HTTP origins", () => {
  const api = loadShared();
  assert.equal(api.normalizeGateway("http://127.0.0.1:8080/path?secret=ignored"), "http://127.0.0.1:8080");
  assert.equal(api.normalizeGateway("http://localhost:3000/"), "http://localhost:3000");
  assert.equal(api.normalizeGateway("http://[::1]:8080"), "http://[::1]:8080");
});

test("gateway normalization rejects remote, HTTPS, and lookalike hosts", () => {
  const api = loadShared();
  for (const value of [
    "https://localhost:8080",
    "http://gateway.example",
    "http://localhost.attacker.example",
    "http://127.0.0.2:8080",
    "file:///tmp/gateway"
  ]) {
    assert.throws(() => api.normalizeGateway(value), undefined, value);
  }
});

test("relayFetch rejects a remote gateway value restored from extension storage", async () => {
  const api = loadShared([], true, {
    gateway: "http://attacker.example",
    token: "relay-secret"
  });

  await assert.rejects(
    () => api.relayFetch("/relay/v1/session/chatgpt", {method:"POST", body:"{}"}),
    /Gateway URL must be local HTTP/
  );
  assert.equal(api.fetchCalls.length, 0, "no request or relay token may reach a remote host");
});

test("provider cookie snapshot excludes cookies outside that provider's domain roots", async () => {
  const fixtures = [
    { name: "session", value: "keep", domain: ".chatgpt.com", path: "/", secure: true, httpOnly: true },
    { name: "auth", value: "keep", domain: "accounts.openai.com", path: "/", secure: true, httpOnly: true },
    { name: "mail", value: "drop", domain: ".gmail.com", path: "/", secure: true, httpOnly: true },
    { name: "spoof", value: "drop", domain: ".openai.com.attacker.example", path: "/", secure: true, httpOnly: true }
  ];
  const api = loadShared(fixtures);
  const snapshot = await api.providerCookieSnapshot("chatgpt");
  assert.deepEqual(JSON.parse(JSON.stringify(snapshot.cookies.map(cookie => cookie.name).sort())), ["auth", "session"]);
  assert.equal(snapshot.local_storage && Object.keys(snapshot.local_storage).length, 0);
});

test("partial provider host permissions reject cookie snapshot before reading cookies", async () => {
  const api = loadShared([], false);
  await assert.rejects(
    () => api.providerCookieSnapshot("chatgpt"),
    /Missing provider host permissions/
  );
  assert.equal(api.getCookieReads(), 0, "cookie data must not be read when permissions are incomplete");
});

test("unsupported provider cannot be used to collect a cookie snapshot", async () => {
  const api = loadShared();
  await assert.rejects(() => api.providerCookieSnapshot("unknown"));
});


test("empty cookie snapshot never overwrites an existing linked gateway session", async () => {
  const api = loadShared([], true, {
    gateway: "http://127.0.0.1:8080",
    token: "relay-secret",
    linkedProviders: ["chatgpt"]
  });

  await api.syncProvider("chatgpt");

  assert.equal(api.fetchCalls.length, 0, "an empty browser snapshot must not revoke a valid gateway session");
});

test("linked provider sync sends its cookie snapshot only to the configured local gateway", async () => {
  const api = loadShared([
    { name: "session", value: "session-secret", domain: ".chatgpt.com", path: "/", secure: true, httpOnly: true }
  ], true, {
    gateway: "http://127.0.0.1:8080",
    token: "relay-secret",
    linkedProviders: ["chatgpt"]
  });

  await api.syncProvider("chatgpt");

  assert.equal(api.fetchCalls.length, 1);
  assert.equal(api.fetchCalls[0].url, "http://127.0.0.1:8080/relay/v1/session/chatgpt");
  assert.equal(api.fetchCalls[0].options.headers.get("Authorization"), "Bearer relay-secret");
  const body = JSON.parse(api.fetchCalls[0].options.body);
  assert.equal(body.cookies.length, 1);
  assert.equal(body.cookies[0].value, "session-secret");
  assert.equal(api.fetchCalls[0].options.cache, "no-store");
});

test("unlinked provider sync does not read or transmit cookies", async () => {
  const api = loadShared([
    { name: "session", value: "session-secret", domain: ".chatgpt.com", path: "/", secure: true, httpOnly: true }
  ], true, {
    gateway: "http://127.0.0.1:8080",
    token: "relay-secret",
    linkedProviders: []
  });

  await api.syncProvider("chatgpt");

  assert.equal(api.getCookieReads(), 0);
  assert.equal(api.fetchCalls.length, 0);
});

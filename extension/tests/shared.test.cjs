const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");

function loadShared(cookieFixtures = []) {
  const context = {
    URL,
    Headers,
    crypto: webcrypto,
    fetch: async () => { throw new Error("fetch should not be called in these tests"); },
    chrome: {
      cookies: {
        // Return the full fixture set to simulate a permissive/misbehaving
        // browser API; providerCookieSnapshot must still enforce its own scope.
        getAll: async () => cookieFixtures
      },
      permissions: {
        contains: async () => true
      },
      storage: { local: { get: async () => ({}) } }
    }
  };
  vm.createContext(context);
  const source = fs.readFileSync(require.resolve("../shared.js"), "utf8");
  vm.runInContext(source + "\nglobalThis.__testApi = { normalizeGateway, providerCookieSnapshot, PROVIDERS };", context);
  return context.__testApi;
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

test("provider cookie snapshot fails closed when host permissions are incomplete", async () => {
  const api = loadShared([], false);
  await assert.rejects(
    () => api.providerCookieSnapshot("chatgpt"),
    /Missing provider host permissions/
  );
  assert.equal(api.cookieReads(), 0, "permission rejection must happen before reading cookies");
  assert.ok(api.requestedOrigins().includes("https://chatgpt.com/*"));
  assert.ok(api.requestedOrigins().includes("https://*.chatgpt.com/*"));
});

test("unsupported provider cannot be used to collect a cookie snapshot", async () => {
  const api = loadShared();
  await assert.rejects(() => api.providerCookieSnapshot("unknown"));
});

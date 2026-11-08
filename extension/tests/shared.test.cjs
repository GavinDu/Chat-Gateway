const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");

function loadShared(cookieFixtures = [], hasAllPermissions = true, storageValues = {}, hooks = {}) {
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
        getAll: async () => {
          cookieReads += 1;
          if (hooks.onCookieRead) await hooks.onCookieRead(storageValues);
          return cookieFixtures;
        }
      },
      permissions: {
        contains: async ({origins}) => {
          if (hooks.onPermissionCheck) await hooks.onPermissionCheck(origins, storageValues);
          return hooks.hasPermissions ? hooks.hasPermissions(origins, storageValues) : hasAllPermissions;
        }
      },
      storage: { local: { get: async () => storageValues } }
    }
  };
  vm.createContext(context);
  const source = fs.readFileSync(require.resolve("../shared.js"), "utf8");
  vm.runInContext(source + "\nglobalThis.__testApi = { normalizeGateway, providerCookieSnapshot, relayFetch, syncProvider, PROVIDERS };", context);
  return { ...context.__testApi, getCookieReads: () => cookieReads, fetchCalls };
}

test("gateway normalization accepts local HTTP and explicit HTTPS origins", () => {
  const api = loadShared();
  assert.equal(api.normalizeGateway("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
  assert.equal(api.normalizeGateway("http://localhost:3000/"), "http://localhost:3000");
  assert.equal(api.normalizeGateway("http://[::1]:8080"), "http://[::1]:8080");
  assert.equal(api.normalizeGateway("https://gateway.example.com"), "https://gateway.example.com");
});

test("gateway normalization rejects remote HTTP and malformed HTTPS origins", () => {
  const api = loadShared();
  for (const value of [
    "http://gateway.example",
    "https://gateway.example.com/path",
    "https://user:pass@gateway.example.com",
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
    /Gateway URL must be local HTTP or an explicitly configured HTTPS remote origin/
  );
  assert.equal(api.fetchCalls.length, 0, "no request or relay token may reach a remote host");
});



test("relayFetch refuses to send the install token when exact HTTPS host permission is absent", async () => {
  const api = loadShared([], false, {
    gateway: "https://attacker.example",
    token: "relay-secret"
  });
  await assert.rejects(
    () => api.relayFetch("/relay/v1/status"),
    /Gateway host permission is missing/
  );
  assert.equal(api.fetchCalls.length, 0);
});

test("relayFetch supports an HTTPS gateway without weakening the HTTP loopback rule", async () => {
  const api = loadShared([], true, {
    gateway: "https://gateway.example.com",
    token: "relay-secret"
  });
  await api.relayFetch("/relay/v1/status");
  assert.equal(api.fetchCalls.length, 1);
  assert.equal(api.fetchCalls[0].url, "https://gateway.example.com/relay/v1/status");
  assert.equal(api.fetchCalls[0].options.headers.get("Authorization"), "Bearer relay-secret");
  assert.equal(api.fetchCalls[0].options.cache, "no-store");
  assert.equal(api.fetchCalls[0].options.redirect, "error");
});

test("a caller cannot override the no-redirect policy on credentialed relay requests", async () => {
  const api = loadShared([], true, {
    gateway: "https://gateway.example.com",
    token: "relay-secret"
  });
  await api.relayFetch("/relay/v1/status", {method:"POST", body:"{}", redirect:"follow"});
  assert.equal(api.fetchCalls.length, 1);
  assert.equal(api.fetchCalls[0].options.redirect, "error");
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

test("Manus relay only collects consented manus.im cookies", async () => {
  const cookies = [
    {name:"session",value:"manus-session",domain:".manus.im",path:"/",secure:true,httpOnly:true},
    {name:"api",value:"manus-api",domain:"api.manus.im",path:"/",secure:true,httpOnly:true},
    {name:"spoof",value:"evil",domain:".manus.im.evil.example",path:"/",secure:true,httpOnly:true},
    {name:"other",value:"cross-provider",domain:".chatgpt.com",path:"/",secure:true,httpOnly:true}
  ];
  const api = loadShared(cookies);
  assert.deepEqual(JSON.parse(JSON.stringify(api.PROVIDERS.manus)), ["manus.im"]);
  const snapshot = await api.providerCookieSnapshot("manus");
  assert.deepEqual(snapshot.cookies.map(cookie => cookie.name).sort(), ["api", "session"]);
  assert.equal(snapshot.local_storage && Object.keys(snapshot.local_storage).length, 0);
});

test("Manus snapshots fail closed without exact host permission", async () => {
  const api = loadShared([], false);
  await assert.rejects(
    () => api.providerCookieSnapshot("manus"),
    /Missing provider host permissions/
  );
  assert.equal(api.getCookieReads(), 0);
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
  assert.equal(api.fetchCalls[0].options.redirect, "error");
});

test("two linked providers sync only their own cookie domains", async () => {
  const api = loadShared([
    { name: "chatgpt-session", value: "chatgpt-secret", domain: ".chatgpt.com", path: "/", secure: true, httpOnly: true },
    { name: "claude-session", value: "claude-secret", domain: ".claude.ai", path: "/", secure: true, httpOnly: true },
    { name: "lookalike", value: "attacker-secret", domain: ".evilchatgpt.com", path: "/", secure: true, httpOnly: true }
  ], true, {
    gateway: "http://127.0.0.1:8080",
    token: "relay-secret",
    linkedProviders: ["chatgpt", "claude"]
  });

  await api.syncProvider("chatgpt");
  await api.syncProvider("claude");

  assert.equal(api.fetchCalls.length, 2);
  const chatgpt = JSON.parse(api.fetchCalls[0].options.body);
  const claude = JSON.parse(api.fetchCalls[1].options.body);
  assert.equal(api.fetchCalls[0].url, "http://127.0.0.1:8080/relay/v1/session/chatgpt");
  assert.equal(api.fetchCalls[1].url, "http://127.0.0.1:8080/relay/v1/session/claude");
  assert.equal(chatgpt.cookies.length, 1);
  assert.equal(chatgpt.cookies[0].value, "chatgpt-secret");
  assert.equal(claude.cookies.length, 1);
  assert.equal(claude.cookies[0].value, "claude-secret");
  assert.ok(!JSON.stringify(chatgpt).includes("claude-secret"));
  assert.ok(!JSON.stringify(claude).includes("chatgpt-secret"));
  assert.ok(!JSON.stringify(chatgpt).includes("attacker-secret"));
  assert.ok(!JSON.stringify(claude).includes("attacker-secret"));
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

test("provider sync never uploads old cookies to a newly paired gateway", async () => {
  const storage = {
    gateway: "http://127.0.0.1:8080",
    token: "old-install-token",
    linkedProviders: ["chatgpt"]
  };
  const api = loadShared([
    {name:"session",value:"synthetic-cookie",domain:".chatgpt.com",path:"/",secure:true}
  ], true, storage, {
    onCookieRead: () => {
      storage.gateway = "https://new-gateway.example";
      storage.token = "new-install-token";
      storage.linkedProviders = ["chatgpt"];
    }
  });
  await api.syncProvider("chatgpt");
  assert.equal(api.fetchCalls.length, 0,
    "a pending old-pairing snapshot must not be sent to a newly paired gateway");
});

test("provider unlink during a cookie read prevents stale background uploads", async () => {
  const storage = {
    gateway: "http://127.0.0.1:8080",
    token: "old-install-token",
    linkedProviders: ["chatgpt"]
  };
  const api = loadShared([
    {name:"session",value:"synthetic-cookie",domain:".chatgpt.com",path:"/",secure:true}
  ], true, storage, {
    onCookieRead: () => { storage.linkedProviders = []; }
  });
  await api.syncProvider("chatgpt");
  assert.equal(api.fetchCalls.length, 0);
});

test("late consent revocation during relay permission check blocks cookie upload", async () => {
  const storage = {
    gateway: "http://127.0.0.1:8080",
    token: "old-install-token",
    linkedProviders: ["chatgpt"]
  };
  const api = loadShared([
    {name:"session",value:"synthetic-cookie",domain:".chatgpt.com",path:"/",secure:true}
  ], true, storage, {
    onPermissionCheck: origins => {
      if (origins.some(origin => origin.startsWith("http://127.0.0.1"))) {
        storage.linkedProviders = [];
      }
    }
  });
  await assert.rejects(() => api.syncProvider("chatgpt"), /consent or gateway connection changed/);
  assert.equal(api.fetchCalls.length, 0);
});

test("revoked provider permission during cookie collection prevents any snapshot", async () => {
  const state = {permissionGranted:true};
  const api = loadShared([
    {name:"session",value:"synthetic-only",domain:".chatgpt.com",path:"/",secure:true}
  ], true, state, {
    onCookieRead: () => { state.permissionGranted = false; },
    hasPermissions: origins => origins.every(origin => origin.startsWith("https://"))
      ? state.permissionGranted : true
  });
  await assert.rejects(
    () => api.providerCookieSnapshot("chatgpt"),
    /permissions were revoked during cookie collection/
  );
  assert.ok(api.getCookieReads() > 0);
  assert.equal(api.fetchCalls.length, 0);
});

test("revoked provider permission at upload boundary blocks previously captured cookies", async () => {
  let providerChecks = 0;
  const state = {
    gateway:"http://127.0.0.1:8080",
    token:"relay-secret",
    linkedProviders:["chatgpt"]
  };
  const api = loadShared([
    {name:"session",value:"synthetic-only",domain:".chatgpt.com",path:"/",secure:true}
  ], true, state, {
    hasPermissions: origins => {
      if (origins.every(origin => origin.startsWith("https://"))) {
        providerChecks += 1;
        return providerChecks < 3;
      }
      return true;
    }
  });
  await assert.rejects(
    () => api.syncProvider("chatgpt"),
    /permissions were revoked before session sync/
  );
  assert.equal(providerChecks, 3);
  assert.equal(api.fetchCalls.length, 0, "revoked consent must block credential uploads");
});

test("token-bound logout can run after provider consent is removed", async () => {
  const state = {
    gateway:"http://127.0.0.1:8080",
    token:"install-old",
    linkedProviders:[]
  };
  const api = loadShared([], true, state);
  await api.relayFetch("/relay/v1/session/chatgpt", {method:"DELETE",body:"{}"}, {
    gateway:state.gateway, token:state.token
  });
  assert.equal(api.fetchCalls.length, 1);
  assert.equal(api.fetchCalls[0].options.method, "DELETE");
});

test("token-bound logout never follows a newly paired gateway", async () => {
  const state = {
    gateway:"http://127.0.0.1:8080",
    token:"install-old",
    linkedProviders:[]
  };
  const api = loadShared([], true, state, {
    onPermissionCheck: () => {
      state.gateway = "https://new-gateway.example";
      state.token = "install-new";
    }
  });
  await assert.rejects(
    () => api.relayFetch("/relay/v1/session/chatgpt", {method:"DELETE",body:"{}"}, {
      gateway:"http://127.0.0.1:8080",token:"install-old"
    }),
    /Provider consent or gateway connection changed during sync/
  );
  assert.equal(api.fetchCalls.length, 0);
});

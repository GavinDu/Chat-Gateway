const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { webcrypto } = require("node:crypto");
const { TextEncoder } = require("node:util");

function loadShared(cookieFixtures = [], hasAllPermissions = true, storageValues = {}, hooks = {}) {
  let cookieReads = 0;
  const fetchCalls = [];
  const context = {
    URL,
    Headers,
    crypto: webcrypto,
    TextEncoder,
    AbortController,
    setTimeout: hooks.setTimeout || setTimeout,
    clearTimeout: hooks.clearTimeout || clearTimeout,
    fetch: async (url, options) => {
      fetchCalls.push({url, options});
      if (hooks.onFetch) return hooks.onFetch(url, options);
      return hooks.fetchResponse || {ok:true, status:200};
    },
    chrome: {
      cookies: {
        // Return the full fixture set to simulate a permissive/misbehaving
        // browser API; providerCookieSnapshot must still enforce its own scope.
        getAll: async ({domain}) => {
          cookieReads += 1;
          if (hooks.onCookieRead) await hooks.onCookieRead(storageValues, domain);
          return cookieFixtures;
        }
      },
      permissions: {
        contains: async ({origins}) => {
          if (hooks.onPermissionCheck) await hooks.onPermissionCheck(origins, storageValues);
          return hooks.hasPermissions ? hooks.hasPermissions(origins, storageValues) : hasAllPermissions;
        }
      },
      storage: { local: {
        get: async () => storageValues,
        set: async update => Object.assign(storageValues, update)
      } }
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

test("relay fetch aborts a stalled credential import without replaying it", async () => {
  let timeout;
  let cleared = false;
  const api = loadShared([], true, {
    gateway: "http://127.0.0.1:8080",
    token: "relay-secret"
  }, {
    setTimeout(callback, delay) {
      timeout = {callback, delay};
      return 42;
    },
    clearTimeout(id) {
      assert.equal(id, 42);
      cleared = true;
    },
    onFetch(_url, options) {
      return new Promise((_resolve, reject) => {
        options.signal.addEventListener("abort", () => reject(new Error("aborted")), {once:true});
      });
    }
  });

  const request = api.relayFetch("/relay/v1/session/chatgpt", {method:"POST", body:"{}"});
  for (let i = 0; i < 8 && !timeout; i++) await new Promise(resolve => setImmediate(resolve));
  assert.equal(timeout?.delay, 60_000);
  assert.equal(api.fetchCalls.length, 1);
  timeout.callback();
  await assert.rejects(request, /session may have changed\. Check provider status before retrying/);
  assert.equal(api.fetchCalls.length, 1, "ambiguous writes must not be replayed");
  assert.equal(api.fetchCalls[0].options.signal.aborted, true);
  assert.equal(cleared, true, "the timeout must be cleared after abort");
});

test("provider cookie reads for separate domains run concurrently after permission consent", async () => {
  const pendingDomains = [];
  let releaseRead;
  const readGate = new Promise(resolve => { releaseRead = resolve; });
  const api = loadShared([
    {name:"one",value:"safe",domain:".chatgpt.com",path:"/",secure:true,httpOnly:true},
    {name:"two",value:"safe",domain:".openai.com",path:"/",secure:true,httpOnly:true}
  ], true, {}, {
    onCookieRead: async (_storage, domain) => {
      pendingDomains.push(domain);
      await readGate;
    }
  });
  const snapshot = api.providerCookieSnapshot("chatgpt");
  try {
    for (let i = 0; i < 8 && pendingDomains.length < 2; i++) {
      await new Promise(resolve => setImmediate(resolve));
    }
    assert.deepEqual(pendingDomains, ["chatgpt.com", "openai.com"],
      "both consented domain reads should start without waiting for the other");
  } finally {
    releaseRead();
  }
  const result = await snapshot;
  assert.deepEqual(
    JSON.parse(JSON.stringify(result.cookies.map(cookie => cookie.name).sort())),
    ["one", "two"]
  );
});

test("failed parallel cookie read never produces a partial snapshot", async () => {
  const api = loadShared([
    {name:"one",value:"safe",domain:".chatgpt.com",path:"/",secure:true,httpOnly:true}
  ], true, {
    gateway:"http://127.0.0.1:8080",token:"relay-secret",linkedProviders:["chatgpt"]
  }, {
    onCookieRead: async (_storage, domain) => {
      if (domain === "openai.com") throw new Error("synthetic cookie store unavailable");
    }
  });
  await assert.rejects(() => api.syncProvider("chatgpt"), /synthetic cookie store unavailable/);
  assert.equal(api.fetchCalls.length, 0, "a partial snapshot must not reach the gateway");
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

test("gemini snapshot drops host-only root duplicates but keeps the domain scope", async () => {
  const fixtures = [
    { name: "SID", value: "host-root", domain: "google.com", path: "/", secure: true, httpOnly: true },
    { name: "SID", value: "domain-root", domain: ".google.com", path: "/", secure: true, httpOnly: true },
    { name: "SID", value: "host-login", domain: "gemini.google.com", path: "/", secure: true, httpOnly: true },
    { name: "NID", value: "host-subdomain", domain: "accounts.google.com", path: "/", secure: true, httpOnly: true }
  ];
  const api = loadShared(fixtures);
  const snapshot = await api.providerCookieSnapshot("gemini");
  const cookies = snapshot.cookies.map(cookie => [cookie.domain, cookie.value]);
  assert.deepEqual(JSON.parse(JSON.stringify(cookies)), [
    [".google.com", "domain-root"],
    ["gemini.google.com", "host-login"],
    ["accounts.google.com", "host-subdomain"]
  ]);
});

test("chatgpt snapshot keeps host-only login-host and auth-subdomain cookies", async () => {
  const fixtures = [
    { name: "session", value: "root-host", domain: "chatgpt.com", path: "/", secure: true, httpOnly: true },
    { name: "session", value: "root-domain", domain: ".chatgpt.com", path: "/", secure: true, httpOnly: true },
    { name: "auth", value: "subdomain-host", domain: "auth.openai.com", path: "/", secure: true, httpOnly: true }
  ];
  const api = loadShared(fixtures);
  const snapshot = await api.providerCookieSnapshot("chatgpt");
  const cookies = snapshot.cookies.map(cookie => [cookie.domain, cookie.value]);
  assert.deepEqual(JSON.parse(JSON.stringify(cookies)), [
    ["chatgpt.com", "root-host"],
    [".chatgpt.com", "root-domain"],
    ["auth.openai.com", "subdomain-host"]
  ]);
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
  assert.deepEqual(JSON.parse(JSON.stringify(snapshot.cookies.map(cookie => cookie.name).sort())), ["api", "session"]);
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

  const status = await api.syncProvider("chatgpt");
  assert.equal(status, "no_cookies", "silent skip is not a successful upload");

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

  const status = await api.syncProvider("chatgpt");
  assert.equal(status, "uploaded");

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


test("conflicting cookie scopes report preserved session and dedicated login guidance", async () => {
  const api=loadShared([{name:"fixture",value:"dummy",domain:".google.com",path:"/",secure:true,httpOnly:true}],true,
    {gateway:"http://127.0.0.1:8080",token:"fixture-token",linkedProviders:["gemini"]},
    {fetchResponse:{ok:false,status:422,json:async()=>({error:{code:"cookie_scope_conflict",message:"private upstream data"}})}});
  await assert.rejects(()=>api.syncProvider("gemini"), e=> e.message.includes("saved session was kept") && e.message.includes("dedicated login browser") && !e.message.includes("private upstream data"));
  assert.equal(api.fetchCalls.length,1);
});

test("unknown unprocessable-session errors retain generic guidance without forwarding text", async () => {
  const api=loadShared([{name:"fixture",value:"dummy",domain:".google.com",path:"/",secure:true,httpOnly:true}],true,
    {gateway:"http://127.0.0.1:8080",token:"fixture-token",linkedProviders:["gemini"]},
    {fetchResponse:{ok:false,status:422,json:async()=>({error:{code:"other",message:"private upstream data"}})}});
  await assert.rejects(()=>api.syncProvider("gemini"), e=> e.message.includes("Gateway rejected session update (422)") && !e.message.includes("private upstream data"));
});


test("typed missing-cookie import error gives login guidance without leaking gateway response", async () => {
  const api = loadShared([{name:"fixture",value:"dummy",domain:".google.com",path:"/",secure:true,httpOnly:true}], true,
    {gateway:"http://127.0.0.1:8080",token:"fixture-token",linkedProviders:["gemini"]},
    {fetchResponse:{ok:false,status:400,json:async()=>({error:{code:"session_missing_credentials",message:"secret cookie value"}})}});
  await assert.rejects(
    () => api.syncProvider("gemini"),
    error => error.message.includes("Sign in to the provider") && !error.message.includes("secret cookie value")
  );
  assert.equal(api.fetchCalls.length, 1);
});

test("Gemini relay retains Chrome SameSite and expiry while leaving session cookies session-only", async () => {
  const api = loadShared([
    {
      name: "persistent", value: "synthetic", domain: ".google.com", path: "/",
      secure: true, httpOnly: true, session: false, expirationDate: 2000000000.5,
      sameSite: "no_restriction"
    },
    {
      name: "session", value: "synthetic", domain: "gemini.google.com", path: "/",
      secure: true, httpOnly: true, session: true, expirationDate: 2000000000,
      sameSite: "strict"
    },
    {
      name: "unspecified", value: "synthetic", domain: ".googleusercontent.com", path: "/",
      secure: true, httpOnly: false, session: true, sameSite: "unspecified"
    }
  ]);
  const snapshot = await api.providerCookieSnapshot("gemini");
  const cookies = Object.fromEntries(JSON.parse(JSON.stringify(snapshot.cookies)).map(cookie => [cookie.name, cookie]));
  assert.equal(cookies.persistent.same_site, "None");
  assert.equal(cookies.persistent.expires, 2000000000.5);
  assert.equal(cookies.session.same_site, "Strict");
  assert.equal(Object.hasOwn(cookies.session, "expires"), false);
  assert.equal(Object.hasOwn(cookies.unspecified, "same_site"), false);
});

test("invalid browser cookie attributes fail the complete relay upload before persistence", async () => {
  for (const invalid of [
    {sameSite: "unsupported"},
    {session: false, expirationDate: Number.NaN},
    {session: false, expirationDate: -2}
  ]) {
    const api = loadShared([
      {name: "safe", value: "fixture", domain: ".google.com", path: "/", secure: true},
      {name: "invalid", value: "fixture", domain: "gemini.google.com",
       path: "/", secure: true, ...invalid}
    ], true, {
      gateway: "http://127.0.0.1:8080", token: "fixture-token", linkedProviders: ["gemini"]
    });
    await assert.rejects(() => api.syncProvider("gemini"), /cookie (SameSite|expiry)/);
    assert.equal(api.fetchCalls.length, 0, "never upload a partial credential snapshot");
  }
});

test("automatic sync skips unchanged snapshots but manual sync always refreshes", async () => {
  const storage = {gateway:"http://127.0.0.1:8080",token:"install-1",linkedProviders:["chatgpt"]};
  const fixtures = [{name:"session",value:"first",domain:".chatgpt.com",path:"/",secure:true,httpOnly:true}];
  const api = loadShared(fixtures, true, storage);
  assert.equal(await api.syncProvider("chatgpt"), "uploaded");
  assert.equal(await api.syncProvider("chatgpt"), "unchanged");
  assert.equal(api.fetchCalls.length, 1);
  assert.match(storage.relaySnapshotVersions.chatgpt, /^[a-f0-9]{64}$/);
  assert.ok(!JSON.stringify(storage.relaySnapshotVersions).includes("first"));
  fixtures[0].value = "rotated";
  assert.equal(await api.syncProvider("chatgpt"), "uploaded");
  assert.equal(await api.syncProvider("chatgpt", {force:true}), "uploaded");
  assert.equal(api.fetchCalls.length, 3);
  storage.token = "install-2";
  assert.equal(await api.syncProvider("chatgpt"), "uploaded");
  assert.equal(api.fetchCalls.length, 4, "new installation must not inherit a previous sync digest");
});

test("failed imports do not mark a snapshot as synchronized", async () => {
  const storage = {gateway:"http://127.0.0.1:8080",token:"install",linkedProviders:["chatgpt"]};
  const api = loadShared([{name:"session",value:"synthetic",domain:".chatgpt.com",path:"/"}],
    true, storage, {fetchResponse:{ok:false,status:504}});
  await assert.rejects(() => api.syncProvider("chatgpt"), /Gateway rejected session update/);
  assert.equal(storage.relaySnapshotVersions, undefined);
  await assert.rejects(() => api.syncProvider("chatgpt"), /Gateway rejected session update/);
  assert.equal(api.fetchCalls.length, 2);
});

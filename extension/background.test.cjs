const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadBackground(linkedProviders, failingProviders = []) {
  const calls = [];
  const timers = new Map();
  const clearedTimers = new Set();
  let nextTimerId = 0;
  const listeners = {
    installed: [],
    startup: [],
    cookieChanged: [],
    message: [],
  };
  const sandbox = {
    Map,
    Set,
    Promise,
    Object,
    setTimeout(callback, delay) {
      const id = ++nextTimerId;
      timers.set(id, { callback, delay });
      return id;
    },
    clearTimeout(id) {
      clearedTimers.add(id);
    },
    importScripts() {},
    hasRelayConnection: state => Boolean(state.gateway && (state.token || state.connectionMode === "local")),
    relayIdentity: state => state.connectionMode === "local" ? "local" : state.token,
    PROVIDERS: { chatgpt: ["chatgpt.com"], claude: ["claude.ai"] },
    syncProvider: async provider => {
      calls.push(provider);
      if (failingProviders.includes(provider)) {
        throw new Error("simulated refresh failure");
      }
    },
    chrome: {
      cookies: {
        onChanged: { addListener: callback => listeners.cookieChanged.push(callback) },
      },
      runtime: {
        onInstalled: { addListener: callback => listeners.installed.push(callback) },
        onStartup: { addListener: callback => listeners.startup.push(callback) },
        onMessage: { addListener: callback => listeners.message.push(callback) },
      },
      storage: {
        local: {
          async get() {
            return { linkedProviders };
          },
        },
      },
    },
  };
  const source = fs.readFileSync(path.join(__dirname, "background.js"), "utf8");
  vm.runInNewContext(source, sandbox, { filename: "background.js" });
  return { calls, listeners, timers, clearedTimers, sandbox };
}

function flushAsyncWork() {
  return new Promise(resolve => setImmediate(resolve));
}

test("explicit provider sync reports a skipped upload instead of success", async () => {
  const f = loadBackground(["chatgpt"]);
  let status = "no_cookies";
  f.sandbox.syncProvider = async provider => {
    f.calls.push(provider);
    return status;
  };
  const request = () => new Promise(resolve => {
    const asynchronous = f.listeners.message[0](
      {type:"sync-provider",provider:"chatgpt"}, {}, resolve
    );
    assert.equal(asynchronous, true);
  });
  const skipped = await request();
  assert.equal(skipped.ok, true);
  assert.equal(skipped.syncStatus, "no_cookies");
  status = "uploaded";
  const uploaded = await request();
  assert.equal(uploaded.ok, true);
  assert.equal(uploaded.syncStatus, "uploaded");
  assert.deepEqual(f.calls, ["chatgpt", "chatgpt"]);
});

test("service-worker install and startup restore each unique linked provider", async () => {
  const { calls, listeners } = loadBackground([
    "chatgpt",
    "chatgpt",
    "unsupported-provider",
    "claude",
  ]);

  assert.equal(listeners.installed.length, 1);
  assert.equal(listeners.startup.length, 1);

  listeners.installed[0]();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);

  calls.length = 0;
  listeners.startup[0]();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("one provider refresh failure does not prevent other linked providers", async () => {
  const { calls, listeners } = loadBackground(["chatgpt", "claude"], ["chatgpt"]);

  listeners.startup[0]();
  await flushAsyncWork();

  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("cookie rotation is debounced and rejects lookalike domains", async () => {
  const { calls, listeners, timers, clearedTimers } = loadBackground([]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".evilchatgpt.com" } });
  assert.equal(timers.size, 0, "lookalike domains must not trigger a provider sync");

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: "chatgpt.com" } });
  assert.equal(timers.size, 2);
  assert.ok(clearedTimers.has(1), "the previous debounce timer should be cancelled");
  assert.equal(timers.get(2).delay, 10_000);

  timers.get(1).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, [], "a stale debounce callback must not sync");

  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt"]);
});

test("two linked providers rotate independently without cross-provider sync", async () => {
  const { calls, listeners, timers } = loadBackground(["chatgpt", "claude"]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: ".claude.ai" } });
  assert.equal(timers.size, 2, "each provider should own an independent debounce timer");

  timers.get(1).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt"]);

  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("rapid cookie rotations coalesce per provider while preserving the other provider", async () => {
  const { calls, listeners, timers, clearedTimers } = loadBackground(["chatgpt", "claude"]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: ".claude.ai" } });
  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });

  assert.equal(timers.size, 3);
  assert.ok(clearedTimers.has(1), "only the previous ChatGPT timer should be cancelled");

  timers.get(1).callback();
  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["claude"], "Claude's independent timer should still fire");

  timers.get(3).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["claude", "chatgpt"]);
});

test("slow provider sync coalesces a cookie burst into one latest trailing snapshot", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  let release;
  const held = new Promise(resolve => { release = resolve; });
  const snapshots = [];
  let version = 1;
  f.sandbox.syncProvider = async provider => {
    snapshots.push({provider, version});
    if (provider === "chatgpt" && snapshots.filter(x => x.provider === "chatgpt").length === 1) {
      await held;
    }
  };
  const fire = async domain => {
    f.listeners.cookieChanged[0]({cookie: {domain}});
    [...f.timers.values()].at(-1).callback();
    await flushAsyncWork();
  };
  await fire(".chatgpt.com");
  for (version = 2; version <= 20; version++) await fire(".chatgpt.com");
  version = 20;
  await fire(".claude.ai");
  assert.equal(snapshots.filter(x => x.provider === "chatgpt").length, 1);
  assert.equal(snapshots.filter(x => x.provider === "claude").length, 1,
    "other providers remain independent");
  release();
  for (let i = 0; i < 25; i++) await flushAsyncWork();
  const chat = snapshots.filter(x => x.provider === "chatgpt");
  assert.equal(chat.length, 2, "an in-flight import permits only one queued refresh");
  assert.equal(chat[1].version, 20, "the trailing job reads latest cookie state");
});

test("relay diagnostics message reports redacted local sync failures without raw errors", async () => {
  const f = loadBackground(["chatgpt"], ["chatgpt"]);
  f.listeners.startup[0]();
  await flushAsyncWork();
  const response = await new Promise(resolve => {
    const asyncReply = f.listeners.message[0]({type:"relay-diagnostics"}, {}, resolve);
    assert.equal(asyncReply, true);
  });
  assert.equal(response.ok, false);
  assert.equal(response.gateway_issue, "not_connected");
  assert.equal(response.extension.connected, false);
  assert.equal(response.extension.provider_sync.chatgpt.state, "failed");
  assert.equal(response.extension.provider_sync.chatgpt.issue, "relay_unavailable");
  assert.ok(!JSON.stringify(response).includes("simulated refresh failure"));
});

test("relay diagnostics reads authenticated gateway status without exposing credentials", async () => {
  const f = loadBackground(["chatgpt"]);
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway:"http://127.0.0.1:8080", token:"private-install-token", linkedProviders:["chatgpt"]
  });
  let requests = 0;
  f.sandbox.relayFetch = async path => {
    requests += 1;
    assert.equal(path, "/relay/v1/diagnostics");
    return {ok:true,status:200,json:async()=>({ok:true,providers:[{provider:"chatgpt",issue:"authentication_unverified"}]})};
  };
  const request = () => new Promise(resolve => f.listeners.message[0]({type:"relay-diagnostics"}, {}, resolve));
  const first = await request();
  assert.equal(first.ok, true);
  assert.equal(first.gateway.providers[0].issue, "authentication_unverified");
  assert.equal(first.extension.connected, true);
  assert.equal(requests, 1);
  await request();
  assert.equal(requests, 1, "repeated popup reads should reuse the short-lived dashboard snapshot");
  assert.ok(!JSON.stringify(first).includes("private-install-token"));
});

test("diagnostics separates connection, browser host permissions and saved gateway sessions", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway:"http://127.0.0.1:8080", connectionMode:"local",
    linkedProviders:["chatgpt", "claude", "chatgpt"]
  });
  f.sandbox.chrome.permissions = {
    contains: async ({origins}) => !origins.some(origin => origin.includes("claude.ai"))
  };
  f.sandbox.relayFetch = async () => ({
    ok:true, status:200, json:async () => ({
      providers:[{provider:"chatgpt",linked:true,authentication:"unknown"},
                 {provider:"claude",linked:false,authentication:"unknown"}]
    })
  });
  const response = await new Promise(resolve => f.listeners.message[0](
    {type:"relay-diagnostics",force:true}, {}, resolve
  ));
  assert.equal(response.ok, true);
  assert.equal(response.extension.connected, true);
  assert.equal(response.extension.connection_mode, "local");
  assert.deepEqual(Array.from(response.extension.linked_providers), ["chatgpt", "claude"]);
  assert.equal(response.extension.permissions.gateway, true);
  assert.equal(response.extension.permissions.providers.chatgpt, true);
  assert.equal(response.extension.permissions.providers.claude, false);
  assert.equal(response.gateway.providers[1].linked, false,
    "extension linkage must not rewrite backend saved-session state");
  assert.match(response.extension.gateway_checked_at, /^[0-9]{4}-[0-9]{2}-[0-9]{2}T/);
  assert.ok(!JSON.stringify(response).includes("127.0.0.1:8080"),
    "never include the configured gateway origin in the diagnostics payload");
});

test("forced diagnostic refresh never reports stale success after a gateway failure", async () => {
  const f = loadBackground(["chatgpt"]);
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway:"http://127.0.0.1:8080", connectionMode:"local", linkedProviders:["chatgpt"]
  });
  let reads = 0;
  f.sandbox.relayFetch = async () => {
    reads += 1;
    if (reads === 1) return {ok:true,status:200,json:async () => ({providers:[]})};
    throw new Error("simulated internal network failure with secret");
  };
  const request = force => new Promise(resolve => f.listeners.message[0](
    {type:"relay-diagnostics",force}, {}, resolve
  ));
  assert.equal((await request(true)).ok, true);
  const failed = await request(true);
  assert.equal(failed.ok, false);
  assert.equal(failed.gateway_issue, "relay_unavailable");
  assert.equal(failed.gateway, undefined);
  assert.equal(reads, 2);
  assert.ok(!JSON.stringify(failed).includes("secret"));
});

test("multiple manual syncs coalesce into one forced refresh behind an automatic import", async () => {
  const f = loadBackground(["chatgpt"]);
  let release;
  const held = new Promise(resolve => { release = resolve; });
  const attempts = [];
  f.sandbox.syncProvider = async (_provider, opts) => {
    attempts.push(opts.force);
    if (attempts.length === 1) await held;
    return "uploaded";
  };
  f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
  f.timers.get(1).callback();
  await flushAsyncWork();
  const request = () => new Promise(resolve =>
    f.listeners.message[0]({type:"sync-provider", provider:"chatgpt"}, {}, resolve));
  const a = request();
  const b = request();
  release();
  await Promise.all([a,b]);
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.deepEqual(attempts, [false, true], "manual recovery runs once after the automatic import");
});

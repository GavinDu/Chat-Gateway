const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadBackground(linkedProviders, failingProviders = [], connected = false) {
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
    randomHex: (() => { let counter = 0; return () => "test-epoch-" + (++counter); })(),
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
            return connected
              ? {gateway:"http://127.0.0.1:8080",connectionMode:"local",linkedProviders}
              : { linkedProviders };
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

test("concurrent cross-provider links serialize one shared membership and epoch map", async () => {
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:[],providerLinkEpochs:{}};
  let releaseFirst;
  let began;
  const started=new Promise(resolve=>{began=resolve});
  const blocked=new Promise(resolve=>{releaseFirst=resolve});
  let writes=0;
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async update=>{
    writes++;
    if(writes===1){began(); await blocked;}
    Object.assign(state,update);
  };
  const expected={gateway:state.gateway,connectionMode:"local"};
  const send=(type,provider)=>new Promise(resolve=>
    f.listeners.message[0]({type,provider,connection:expected},{},resolve));
  const first=send("link-provider","chatgpt");
  await started;
  const second=send("link-provider","claude");
  for(let i=0;i<4;i++)await flushAsyncWork();
  assert.equal(writes,1,"second link must not run a storage get/set during first write");
  releaseFirst();
  assert.equal((await first).ok,true);
  assert.equal((await second).ok,true);
  assert.deepEqual(Array.from(state.linkedProviders).sort(),["chatgpt","claude"]);
  assert.ok(state.providerLinkEpochs.chatgpt);
  assert.ok(state.providerLinkEpochs.claude);
  assert.notEqual(state.providerLinkEpochs.chatgpt,state.providerLinkEpochs.claude);
  const before={...state.providerLinkEpochs};
  const restarted=loadBackground([],[],true);
  restarted.sandbox.chrome.storage.local.get=async()=>({...state});
  restarted.sandbox.chrome.storage.local.set=async next=>Object.assign(state,next);
  const repeated=await new Promise(resolve=>restarted.listeners.message[0]({
    type:"link-provider",provider:"chatgpt",connection:expected},{},resolve));
  assert.equal(repeated.ok,true);
  assert.equal(repeated.alreadyLinked,true);
  assert.deepEqual({...state.providerLinkEpochs},before,"MV3 restart preserves epochs");
});

test("logout of provider B racing link of A does not resurrect B consent",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:["claude"],providerLinkEpochs:{claude:"old-consent"}};
  let release;
  let entered;
  const started=new Promise(resolve=>{entered=resolve});
  const gate=new Promise(resolve=>{release=resolve});
  let writes=0;
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async update=>{
    writes++;
    if(writes===1){entered(); await gate;}
    Object.assign(state,update);
  };
  f.sandbox.relayFetch=async()=>({ok:true,status:200});
  const send=(type,provider,connection)=>new Promise(resolve=>
    f.listeners.message[0]({type,provider,connection},{},resolve));
  const newLink=send("link-provider","chatgpt",{
    gateway:state.gateway,connectionMode:"local"});
  await started;
  const logout=send("logout-provider","claude");
  for(let i=0;i<4;i++)await flushAsyncWork();
  assert.equal(writes,1);
  release();
  assert.equal((await newLink).ok,true);
  assert.equal((await logout).ok,true);
  assert.deepEqual(Array.from(state.linkedProviders),["chatgpt"]);
  assert.ok(state.providerLinkEpochs.chatgpt);
  assert.notEqual(state.providerLinkEpochs.claude,"old-consent");
});

test("same-gateway unlink and relink epoch cannot attribute the old upload to new consent", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  const state = {
    gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:["chatgpt"],providerLinkEpochs:{chatgpt:"epoch-old"}
  };
  f.sandbox.chrome.storage.local.get = async () => ({...state});
  f.sandbox.chrome.storage.local.set = async update => Object.assign(state,update);
  const pending = [];
  f.sandbox.syncProvider = async () => new Promise(resolve => pending.push(resolve));
  const send = request => new Promise(resolve =>
    f.listeners.message[0](request, {}, resolve));
  const old = send({type:"sync-provider",provider:"chatgpt"});
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.equal(pending.length,1);

  // This is the same HTTP gateway and install; only the consent generation
  // distinguishes an old account link from the newly explicit opt-in.
  state.linkedProviders = [];
  state.providerLinkEpochs = {chatgpt:"epoch-revoked"};
  state.linkedProviders = ["chatgpt"];
  state.providerLinkEpochs = {chatgpt:"epoch-new"};
  pending.shift()("uploaded");
  assert.equal((await old).ok,true);
  const first = await send({type:"relay-diagnostics"});
  assert.equal(first.extension.provider_sync.chatgpt.state,"not_attempted",
    "stale A completion must not become an uploaded result for new B consent");

  const fresh = send({type:"sync-provider",provider:"chatgpt"});
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.equal(pending.length,1);
  pending.shift()("uploaded");
  assert.equal((await fresh).ok,true);
  const latest = await send({type:"relay-diagnostics"});
  assert.equal(latest.extension.provider_sync.chatgpt.state,"uploaded");
  assert.ok(latest.extension.provider_sync.chatgpt.updated_at);
  assert.equal(state.providerLinkEpochs.chatgpt,"epoch-new",
    "a worker may observe but must not regenerate a persisted consent epoch");
});

test("stale gateway A diagnostics cannot overwrite B cache or poison B cookie recovery", async () => {
  for (const oldFailure of [false, true]) {
    const f = loadBackground(["chatgpt"]);
    const firstGateway = "http://127.0.0.1:8080";
    const secondGateway = "http://127.0.0.1:9090";
    let gateway = firstGateway;
    const releases = new Map();
    let fetches = 0;
    f.sandbox.chrome.storage.local.get = async () => ({
      gateway, connectionMode:"local", linkedProviders:["chatgpt"]
    });
    f.sandbox.relayFetch = async (_path, _options, expected) => {
      fetches += 1;
      return await new Promise(resolve => releases.set(expected.gateway, resolve));
    };
    const diagnostics = force => new Promise(resolve =>
      f.listeners.message[0]({type:"relay-diagnostics",force},{},resolve));

    const old = diagnostics(true);
    await flushAsyncWork();
    assert.equal(typeof releases.get(firstGateway), "function");

    gateway = secondGateway;
    const newer = diagnostics(true);
    await flushAsyncWork();
    assert.equal(typeof releases.get(secondGateway), "function");
    releases.get(secondGateway)({ok:true,status:200,json:async()=>({
      marker:"new-current-gateway",providers:[{provider:"chatgpt",linked:true}]
    })});
    const latest = await newer;
    assert.equal(latest.ok, true);
    assert.equal(latest.gateway.marker, "new-current-gateway");

    releases.get(firstGateway)(oldFailure
      ? {ok:false,status:401}
      : {ok:true,status:200,json:async()=>({
          marker:"stale-previous-gateway",providers:[]
        })});
    const stale = await old;
    assert.equal(stale.ok, false);
    assert.equal(stale.gateway_issue, "gateway_connection_changed",
      "stale authorization errors and old successes must be discarded");

    const cached = await diagnostics(false);
    assert.equal(cached.ok, true);
    assert.equal(cached.gateway.marker, "new-current-gateway");
    assert.equal(fetches, 2, "stale result must not invalidate the new cache");

    f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
    [...f.timers.values()].at(-1).callback();
    await flushAsyncWork();
    assert.deepEqual(f.calls, ["chatgpt"],
      "old identity failures cannot impose a new identity's automatic sync backoff");
  }
});

test("unreachable configured gateway skips automatic imports and resumes after recovery", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway:"http://127.0.0.1:8080", connectionMode:"local",
    linkedProviders:["chatgpt", "claude"]
  });
  let healthy = false;
  f.sandbox.relayFetch = async path => {
    assert.equal(path, "/relay/v1/diagnostics");
    if (!healthy) return {ok:false,status:503};
    return {ok:true,status:200,json:async()=>({
      ok:true,providers:[{provider:"chatgpt",linked:true},{provider:"claude",linked:false}]
    })};
  };
  f.listeners.startup[0]();
  for (let i = 0; i < 3; i++) await flushAsyncWork();
  assert.deepEqual(f.calls, [],
    "automatic refresh must not import cookies when the connected gateway is unavailable");

  // A user-invoked sync remains usable independently of the retry policy.
  const manual = await new Promise(resolve => f.listeners.message[0](
    {type:"sync-provider",provider:"chatgpt"}, {}, resolve
  ));
  assert.equal(manual.ok, true);
  assert.deepEqual(f.calls, ["chatgpt"]);
  f.calls.length = 0;

  healthy = true;
  f.listeners.startup[0]();
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.deepEqual(f.calls, ["chatgpt", "claude"],
    "startup repair must resume after a successful gateway status check");
});

test("old provider import result cannot overwrite new gateway dashboard or cache", async () => {
  for (const oldFailure of [false, true]) {
    const f = loadBackground(["chatgpt"], [], true);
    let gateway = "http://127.0.0.1:8080";
    const oldGateway = gateway;
    let release;
    const held = new Promise(resolve => {release = resolve;});
    f.sandbox.chrome.storage.local.get = async () => ({
      gateway, connectionMode:"local", linkedProviders:["chatgpt"]
    });
    f.sandbox.syncProvider = async provider => {
      f.calls.push(provider);
      await held;
      if (oldFailure) throw new Error("old install sync failure");
      return "uploaded";
    };
    f.sandbox.relayFetch = async () => ({
      ok:true,status:200,json:async()=>({
        marker:"connected-gateway-B",providers:[{provider:"chatgpt",linked:true}]
      })
    });
    const send = (request) => new Promise(resolve =>
      f.listeners.message[0](request,{},resolve));
    const previous = send({type:"sync-provider",provider:"chatgpt"});
    for (let i = 0; i < 3; i++) await flushAsyncWork();
    assert.deepEqual(f.calls, ["chatgpt"], "old gateway import started");

    gateway = "http://127.0.0.1:9090";
    assert.notEqual(gateway, oldGateway);
    const fresh = await send({type:"relay-diagnostics",force:true});
    assert.equal(fresh.ok, true);
    assert.equal(fresh.gateway.marker,"connected-gateway-B");
    assert.equal(fresh.extension.provider_sync.chatgpt.state, "not_attempted");

    release();
    const completed = await previous;
    assert.equal(completed.ok, !oldFailure);
    const cached = await send({type:"relay-diagnostics",force:false});
    assert.equal(cached.ok, true);
    assert.equal(cached.gateway.marker,"connected-gateway-B");
    assert.equal(cached.extension.provider_sync.chatgpt.state,"not_attempted",
      "old gateway upload or error may not become new connection status");
  }
});

test("unlink during in-flight import does not leave a synthetic linked sync result", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let linkedProviders = ["chatgpt"];
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway:"http://127.0.0.1:8080",connectionMode:"local",linkedProviders
  });
  let release;
  const held = new Promise(resolve => {release=resolve});
  f.sandbox.syncProvider = async provider => {
    f.calls.push(provider);
    await held;
    return "uploaded";
  };
  const send = request => new Promise(resolve =>
    f.listeners.message[0](request,{},resolve));
  const old = send({type:"sync-provider",provider:"chatgpt"});
  for (let i = 0; i < 3; i++) await flushAsyncWork();
  linkedProviders = [];
  release();
  await old;
  linkedProviders = ["chatgpt"];
  const after = await send({type:"relay-diagnostics"});
  assert.equal(after.extension.provider_sync.chatgpt.state,"not_attempted",
    "relinked provider must not inherit the old unlinked import result");
});

test("known gateway outage suppresses cookie-triggered imports until health returns or identity changes", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  let healthy = false;
  let gateway = "http://127.0.0.1:8080";
  f.sandbox.chrome.storage.local.get = async () => ({
    gateway, connectionMode:"local", linkedProviders:["chatgpt","claude"]
  });
  f.sandbox.relayFetch = async () => healthy
    ? {ok:true,status:200,json:async()=>({providers:[]})}
    : {ok:false,status:503};
  const diagnostics = () => new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force:true},{},resolve));
  assert.equal((await diagnostics()).ok,false);

  f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
  const firstTimer = [...f.timers.values()].at(-1);
  firstTimer.callback();
  await flushAsyncWork();
  assert.deepEqual(f.calls, [], "known outage must not trigger a queued automatic import");

  // Manual provider recovery is never blocked by the backoff.
  const manually = await new Promise(resolve =>
    f.listeners.message[0]({type:"sync-provider",provider:"chatgpt"},{},resolve));
  assert.equal(manually.ok,true);
  assert.deepEqual(f.calls, ["chatgpt"]);
  f.calls.length = 0;

  healthy = true;
  assert.equal((await diagnostics()).ok,true);
  f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
  [...f.timers.values()].at(-1).callback();
  await flushAsyncWork();
  assert.deepEqual(f.calls, ["chatgpt"], "successful health check must resume automatic resync");
  f.calls.length = 0;

  healthy = false;
  assert.equal((await diagnostics()).ok,false);
  gateway = "http://127.0.0.1:9090";
  f.listeners.cookieChanged[0]({cookie:{domain:".claude.ai"}});
  // Connection identity changes are observed by relayDiagnostics, not by
  // cookie callbacks. A fresh identity check clears the old outage backoff.
  healthy = true;
  assert.equal((await diagnostics()).ok,true);
  [...f.timers.values()].at(-1).callback();
  await flushAsyncWork();
  assert.deepEqual(f.calls, ["claude"]);
});

test("startup repair resnapshots provider consent after a delayed health response",async()=>{
  for(const scenario of [
    {initial:["chatgpt"],latest:[],expected:[]},
    {initial:["chatgpt"],latest:["claude"],expected:["claude"]},
  ]) {
    const f=loadBackground(scenario.initial,[],true);
    let linked=[...scenario.initial];
    let release;
    let started;
    const waiting=new Promise(resolve=>{started=resolve});
    const gate=new Promise(resolve=>{release=resolve});
    f.sandbox.chrome.storage.local.get=async()=>({
      gateway:"http://127.0.0.1:8080",connectionMode:"local",
      linkedProviders:[...linked]
    });
    f.sandbox.relayFetch=async()=>{
      started();
      await gate;
      return {ok:true,status:200,json:async()=>({providers:[]})};
    };
    f.listeners.startup[0]();
    await waiting;
    linked=[...scenario.latest];
    release();
    for(let i=0;i<8;i++)await flushAsyncWork();
    assert.deepEqual(f.calls,scenario.expected,
      "old linked providers cannot queue automatic imports after consent changes");
  }
});

test("startup repair refuses to use old status after gateway identity changes",async()=>{
  const f=loadBackground(["chatgpt"],[],true);
  let gateway="http://127.0.0.1:8080";
  let release;
  let started;
  const waiting=new Promise(resolve=>{started=resolve});
  const gate=new Promise(resolve=>{release=resolve});
  f.sandbox.chrome.storage.local.get=async()=>({
    gateway,connectionMode:"local",linkedProviders:["chatgpt"]
  });
  f.sandbox.relayFetch=async()=>{
    started();
    await gate;
    return {ok:true,status:200,json:async()=>({providers:[]})};
  };
  f.listeners.startup[0]();
  await waiting;
  gateway="http://127.0.0.1:9090";
  release();
  for(let i=0;i<8;i++)await flushAsyncWork();
  assert.deepEqual(f.calls,[],
    "health check from an old gateway must not trigger new automatic imports");
});

test("disconnected startup and install never fan out automatic provider imports", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  f.listeners.startup[0]();
  f.listeners.installed[0]();
  for (let i = 0; i < 5; i++) await flushAsyncWork();
  assert.deepEqual(f.calls, [], "no gateway means no automatic cookie read or import");
  const diagnostics = await new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics"},{},resolve));
  assert.equal(diagnostics.ok, false);
  assert.equal(diagnostics.gateway_issue, "not_connected");
  assert.equal(diagnostics.extension.provider_sync.chatgpt.state, "not_attempted");
  assert.equal(diagnostics.extension.provider_sync.claude.state, "not_attempted");
});

test("service-worker install and startup restore each unique linked provider", async () => {
  const { calls, listeners, sandbox } = loadBackground([
    "chatgpt",
    "chatgpt",
    "unsupported-provider",
    "claude",
  ], [], true);
  sandbox.relayFetch = async () => ({
    ok:true,status:200,json:async()=>({providers:[]})
  });

  assert.equal(listeners.installed.length, 1);
  assert.equal(listeners.startup.length, 1);

  listeners.installed[0]();
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);

  calls.length = 0;
  listeners.startup[0]();
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("one provider refresh failure does not prevent other linked providers", async () => {
  const { calls, listeners, sandbox } = loadBackground(["chatgpt", "claude"], ["chatgpt"], true);
  sandbox.relayFetch = async () => ({ok:true,status:200,json:async()=>({providers:[]})});

  listeners.startup[0]();
  for (let i = 0; i < 4; i++) await flushAsyncWork();

  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("unlinked or disconnected providers never queue automatic cookie imports", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let linkedProviders = [];
  let connected = true;
  f.sandbox.chrome.storage.local.get = async () => ({
    linkedProviders,
    ...(connected ? {gateway:"http://127.0.0.1:8080",connectionMode:"local"} : {})
  });
  const fire = async () => {
    f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
    [...f.timers.values()].at(-1).callback();
    await flushAsyncWork();
  };
  await fire();
  assert.deepEqual(f.calls, [], "revoked provider consent blocks automatic sync");
  linkedProviders = ["chatgpt"];
  connected = false;
  await fire();
  assert.deepEqual(f.calls, [], "disconnected gateway blocks automatic sync");
  connected = true;
  await fire();
  assert.deepEqual(f.calls, ["chatgpt"], "linked provider resync resumes when connected");
});

test("superseded cookie debounce cannot enqueue after a delayed consent read", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let release;
  let first = true;
  f.sandbox.chrome.storage.local.get = async () => {
    if (first) {
      first = false;
      await new Promise(resolve => {release = resolve;});
    }
    return {gateway:"http://127.0.0.1:8080",connectionMode:"local",linkedProviders:["chatgpt"]};
  };
  f.listeners.cookieChanged[0]({cookie:{domain:"chatgpt.com"}});
  const oldTimer = [...f.timers.values()].at(-1);
  const slow = oldTimer.callback();
  await flushAsyncWork();
  f.listeners.cookieChanged[0]({cookie:{domain:".chatgpt.com"}});
  const newest = [...f.timers.values()].at(-1);
  await newest.callback();
  release();
  await slow;
  await flushAsyncWork();
  assert.deepEqual(f.calls, ["chatgpt"], "only the latest generation can sync after async storage");
});

test("cookie rotation is debounced and rejects lookalike domains", async () => {
  const { calls, listeners, timers, clearedTimers } = loadBackground(["chatgpt"], [], true);
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
  const { calls, listeners, timers } = loadBackground(["chatgpt", "claude"], [], true);
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
  const { calls, listeners, timers, clearedTimers } = loadBackground(["chatgpt", "claude"], [], true);
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
  const f = loadBackground(["chatgpt", "claude"], [], true);
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
  // Manual explicit sync remains independent of automatic disconnect skip.
  const manual = await new Promise(resolve => {
    f.listeners.message[0]({type:"sync-provider",provider:"chatgpt"},{},resolve);
  });
  assert.equal(manual.ok,false);
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

test("provider sync timestamp is independent of login and turns stale when gateway status fails", async () => {
  const f = loadBackground(["chatgpt", "claude"]);
  let healthy = true;
  let connected = true;
  f.sandbox.chrome.storage.local.get = async () => connected
    ? {gateway:"http://127.0.0.1:8080",connectionMode:"local",linkedProviders:["chatgpt","claude"]}
    : {linkedProviders:["chatgpt","claude"]};
  f.sandbox.relayFetch = async () => healthy
    ? {ok:true,status:200,json:async()=>({providers:[
      {provider:"chatgpt",linked:false,authentication:"unknown"}
    ]})}
    : {ok:false,status:503};
  const diagnostic = () => new Promise(resolve => f.listeners.message[0](
    {type:"relay-diagnostics",force:true},{},resolve));
  const clean = await diagnostic();
  assert.equal(clean.ok,true);
  assert.equal(clean.extension.provider_sync.chatgpt.state,"not_attempted");
  assert.equal(clean.extension.provider_sync.chatgpt.updated_at,null);
  assert.equal(clean.extension.provider_sync.chatgpt.stale,false);
  const synced = await new Promise(resolve => f.listeners.message[0](
    {type:"sync-provider",provider:"chatgpt"},{},resolve));
  assert.equal(synced.ok,true);
  const after = await diagnostic();
  const previous = after.extension.provider_sync.chatgpt;
  assert.equal(previous.stale,false);
  assert.match(previous.updated_at,/^\d{4}-\d{2}-\d{2}T/);
  assert.equal(after.gateway.providers[0].authentication,"unknown",
    "a successful local sync must never imply authenticated login");
  assert.equal(after.extension.provider_sync.claude.state,"not_attempted");
  healthy = false;
  const failed = await diagnostic();
  assert.equal(failed.ok,false);
  assert.equal(failed.gateway,undefined);
  assert.equal(failed.extension.provider_sync.chatgpt.stale,true);
  assert.equal(failed.extension.provider_sync.chatgpt.updated_at,previous.updated_at);
  assert.equal(failed.extension.provider_sync.claude.stale,true);
  healthy = true;
  assert.equal((await diagnostic()).extension.provider_sync.chatgpt.stale,false);
  connected = false;
  const disconnected = await diagnostic();
  assert.equal(disconnected.ok,false);
  assert.equal(disconnected.gateway_issue,"not_connected");
  assert.equal(disconnected.extension.provider_sync.chatgpt.updated_at,null,
    "disconnect must not leak an old install's provider state");
  // MV3 worker restart conservatively resets in-memory sync evidence.
  const restarted = loadBackground(["chatgpt"]);
  const restartDiagnostic = await new Promise(resolve => restarted.listeners.message[0](
    {type:"relay-diagnostics"}, {}, resolve));
  assert.equal(restartDiagnostic.extension.provider_sync.chatgpt.state,"not_attempted");
  assert.equal(restartDiagnostic.extension.provider_sync.chatgpt.updated_at,null);
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
  const afterFailure = await request(false);
  assert.equal(afterFailure.ok, false,
    "a failed forced refresh must invalidate the former successful diagnostics cache");
  assert.equal(afterFailure.gateway, undefined);
  assert.equal(reads, 3);
});

test("multiple manual syncs coalesce into one forced refresh behind an automatic import", async () => {
  const f = loadBackground(["chatgpt"], [], true);
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

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadBackground(linkedProviders, failingProviders = [], connected = false) {
  const calls = [];
  const timers = new Map();
  const clearedTimers = new Set();
  const createdAlarms = [];
  let nextTimerId = 0;
  const listeners = {
    installed: [],
    startup: [],
    cookieChanged: [],
    message: [],
    alarm: [],
  };
  const sandbox = {
    Map,
    Set,
    randomHex: (() => { let counter = 0; return () => "test-epoch-" + (++counter); })(),
    isLocalGateway: gateway => typeof gateway==="string" &&
      (/^http:\/\/127\.0\.0\.1:[0-9]+$/.test(gateway) || /^http:\/\/localhost:[0-9]+$/.test(gateway)),
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
      alarms: {
        create: (name, options) => createdAlarms.push({name, ...options}),
        onAlarm: { addListener: callback => listeners.alarm.push(callback) },
      },
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
  return { calls, listeners, timers, clearedTimers, createdAlarms, sandbox };
}

function flushAsyncWork() {
  return new Promise(resolve => setImmediate(resolve));
}

test("periodic relay alarm refreshes status without recreating repeating alarm", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let probes = 0;
  f.sandbox.relayFetch = async () => {
    probes++;
    return {ok:true,status:200,json:async()=>({providers:[{provider:"chatgpt",linked:true}]})};
  };

  f.listeners.startup[0]();
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.deepEqual(
    JSON.parse(JSON.stringify(f.createdAlarms)),
    [{name:"relay-session-refresh",periodInMinutes:15}],
    "startup must register one bounded repeating refresh"
  );
  assert.equal(probes, 1);

  f.listeners.alarm[0]({name:"relay-session-refresh"});
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.equal(probes, 2, "periodic wake must still query fresh gateway health");
  assert.equal(
    f.createdAlarms.length, 1,
    "a repeating alarm must not rearm/reset itself on every firing"
  );

  f.listeners.alarm[0]({name:"relay-unknown"});
  for (let i = 0; i < 4; i++) await flushAsyncWork();
  assert.equal(probes, 2, "unknown alarms are not provider heartbeat signals");
  assert.equal(f.createdAlarms.length, 1);
});

test("relay diagnostics classify user cancellation apart from gateway timeout", () => {
  const f = loadBackground([]);
  assert.equal(
    f.sandbox.classifyRelayFailure(new Error("Gateway request cancelled; check provider status before retrying.")),
    "gateway_cancelled"
  );
  assert.equal(
    f.sandbox.classifyRelayFailure(new Error("Gateway request timed out; the session may have changed.")),
    "gateway_timeout"
  );
  assert.equal(
    f.sandbox.classifyRelayFailure(new Error("private cookie=secret and unrecognized transport failure")),
    "relay_unavailable",
    "never expose raw upstream text in diagnostic issue codes"
  );
  assert.equal(
    f.sandbox.classifyRelayFailure(new Error("provider callback cancelled by upstream proxy")),
    "relay_unavailable",
    "unverified upstream text must not impersonate explicit owner cancellation"
  );
});

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

test("local disconnect cannot be undone by a delayed queued provider link",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:[],providerLinkEpochs:{}};
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  let begin;
  let release;
  const started=new Promise(resolve=>{begin=resolve});
  const gate=new Promise(resolve=>{release=resolve});
  let writes=0;
  f.sandbox.chrome.storage.local.set=async change=>{
    writes++;
    if(writes===1){begin();await gate;}
    Object.assign(state,change);
  };
  f.sandbox.chrome.storage.local.remove=async keys=>{
    for(const key of Array.isArray(keys)?keys:[keys])delete state[key];
  };
  const send=message=>new Promise(resolve=>f.listeners.message[0](message,{},resolve));
  const expected={gateway:state.gateway,connectionMode:"local"};
  const link=send({type:"link-provider",provider:"chatgpt",
    connection:expected,attemptId:"attempt-to-link"});
  await started;
  const disconnect=send({type:"disconnect-local-relay",connection:expected});
  for(let i=0;i<4;i++)await flushAsyncWork();
  assert.equal(writes,1,"disconnect must queue behind pending provider consent mutation");
  release();
  assert.equal((await link).ok,true);
  assert.equal((await disconnect).ok,true);
  assert.equal(state.localDisconnected,true);
  assert.equal(state.connectionMode,undefined);
  assert.equal(state.linkedProviders,undefined);
  assert.ok(!state.token);

  const stale=await send({type:"link-provider",provider:"chatgpt",
    connection:expected,attemptId:"late-attempt"});
  assert.equal(stale.ok,false,"a stale popup cannot re-link after disconnect");
  assert.equal(state.linkedProviders,undefined);
});

test("stale automatic local connect cannot reverse explicit opt-out; manual connect can",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",localDisconnected:true,
    linkedProviders:[],providerLinkEpochs:{}};
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async next=>Object.assign(state,next);
  f.sandbox.chrome.storage.local.remove=async keys=>{
    for(const k of Array.isArray(keys)?keys:[keys])delete state[k];
  };
  const send=message=>new Promise(resolve=>f.listeners.message[0](message,{},resolve));
  const stale=await send({type:"connect-local-relay",gateway:state.gateway,manual:false});
  assert.equal(stale.ok,false);
  assert.equal(state.localDisconnected,true);
  assert.equal(state.connectionMode,undefined);
  const manual=await send({type:"connect-local-relay",gateway:state.gateway,manual:true});
  assert.equal(manual.ok,true);
  assert.equal(state.localDisconnected,false);
  assert.equal(state.connectionMode,"local");
  assert.deepEqual(Array.from(state.linkedProviders),[]);
  assert.equal(state.token,undefined);
});

test("local gateway switch resets linked-provider consent rather than carrying cookies to new install",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:["chatgpt"],providerLinkEpochs:{chatgpt:"older-consent"}};
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async next=>Object.assign(state,next);
  f.sandbox.chrome.storage.local.remove=async keys=>{
    for(const k of Array.isArray(keys)?keys:[keys])delete state[k];
  };
  const result=await new Promise(resolve=>f.listeners.message[0]({
    type:"connect-local-relay",gateway:"http://127.0.0.1:9090",manual:true
  },{},resolve));
  assert.equal(result.ok,true);
  assert.equal(state.gateway,"http://127.0.0.1:9090");
  assert.equal(state.connectionMode,"local");
  assert.deepEqual(Array.from(state.linkedProviders),[]);
  assert.equal(state.providerLinkEpochs.chatgpt,"older-consent",
    "epochs may be retained as private history but old link consent must not be active");
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
  const send=(type,provider,attemptId)=>new Promise(resolve=>
    f.listeners.message[0]({type,provider,connection:expected,attemptId},{},resolve));
  const first=send("link-provider","chatgpt","attempt-chatgpt");
  await started;
  const second=send("link-provider","claude","attempt-claude");
  for(let i=0;i<4;i++)await flushAsyncWork();
  assert.equal(writes,1,"second link must not run a storage get/set during first write");
  releaseFirst();
  assert.equal((await first).ok,true);
  assert.equal((await second).ok,true);
  assert.equal((await send("commit-provider-link","chatgpt","attempt-chatgpt")).committed,true);
  assert.equal((await send("commit-provider-link","claude","attempt-claude")).committed,true);
  assert.deepEqual(Array.from(state.linkedProviders).sort(),["chatgpt","claude"]);
  assert.ok(state.providerLinkEpochs.chatgpt);
  assert.ok(state.providerLinkEpochs.claude);
  assert.notEqual(state.providerLinkEpochs.chatgpt,state.providerLinkEpochs.claude);
  const before={...state.providerLinkEpochs};
  const restarted=loadBackground([],[],true);
  restarted.sandbox.chrome.storage.local.get=async()=>({...state});
  restarted.sandbox.chrome.storage.local.set=async next=>Object.assign(state,next);
  const repeated=await new Promise(resolve=>restarted.listeners.message[0]({
    type:"link-provider",provider:"chatgpt",connection:expected,
    attemptId:"attempt-after-restart"},{},resolve));
  assert.equal(repeated.ok,true);
  assert.equal(repeated.alreadyLinked,true);
  assert.deepEqual({...state.providerLinkEpochs},before,"MV3 restart preserves epochs");
});

test("stale failed first link cannot revoke a newer same-gateway provisional or committed link",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:[],providerLinkEpochs:{},providerLinkPendingAttempts:{}};
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async change=>Object.assign(state,change);
  const expected={gateway:state.gateway,connectionMode:"local"};
  const send=(type,attemptId)=>new Promise(resolve=>f.listeners.message[0]({
    type,provider:"chatgpt",connection:expected,attemptId
  },{},resolve));
  const a=await send("link-provider","attempt-old-A");
  assert.equal(a.ok,true);
  assert.equal(a.tentative,true);
  const oldEpoch=state.providerLinkEpochs.chatgpt;
  const b=await send("link-provider","attempt-new-B");
  assert.equal(b.ok,true);
  assert.equal(b.tentative,true);
  assert.notEqual(state.providerLinkEpochs.chatgpt,oldEpoch);
  assert.equal(state.providerLinkPendingAttempts.chatgpt,"attempt-new-B");

  const commit=await send("commit-provider-link","attempt-new-B");
  assert.equal(commit.committed,true);
  const oldRollback=await send("rollback-provider-link","attempt-old-A");
  assert.equal(oldRollback.rolledBack,false,"old A must not revoke B");
  assert.deepEqual(Array.from(state.linkedProviders),["chatgpt"]);
  assert.equal(state.providerLinkPendingAttempts.chatgpt,undefined);
  const saved=state.providerLinkEpochs.chatgpt;
  const repeated=await send("link-provider","attempt-normal-sync");
  assert.equal(repeated.alreadyLinked,true);
  assert.equal(repeated.tentative,false,"ordinary subsequent sync is not a fresh provisional link");
  assert.equal(state.providerLinkEpochs.chatgpt,saved);

  const restarted=loadBackground([],[],true);
  restarted.sandbox.chrome.storage.local.get=async()=>({...state});
  restarted.sandbox.chrome.storage.local.set=async x=>Object.assign(state,x);
  const oldAcrossRestart=await new Promise(resolve=>restarted.listeners.message[0]({
    type:"rollback-provider-link",provider:"chatgpt",connection:expected,
    attemptId:"attempt-old-A"
  },{},resolve));
  assert.equal(oldAcrossRestart.rolledBack,false);
  assert.deepEqual(Array.from(state.linkedProviders),["chatgpt"]);
});

test("failed current tentative link rolls back without affecting other provider consent",async()=>{
  const f=loadBackground([],[],true);
  const state={gateway:"http://127.0.0.1:8080",connectionMode:"local",
    linkedProviders:["claude"],providerLinkEpochs:{claude:"established"},
    providerLinkPendingAttempts:{}};
  f.sandbox.chrome.storage.local.get=async()=>({...state});
  f.sandbox.chrome.storage.local.set=async patch=>Object.assign(state,patch);
  const expected={gateway:state.gateway,connectionMode:"local"};
  const send=(type,provider,attemptId)=>new Promise(resolve=>f.listeners.message[0]({
    type,provider,connection:expected,attemptId
  },{},resolve));
  assert.equal((await send("link-provider","chatgpt","attempt-first-A")).tentative,true);
  assert.equal((await send("rollback-provider-link","chatgpt","attempt-first-A")).rolledBack,true);
  assert.deepEqual(Array.from(state.linkedProviders),["claude"]);
  assert.equal(state.providerLinkEpochs.claude,"established");
  assert.equal(state.providerLinkPendingAttempts.chatgpt,undefined);
  const revokedEpoch=state.providerLinkEpochs.chatgpt;
  assert.equal((await send("link-provider","chatgpt","attempt-second-B")).tentative,true);
  assert.notEqual(state.providerLinkEpochs.chatgpt,revokedEpoch);
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
    f.listeners.message[0]({type,provider,connection,
      attemptId:type==="link-provider" ? "attempt-link-a" : undefined},{},resolve));
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

test("simultaneous same-identity diagnostics share one POST but retain each consent snapshot", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  const state = {
    gateway:"http://127.0.0.1:8080",
    connectionMode:"local", linkedProviders:["chatgpt"]
  };
  f.sandbox.chrome.storage.local.get = async () => ({...state});
  let fetches = 0;
  let release;
  f.sandbox.relayFetch = async path => {
    assert.equal(path, "/relay/v1/diagnostics");
    fetches++;
    return await new Promise(resolve => { release = resolve; });
  };
  const diagnostics = force => new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force},{},resolve));

  const first = diagnostics(true);
  await flushAsyncWork();
  assert.equal(fetches, 1);
  // Replacing the linked-provider list does not change the gateway token.
  // A second popup view must retain its newer consent snapshot even though
  // it shares the same read-only gateway health request.
  state.linkedProviders = ["claude"];
  const second = diagnostics(true);
  await flushAsyncWork();
  assert.equal(fetches, 1, "same gateway identity must not duplicate health requests");

  release({ok:true,status:200,json:async()=>({marker:"same-identity",providers:[]})});
  const [oldView, newView] = await Promise.all([first,second]);
  assert.equal(oldView.ok, true);
  assert.equal(newView.ok, true);
  assert.equal(oldView.gateway.marker, "same-identity");
  assert.equal(newView.gateway.marker, "same-identity");
  assert.deepEqual(Array.from(oldView.extension.linked_providers), ["chatgpt"]);
  assert.deepEqual(Array.from(newView.extension.linked_providers), ["claude"]);
  assert.notEqual(newView.gateway_issue, "gateway_connection_changed");
  await diagnostics(false);
  assert.equal(fetches, 1, "the completed probe still supplies the normal short-lived cache");

  f.sandbox.relayFetch = async () => {
    fetches++;
    return {ok:true,status:200,json:async()=>({marker:"fresh-forced",providers:[]})};
  };
  const refreshed = await diagnostics(true);
  assert.equal(refreshed.ok, true);
  assert.equal(refreshed.gateway.marker, "fresh-forced");
  assert.equal(fetches, 2, "a later forced refresh must make a new request");
});

test("same-identity concurrent failed diagnostics release single-flight for recovery", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let fetches = 0;
  let release;
  f.sandbox.relayFetch = async () => {
    fetches++;
    return await new Promise(resolve => { release = resolve; });
  };
  const diagnostics = force => new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force},{},resolve));
  const first = diagnostics(true);
  const second = diagnostics(false);
  await flushAsyncWork();
  assert.equal(fetches, 1, "same identity's forced and unforced calls join");
  release({ok:false,status:503});
  const [a,b] = await Promise.all([first,second]);
  assert.equal(a.ok, false);
  assert.equal(b.ok, false);
  assert.equal(a.gateway_issue, "gateway_request_failed");
  assert.equal(b.gateway_issue, "gateway_request_failed");

  f.sandbox.relayFetch = async () => {
    fetches++;
    return {ok:true,status:200,json:async()=>({marker:"recovered",providers:[]})};
  };
  const recovered = await diagnostics(false);
  assert.equal(recovered.ok, true);
  assert.equal(recovered.gateway.marker, "recovered");
  assert.equal(fetches, 2, "failure must release the lease and allow retry");
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

test("overlapping startup and install share one automatic status and provider refresh", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let releaseStatus;
  const heldStatus = new Promise(resolve => { releaseStatus = resolve; });
  let probes = 0;
  f.sandbox.relayFetch = async () => {
    probes++;
    if (probes === 1) await heldStatus;
    return {ok:true,status:200,json:async()=>({providers:[]})};
  };

  f.listeners.startup[0]();
  f.listeners.installed[0]();
  for (let i = 0; i < 6; i++) await flushAsyncWork();
  assert.equal(probes, 1, "overlapping lifecycle events must not duplicate gateway diagnostics");
  assert.deepEqual(f.calls, [], "provider import must await the single status probe");

  // Explicit manual recovery is never blocked by an automatic heartbeat.
  const manual = await new Promise(resolve => f.listeners.message[0](
    {type:"sync-provider", provider:"chatgpt"}, {}, resolve));
  assert.equal(manual.ok, true);
  assert.deepEqual(f.calls, ["chatgpt"]);

  releaseStatus();
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.equal(probes, 1);
  assert.deepEqual(f.calls, ["chatgpt", "chatgpt"],
    "one automatic refresh still repairs the missing provider snapshot");

  // No persistent cache: a later alarm/startup can verify newly missing data.
  f.listeners.startup[0]();
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.equal(probes, 2);
  assert.deepEqual(f.calls, ["chatgpt", "chatgpt", "chatgpt"]);
});

test("failed automatic diagnostics releases scheduled single-flight lease", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  let probes = 0;
  let unavailable = true;
  f.sandbox.relayFetch = async () => {
    probes++;
    if (unavailable) throw new Error("simulated gateway outage with secret");
    return {ok:true,status:200,json:async()=>({providers:[]})};
  };
  f.listeners.startup[0]();
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.equal(probes, 1);
  assert.deepEqual(f.calls, [], "failed health check must not queue cookie imports");

  unavailable = false;
  f.listeners.installed[0]();
  for (let i = 0; i < 8; i++) await flushAsyncWork();
  assert.equal(probes, 2, "a later alarm/install must retry after failure");
  assert.deepEqual(f.calls, ["chatgpt"]);
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

test("live provider diagnostics show a held sync and preserve it on first status read", async () => {
  const f = loadBackground(["chatgpt", "claude"], [], true);
  let release;
  let started = false;
  const held = new Promise(resolve => { release = resolve; });
  f.sandbox.syncProvider = async provider => {
    assert.equal(provider, "chatgpt");
    started = true;
    await held;
    return "uploaded";
  };
  f.sandbox.relayFetch = async () => ({
    ok:true, status:200, json:async()=>({providers:[
      {provider:"chatgpt",linked:false,authentication:"unknown"},
      {provider:"claude",linked:false,authentication:"unknown"}
    ]})
  });
  const diagnosis = () => new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force:true},{},resolve));
  // This import begins before *any* dashboard check has initialized its
  // connection identity; that first dashboard query must not erase it.
  const pending = new Promise(resolve =>
    f.listeners.message[0]({type:"sync-provider",provider:"chatgpt"},{},resolve));
  for (let i = 0; i < 16 && !started; i++) await flushAsyncWork();
  assert.equal(started, true);
  const during = await diagnosis();
  assert.equal(during.ok, true);
  assert.equal(during.extension.provider_sync.chatgpt.state, "syncing");
  assert.equal(during.extension.provider_sync.chatgpt.issue, null);
  assert.match(during.extension.provider_sync.chatgpt.updated_at, /^\d{4}-\d{2}-\d{2}T/);
  assert.equal(during.extension.provider_sync.claude.state, "not_attempted");
  assert.equal(during.gateway.providers[0].authentication, "unknown");
  assert.ok(!JSON.stringify(during).includes("private-cookie"));

  release();
  const completed = await pending;
  assert.equal(completed.ok, true);
  assert.equal(completed.syncStatus, "uploaded");
  const after = await diagnosis();
  assert.equal(after.extension.provider_sync.chatgpt.state, "uploaded");
  assert.equal(after.extension.provider_sync.claude.state, "not_attempted");
  assert.equal(after.gateway.providers[0].authentication, "unknown",
    "successful import is not proof of signed-in chat");
});

test("in-flight syncing status cannot survive a gateway replacement", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  const state = {
    gateway:"http://127.0.0.1:8080", connectionMode:"local",
    linkedProviders:["chatgpt"], providerLinkEpochs:{chatgpt:"first-owner"}
  };
  f.sandbox.chrome.storage.local.get = async () => ({...state});
  f.sandbox.relayFetch = async () => ({
    ok:true,status:200,json:async()=>({providers:[{provider:"chatgpt",linked:false}]})
  });
  let release;
  let started = false;
  const held = new Promise(resolve => { release = resolve; });
  f.sandbox.syncProvider = async () => {
    started = true;
    await held;
    return "uploaded";
  };
  const diagnose = () => new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force:true},{},resolve));
  const pending = new Promise(resolve =>
    f.listeners.message[0]({type:"sync-provider",provider:"chatgpt"},{},resolve));
  for (let i = 0; i < 16 && !started; i++) await flushAsyncWork();
  assert.equal((await diagnose()).extension.provider_sync.chatgpt.state,"syncing");

  state.gateway = "http://localhost:9090";
  state.providerLinkEpochs = {chatgpt:"replacement-owner"};
  assert.equal((await diagnose()).extension.provider_sync.chatgpt.state,"not_attempted",
    "new gateway must not inherit old in-progress diagnostics");

  release();
  const completed = await pending;
  assert.equal(completed.ok,true);
  assert.equal((await diagnose()).extension.provider_sync.chatgpt.state,"not_attempted",
    "stale import completion must not overwrite replacement gateway status");
});

test("gateway A-B-A identity transition invalidates a held old status probe", async () => {
  const f = loadBackground(["chatgpt"], [], true);
  const state = {
    gateway:"http://127.0.0.1:8080", connectionMode:"local", linkedProviders:["chatgpt"]
  };
  f.sandbox.chrome.storage.local.get = async () => ({...state});
  let release;
  let dispatched = false;
  const held = new Promise(resolve => { release = resolve; });
  f.sandbox.relayFetch = async () => {
    dispatched = true;
    await held;
    return {ok:true,status:200,json:async()=>({providers:[{provider:"chatgpt",linked:true}]})};
  };
  const first = new Promise(resolve =>
    f.listeners.message[0]({type:"relay-diagnostics",force:true},{},resolve));
  for (let i = 0; i < 16 && !dispatched; i++) await flushAsyncWork();
  assert.equal(dispatched, true);

  state.gateway = "http://localhost:9090";
  f.sandbox.ensureGatewayDiagnosticIdentity(state);
  state.gateway = "http://127.0.0.1:8080";
  f.sandbox.ensureGatewayDiagnosticIdentity(state);
  release();

  const old = await first;
  assert.equal(old.ok, false, "return to gateway A must not revive its stale probe");
  assert.equal(old.gateway_issue, "gateway_connection_changed");
  assert.equal(old.gateway, undefined);
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

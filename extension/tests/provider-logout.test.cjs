const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadWorker({failDelete = false} = {}) {
  const values = {
    gateway:"http://127.0.0.1:8080",
    token:"install-old",
    linkedProviders:["metaai"]
  };
  const calls = [];
  const listeners = {};
  let releaseSync;
  let startedSync;
  const started = new Promise(resolve => { startedSync = resolve; });
  const gate = new Promise(resolve => { releaseSync = resolve; });
  const sandbox = {
    Map, Promise, Set,
    importScripts() {},
    clearTimeout() {},
    setTimeout() { return 1; },
    PROVIDERS: {metaai:["meta.ai"]},
    chrome: {
      cookies:{onChanged:{addListener(fn){listeners.cookie=fn;}}},
      runtime:{
        onMessage:{addListener(fn){listeners.message=fn;}},
        onInstalled:{addListener(){}},onStartup:{addListener(){}},
      },
      alarms:{onAlarm:{addListener(){}}},
      storage:{local:{
        async get() {return {...values};},
        async set(next) {Object.assign(values,next);},
      }},
    },
    async syncProvider(provider) {
      if (!values.linkedProviders.includes(provider)) return;
      calls.push("sync " + provider);
      startedSync();
      await gate;
    },
    async relayFetch(url, args, expected) {
      if (expected && (expected.gateway !== values.gateway || expected.token !== values.token)) {
        throw new Error("Gateway connection changed during provider logout.");
      }
      calls.push(args.method + " " + url);
      return {ok: !failDelete, status:failDelete?500:200};
    },
  };
  vm.runInNewContext(
    fs.readFileSync(path.join(__dirname,"..","background.js"),"utf8"),
    sandbox, {filename:"background.js"}
  );
  function send(message) {
    return new Promise(resolve => {
      const result = listeners.message(message, {}, resolve);
      assert.equal(result, true, "service-worker message must hold response port");
    });
  }
  return {values,calls,send,started,releaseSync:() => releaseSync()};
}

test("logout waits for in-flight sync, then revokes and blocks further sync", async () => {
  const worker = loadWorker();
  const syncing = worker.send({type:"sync-provider",provider:"metaai"});
  await worker.started;
  const loggingOut = worker.send({type:"logout-provider",provider:"metaai"});
  await Promise.resolve();
  assert.deepEqual(worker.calls, ["sync metaai"]);
  worker.releaseSync();
  assert.equal((await syncing).ok,true);
  assert.equal((await loggingOut).ok,true);
  assert.deepEqual(worker.calls, ["sync metaai","DELETE /relay/v1/session/metaai"]);
  assert.deepEqual(Array.from(worker.values.linkedProviders), []);
  await worker.send({type:"sync-provider",provider:"metaai"});
  assert.equal(worker.calls.length,2,"post-logout cookie refresh must not reimport session");
});

test("failed vault deletion never silently re-enables background sync", async () => {
  const worker = loadWorker({failDelete:true});
  worker.releaseSync();
  const result = await worker.send({type:"logout-provider",provider:"metaai"});
  assert.equal(result.ok,false);
  assert.match(result.error,/logout failed/);
  assert.deepEqual(Array.from(worker.values.linkedProviders),[]);
});

test("queued logout cannot revoke a different gateway after re-pair", async () => {
  const worker = loadWorker();
  const syncing = worker.send({type:"sync-provider",provider:"metaai"});
  await worker.started;
  const loggingOut = worker.send({type:"logout-provider",provider:"metaai"});
  // The logout captures the original connection at message receipt, but its
  // work cannot execute until the preceding provider sync completes.
  await Promise.resolve();
  worker.values.gateway = "https://new-gateway.example";
  worker.values.token = "install-new";
  worker.values.linkedProviders = ["metaai"];
  worker.releaseSync();
  assert.equal((await syncing).ok, true);
  const result = await loggingOut;
  assert.equal(result.ok, false);
  assert.match(result.error, /Gateway connection changed/);
  assert.deepEqual(worker.calls, ["sync metaai"]);
  assert.deepEqual(worker.values.linkedProviders, ["metaai"]);
});

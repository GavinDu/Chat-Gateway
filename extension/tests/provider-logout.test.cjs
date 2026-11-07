const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadWorker({failDelete = false} = {}) {
  const values = {linkedProviders:["metaai"]};
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
    async relayFetch(url, args) {
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

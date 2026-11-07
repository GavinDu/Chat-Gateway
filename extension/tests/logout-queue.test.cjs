const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const flush = () => new Promise(resolve => setImmediate(resolve));

test("logout waits for pending imports and prevents automatic re-link", async () => {
  const listeners = [];
  const state = {
    gateway:"http://127.0.0.1:8080",
    token:"install-old",
    linkedProviders:["metaai"]
  };
  const calls = [];
  let completeSync;
  const sandbox = {
    Map, Set, Promise, Date, setTimeout, clearTimeout, importScripts() {},
    PROVIDERS:{metaai:["meta.ai"]},
    syncProvider:async provider=>{
      if (!state.linkedProviders.includes(provider)) return;
      calls.push("sync:" + provider);
      await new Promise(resolve => {completeSync = resolve;});
    },
    relayFetch:async (uri, opts)=>{
      calls.push(opts.method + ":" + uri);
      return {ok:true,status:200};
    },
    chrome:{
      cookies:{onChanged:{addListener(){}}},
      alarms:{onAlarm:{addListener(){}},clear:async()=>{},create:async()=>{}},
      runtime:{onInstalled:{addListener(){}},onStartup:{addListener(){}},onMessage:{addListener:fn=>listeners.push(fn)}},
      storage:{local:{
        get:async()=>({...state}),
        set:async data=>Object.assign(state,data),
      }},
    },
  };
  vm.runInNewContext(
    fs.readFileSync(path.join(__dirname,"..","background.js"),"utf8"),
    sandbox,{filename:"background.js"}
  );
  const send = (type) => new Promise(resolve => {
    const handled = listeners[0]({type,provider:"metaai"},null,resolve);
    assert.equal(handled,true);
  });

  const sync = send("sync-provider");
  await flush();
  assert.deepEqual(calls,["sync:metaai"]);
  const logout = send("logout-provider");
  await flush();
  assert.deepEqual(calls,["sync:metaai"],"revoke must wait for in-flight sync");

  completeSync();
  assert.equal((await sync).ok,true);
  assert.equal((await logout).ok,true);
  assert.deepEqual(calls,["sync:metaai","DELETE:/relay/v1/session/metaai"]);
  assert.deepEqual(state.linkedProviders,[]);

  assert.equal((await send("sync-provider")).ok,true);
  assert.deepEqual(calls,["sync:metaai","DELETE:/relay/v1/session/metaai"],
    "queued or later cookie change must not upload a logged-out session");
});

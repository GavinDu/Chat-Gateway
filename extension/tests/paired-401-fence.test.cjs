const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function load(extra = {}) {
  const state = {
    gateway:"https://gateway.example", token:"a".repeat(64),
    connectionMode:"paired", relayConnectionEpoch:"gen-a",
    linkedProviders:["chatgpt","manus"],
    providerLinkEpochs:{chatgpt:"epoch-a",manus:"epoch-b"},
    providerLinkPendingAttempts:{chatgpt:"pending-a"},
    relaySnapshotVersions:{chatgpt:3},
    ...extra
  };
  let listener;
  let rotation=0;
  const context={
    Map,Set,Promise,Date,setTimeout,clearTimeout,
    importScripts(){},
    PROVIDERS:{chatgpt:["chatgpt.com"],manus:["manus.im"]},
    randomHex:()=>("revoked-epoch-"+(++rotation)),
    isLocalGateway:value=>typeof value==="string" && value.startsWith("http://127.0.0.1:"),
    hasRelayConnection:value=>Boolean(value.gateway && (value.token || value.connectionMode==="local")),
    relayIdentity:value=>value.connectionMode==="local"?"local":value.token,
    normalizeGateway:value=>new URL(value).origin,
    chrome:{
      runtime:{
        onInstalled:{addListener(){}},onStartup:{addListener(){}},
        onMessage:{addListener(callback){listener=callback;}}
      },
      cookies:{onChanged:{addListener(){}}},
      permissions:{contains:async()=>true},
      alarms:{onAlarm:{addListener(){}},clear:async()=>{}},
      storage:{local:{
        get:async()=>({...state}),
        set:async patch=>Object.assign(state,patch),
        remove:async keys=>{for(const key of Array.isArray(keys)?keys:[keys])delete state[key];}
      }}
    }
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(require.resolve("../background.js"),"utf8")+
    "\nglobalThis.invalidatePaired=invalidatePairedRelay;globalThis.holdConsent=queueConsentMutation;",context);
  const expected=()=>({
    gateway:state.gateway,token:state.token,
    connectionMode:state.connectionMode,relayConnectionEpoch:state.relayConnectionEpoch
  });
  const send=connection=>new Promise(resolve=>{
    assert.equal(listener({type:"invalidate-paired-relay",connection},{},resolve),true);
  });
  return {state,expected,send,hold:context.holdConsent,invalidate:context.invalidatePaired,
    storage:context.chrome.storage.local};
}

test("gateway 401 invalidation serializes behind consent writer and revokes only current paired identity",async()=>{
  const f=load({pendingConnection:{request_id:"still-active",gateway:"https://gateway.example",expires:999999}});
  const before=f.expected();
  let release;
  const gate=new Promise(resolve=>{release=resolve});
  const writing=f.hold(async()=>{
    await gate;
    f.state.linkedProviders=["chatgpt","manus"];
  });
  const invalidating=f.send(before);
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(f.state.token,"a".repeat(64),"cannot commit outside worker queue");
  release();
  await writing;
  assert.deepEqual(Object.keys(await invalidating),["ok","invalidated"]);
  assert.equal(f.state.token,null);
  assert.equal(f.state.connectionMode,null);
  assert.notEqual(f.state.relayConnectionEpoch,"gen-a");
  assert.deepEqual(Array.from(f.state.linkedProviders),[]);
  assert.notEqual(f.state.providerLinkEpochs.chatgpt,"epoch-a");
  assert.notEqual(f.state.providerLinkEpochs.manus,"epoch-b");
  assert.deepEqual(Object.keys(f.state.providerLinkPendingAttempts),[]);
  assert.deepEqual(Object.keys(f.state.relaySnapshotVersions),[]);
  assert.equal(f.state.pendingConnection.request_id,"still-active",
    "a gateway status failure must not erase a distinct pending approval");
});

test("stale 401 from paired A does not revoke newer paired B",async()=>{
  const f=load();
  const before=f.expected();
  f.state.gateway="https://new.example";
  f.state.token="b".repeat(64);
  f.state.relayConnectionEpoch="gen-b";
  const reply=await f.send(before);
  assert.equal(reply.ok,true);
  assert.equal(reply.invalidated,false);
  assert.equal(f.state.gateway,"https://new.example");
  assert.equal(f.state.token,"b".repeat(64));
  assert.deepEqual(Array.from(f.state.linkedProviders),["chatgpt","manus"]);
});

test("same-token ABA re-pair generation rejects old status",async()=>{
  const f=load();
  const before=f.expected();
  f.state.relayConnectionEpoch="gen-b";
  const reply=await f.send(before);
  assert.equal(reply.invalidated,false);
  assert.equal(f.state.token,"a".repeat(64));
});

test("local no-pairing 401 does not clear local connection or provider consent",async()=>{
  const f=load({gateway:"http://127.0.0.1:8080",connectionMode:"local",
    token:null,relayConnectionEpoch:null});
  const response=await f.send(f.expected());
  assert.equal(response.invalidated,false);
  assert.equal(f.state.connectionMode,"local");
  assert.deepEqual(Array.from(f.state.linkedProviders),["chatgpt","manus"]);
});

test("stale 401 after explicit disconnect cannot revive or rotate current state",async()=>{
  const f=load();const before=f.expected();
  f.state.token=null;
  f.state.connectionMode=null;
  f.state.relayConnectionEpoch="gen-new";
  const reply=await f.send(before);
  assert.equal(reply.invalidated,false);
  assert.equal(f.state.relayConnectionEpoch,"gen-new");
});

test("storage commit failure returns fixed redacted error without clearing current token",async()=>{
  const f=load();
  const before=f.expected();
  f.storage.set=async()=>{throw new Error("secret-error-with-token-"+before.token);};
  const reply=await f.send(before);
  assert.equal(reply.ok,false);
  assert.equal(reply.error,"Paired gateway status invalidation unavailable.");
  assert.ok(!JSON.stringify(reply).includes(before.token));
  assert.equal(f.state.token,before.token);
  assert.deepEqual(Array.from(f.state.linkedProviders),["chatgpt","manus"]);
});

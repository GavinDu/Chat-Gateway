const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function load(values) {
  const state = {
    gateway:"https://gateway.example",
    token:"a".repeat(64),
    connectionMode:"paired",
    linkedProviders:["chatgpt"],
    providerLinkEpochs:{chatgpt:"old-epoch"},
    providerLinkPendingAttempts:{chatgpt:"old-attempt"},
    relaySnapshotVersions:{chatgpt:3},
    ...values
  };
  let rotation = 0;
  let onMessage = null;
  const context = {
    Map, Set, Promise, Date, setTimeout, clearTimeout,
    importScripts(){},
    PROVIDERS:{chatgpt:["chatgpt.com"],manus:["manus.im"]},
    randomHex:()=>("new-epoch-"+(++rotation)),
    normalizeGateway:value=>value,
    isLocalGateway:value=>value.startsWith("http://127.0.0.1:"),
    hasRelayConnection:value=>Boolean(value.gateway && (value.token || value.connectionMode==="local")),
    relayIdentity:value=>value.connectionMode==="local" ? "local" : value.token,
    chrome:{
      cookies:{onChanged:{addListener(){}}},
      permissions:{contains:async()=>true},
      runtime:{
        onInstalled:{addListener(){}}, onStartup:{addListener(){}},
        onMessage:{addListener(fn){onMessage=fn;}}
      },
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
    "\nglobalThis.disconnectPaired=disconnectPairedRelay; globalThis.holdConsent=queueConsentMutation;",context);
  const expected = () => ({
    gateway:state.gateway,token:state.token,connectionMode:state.connectionMode,
    pendingRequest:state.pendingConnection && {
      request_id:state.pendingConnection.request_id,
      expires:state.pendingConnection.expires
    }
  });
  const send=message=>new Promise(resolve=>{
    assert.equal(onMessage(message,{},resolve),true);
  });
  return {state,disconnect:context.disconnectPaired,hold:context.holdConsent,expected,send};
}

test("paired Unpair is serialized after an in-flight provider consent writer",async()=>{
  const fixture=load({pendingConnection:{gateway:"https://gateway.example",request_id:"b".repeat(64),expires:10000}});
  const before=fixture.expected();
  let release;
  const gate=new Promise(resolve=>{release=resolve;});
  const writer=fixture.hold(async()=>{
    await gate;
    fixture.state.linkedProviders=["chatgpt","manus"];
    fixture.state.providerLinkEpochs={chatgpt:"old-epoch",manus:"other-epoch"};
  });
  const disconnect=fixture.disconnect(before);
  await new Promise(resolve=>setImmediate(resolve));
  release();
  await writer;
  const result=await disconnect;
  assert.equal(result.clearedPending,true);
  assert.equal(fixture.state.token,null);
  assert.deepEqual(Array.from(fixture.state.linkedProviders),[]);
  assert.notEqual(fixture.state.providerLinkEpochs.chatgpt,"old-epoch");
  assert.notEqual(fixture.state.providerLinkEpochs.manus,"other-epoch");
  assert.deepEqual(Object.keys(fixture.state.providerLinkPendingAttempts),[]);
  assert.deepEqual(Object.keys(fixture.state.relaySnapshotVersions),[]);
  assert.equal(fixture.state.pendingConnection,null);
});

test("late paired Unpair cannot erase a re-paired install",async()=>{
  const fixture=load();
  const before=fixture.expected();
  fixture.state.gateway="https://new.example";
  fixture.state.token="c".repeat(64);
  fixture.state.linkedProviders=["manus"];
  await assert.rejects(fixture.disconnect(before),/changed/);
  assert.equal(fixture.state.token,"c".repeat(64));
  assert.deepEqual(Array.from(fixture.state.linkedProviders),["manus"]);
});

test("paired Unpair rejects local no-pairing mode and http gateway",async()=>{
  const fixture=load({gateway:"http://127.0.0.1:8080",token:null,connectionMode:"local"});
  const before=fixture.expected();
  await assert.rejects(fixture.disconnect(before),/changed/);
  assert.equal(fixture.state.connectionMode,"local");
  assert.equal(fixture.state.localDisconnected,undefined);
});

test("newer pending approval is preserved and cannot be overwritten by an old Unpair callback",async()=>{
  const fixture=load({pendingConnection:{gateway:"https://gateway.example",request_id:"c".repeat(64),expires:10000}});
  const expected=fixture.expected();
  fixture.state.pendingConnection={gateway:"https://gateway.example",request_id:"d".repeat(64),expires:12000};
  const result=await fixture.disconnect(expected);
  assert.equal(result.clearedPending,false);
  assert.equal(fixture.state.pendingConnection.request_id,"d".repeat(64));
  assert.equal(fixture.state.token,null);
});

test("worker message returns no token and rejects stale explicit Unpair identity",async()=>{
  const fixture=load();
  const expected=fixture.expected();
  const ok=await fixture.send({type:"disconnect-paired-relay",connection:expected});
  assert.deepEqual(Object.keys(ok),["ok"]);
  assert.equal(ok.ok,true);
  assert.equal(fixture.state.token,null);
  const rejected=await fixture.send({type:"disconnect-paired-relay",connection:expected});
  assert.equal(rejected.ok,false);
  assert.ok(!JSON.stringify(rejected).includes("a".repeat(64)));
});

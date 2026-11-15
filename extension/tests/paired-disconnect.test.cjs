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
    "\nglobalThis.disconnectPaired=disconnectPairedRelay; globalThis.connectPaired=connectPairedRelay; globalThis.holdConsent=queueConsentMutation;",context);
  const expected = () => ({
    gateway:state.gateway,token:state.token,connectionMode:state.connectionMode,
    relayConnectionEpoch:state.relayConnectionEpoch,
    localDisconnected:state.localDisconnected,
    pendingRequest:state.pendingConnection && {
      request_id:state.pendingConnection.request_id,
      expires:state.pendingConnection.expires
    }
  });
  const send=message=>new Promise(resolve=>{
    assert.equal(onMessage(message,{},resolve),true);
  });
  return {state,disconnect:context.disconnectPaired,connect:context.connectPaired,
    hold:context.holdConsent,expected,send};
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

test("remote Pair is committed behind consent writer and rotates the generation",async()=>{
  const fixture=load({relayConnectionEpoch:"first-gen"});
  const before=fixture.expected();
  let release;
  const gate=new Promise(resolve=>{release=resolve;});
  const earlier=fixture.hold(async()=>{
    await gate;
    fixture.state.linkedProviders=["chatgpt","manus"];
  });
  const pending=fixture.connect("https://replacement.example","d".repeat(64),before);
  await new Promise(resolve=>setImmediate(resolve));
  release();
  await earlier;
  await pending;
  assert.equal(fixture.state.gateway,"https://replacement.example");
  assert.equal(fixture.state.token,"d".repeat(64));
  assert.equal(fixture.state.connectionMode,"paired");
  assert.deepEqual(Array.from(fixture.state.linkedProviders),[]);
  assert.notEqual(fixture.state.relayConnectionEpoch,"first-gen");
  assert.deepEqual(Object.keys(fixture.state.providerLinkPendingAttempts),[]);
  assert.equal(fixture.state.pendingConnection,null);
});

test("stale Pair after explicit Unpair cannot resurrect an install through ABA",async()=>{
  const fixture=load({relayConnectionEpoch:"old-gen"});
  const before=fixture.expected();
  await fixture.disconnect(before);
  assert.equal(fixture.state.token,null);
  assert.notEqual(fixture.state.relayConnectionEpoch,"old-gen");
  await assert.rejects(fixture.connect("https://gateway.example","e".repeat(64),before),/changed/);
  assert.equal(fixture.state.token,null);
});

test("a delayed Pair response cannot replace newer remote Pair or local mode",async()=>{
  const fixture=load();
  const before=fixture.expected();
  await fixture.connect("https://new.example","e".repeat(64),before);
  await assert.rejects(fixture.connect("https://old.example","f".repeat(64),before),/changed/);
  assert.equal(fixture.state.token,"e".repeat(64));
  assert.equal(fixture.state.gateway,"https://new.example");

  const local=load({gateway:"http://127.0.0.1:8080",connectionMode:"local",token:null});
  const old=local.expected();
  local.state.localDisconnected=true;
  await assert.rejects(local.connect("https://new.example","f".repeat(64),old),/changed/);
  assert.equal(local.state.connectionMode,"local");
});

test("invalid remote Pair origin or token fails closed",async()=>{
  const fixture=load();
  const before=fixture.expected();
  for(const [host,token] of [
    ["http://private.example","c".repeat(64)],
    ["https://private.example.attacker.invalid/path","c".repeat(64)],
    ["https://gateway.example","not-a-token"],
  ]) {
    await assert.rejects(fixture.connect(host,token,before));
    assert.equal(fixture.state.token,"a".repeat(64));
  }
});

test("remote Pair worker acknowledgement is redacted and rejects stale identity",async()=>{
  const fixture=load();
  const expected=fixture.expected();
  const ok=await fixture.send({
    type:"connect-paired-relay",gateway:"https://new.example",
    token:"d".repeat(64),expected
  });
  assert.deepEqual(Object.keys(ok),["ok"]);
  assert.equal(ok.ok,true);
  const stale=await fixture.send({
    type:"connect-paired-relay",gateway:"https://old.example",
    token:"e".repeat(64),expected
  });
  assert.equal(stale.ok,false);
  assert.ok(!JSON.stringify(stale).includes("d".repeat(64)));
});

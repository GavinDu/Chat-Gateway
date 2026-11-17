const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

const gateway = "http://127.0.0.1:8080";
const requestId = "a".repeat(64);
const pollSecret = "b".repeat(64);

function fixture(initial = {}) {
  const state = {gateway, linkedProviders:[], ...initial};
  let listener;
  let counter = 0;
  let fetchResponse = async () => ({status:202,ok:true,json:async()=>({pending:true})});
  const calls = [];
  const context = {
    Map, Set, Promise, Date, AbortController, setTimeout, clearTimeout,
    importScripts(){},
    PROVIDERS:{chatgpt:["chatgpt.com"],manus:["manus.im"]},
    randomHex:()=>(++counter).toString(16).padStart(32,"0"),
    normalizeGateway:value=>new URL(value).origin,
    isLocalGateway:value=>typeof value==="string" && (
      value.startsWith("http://127.0.0.1:") || value.startsWith("http://localhost:")
    ),
    hasRelayConnection:value=>Boolean(value.gateway && (value.token || value.connectionMode==="local")),
    relayIdentity:value=>value.connectionMode==="local" ? "local" : value.token,
    fetch:async (...args)=>{calls.push(args);return fetchResponse(...args);},
    chrome:{
      runtime:{onMessage:{addListener(callback){listener=callback;}},
        onInstalled:{addListener(){}},onStartup:{addListener(){}}},
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
    "\nglobalThis.approvalApi={reservePendingApproval,installPendingApproval,releasePendingApproval,clearPendingApprovalIfCurrent,pollConnection,connectLocalRelay,disconnectLocalRelay,queueConsentMutation};",context);
  const api=context.approvalApi;
  const send=message=>new Promise(resolve=>{
    assert.equal(listener(message,{},resolve),true);
  });
  return {state,api,send,calls,setResponse:fn=>{fetchResponse=fn;},storage:context.chrome.storage.local};
}

test("pending approval reservation and install are exclusively worker-owned",async()=>{
  const f=fixture();
  const generation=await f.api.reservePendingApproval(gateway);
  assert.match(generation,/^[a-f0-9]{32}$/);
  assert.equal(f.state.pendingConnection,undefined);
  const pending=await f.api.installPendingApproval(gateway,generation,requestId,pollSecret);
  assert.equal(pending.request_id,requestId);
  assert.equal(pending.poll_secret,undefined,"popup receives no polling capability");
  assert.equal(f.state.pendingConnection.poll_secret,pollSecret);
  assert.equal(f.state.pendingConnection.approvalGeneration,generation);
  assert.equal(f.state.pendingApprovalRequest,null);
});

test("a second popup cannot steal an active reservation or duplicate live approval",async()=>{
  const f=fixture();
  const generation=await f.api.reservePendingApproval(gateway);
  await assert.rejects(f.api.reservePendingApproval(gateway),/in progress/);
  await f.api.installPendingApproval(gateway,generation,requestId,pollSecret);
  await assert.rejects(f.api.reservePendingApproval(gateway),/existing approval/);
  assert.equal(f.state.pendingConnection.approvalGeneration,generation);
});

test("old response cannot install after local connect-disconnect to same URL (ABA)",async()=>{
  const f=fixture();
  const generation=await f.api.reservePendingApproval(gateway);
  await f.api.connectLocalRelay(gateway,true);
  const connectedEpoch=f.state.relayConnectionEpoch;
  await f.api.disconnectLocalRelay({gateway,token:f.state.token,connectionMode:"local"});
  assert.notEqual(f.state.relayConnectionEpoch,connectedEpoch);
  await assert.rejects(f.api.installPendingApproval(gateway,generation,requestId,pollSecret),/changed/);
  assert.equal(f.state.pendingConnection,undefined);
});

test("a stale cleanup cannot erase a renewed approval, even if ID and secret are reused",async()=>{
  const f=fixture();
  const first=await f.api.reservePendingApproval(gateway);
  await f.api.installPendingApproval(gateway,first,requestId,pollSecret);
  const old={...f.state.pendingConnection};
  assert.equal(await f.api.clearPendingApprovalIfCurrent(old),true);
  const second=await f.api.reservePendingApproval(gateway);
  await f.api.installPendingApproval(gateway,second,requestId,pollSecret);
  assert.equal(await f.api.clearPendingApprovalIfCurrent(old),false);
  assert.equal(f.state.pendingConnection.approvalGeneration,second);
  assert.notEqual(second,first);
});

test("410 from stale polling request cannot remove newly installed B",async()=>{
  const f=fixture();
  const first=await f.api.reservePendingApproval(gateway);
  await f.api.installPendingApproval(gateway,first,requestId,pollSecret);
  let release;
  const gate=new Promise(resolve=>{release=resolve;});
  f.setResponse(async()=>{await gate;return {status:410,ok:false,json:async()=>({})};});
  const polling=f.api.pollConnection();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(f.calls.length,1);
  await f.api.clearPendingApprovalIfCurrent({...f.state.pendingConnection});
  const second=await f.api.reservePendingApproval(gateway);
  await f.api.installPendingApproval(gateway,second,"c".repeat(64),"d".repeat(64));
  release();
  await assert.rejects(polling,/unavailable/);
  assert.equal(f.state.pendingConnection.approvalGeneration,second);
});

test("worker restart can complete a persisted reservation without trusting popup storage",async()=>{
  const first=fixture();
  const generation=await first.api.reservePendingApproval(gateway);
  const resumed=fixture(first.state);
  const pending=await resumed.api.installPendingApproval(gateway,generation,requestId,pollSecret);
  assert.equal(pending.request_id,requestId);
  assert.equal(resumed.state.pendingConnection.approvalGeneration,generation);
});

test("expired or released reservation refuses delayed network response",async()=>{
  const f=fixture();
  const generation=await f.api.reservePendingApproval(gateway);
  f.state.pendingApprovalRequest.expires=0;
  await assert.rejects(f.api.installPendingApproval(gateway,generation,requestId,pollSecret),/changed/);
  const newer=await f.api.reservePendingApproval(gateway);
  assert.notEqual(newer,generation);
  assert.equal(await f.api.releasePendingApproval(generation),false);
  assert.equal(f.state.pendingApprovalRequest.generation,newer);
  assert.equal(await f.api.releasePendingApproval(newer),true);
  assert.equal(f.state.pendingApprovalRequest,null);
});

test("storage write failure does not publish a partial approved connection",async()=>{
  const f=fixture();
  const generation=await f.api.reservePendingApproval(gateway);
  const set=f.storage.set;
  f.storage.set=async patch=>{
    if(patch.pendingConnection) throw Error("sensitive-storage-failure-"+pollSecret);
    return set(patch);
  };
  const result=await f.send({type:"install-pending-approval",gateway,generation,
    request_id:requestId,poll_secret:pollSecret});
  assert.equal(result.ok,false);
  assert.ok(!JSON.stringify(result).includes(pollSecret));
  assert.equal(f.state.pendingConnection,undefined);
  assert.equal(f.state.pendingApprovalRequest.generation,generation);
});

test("accepted approval rotates connection epoch and preserves latest same-gateway consent",async()=>{
  const f=fixture({linkedProviders:["manus"],relayConnectionEpoch:"old-generation"});
  const generation=await f.api.reservePendingApproval(gateway);
  await f.api.installPendingApproval(gateway,generation,requestId,pollSecret);
  f.setResponse(async()=>({status:200,ok:true,json:async()=>({token:"e".repeat(64)})}));
  await f.api.pollConnection();
  assert.equal(f.state.pendingConnection,undefined);
  assert.equal(f.state.token,"e".repeat(64));
  assert.equal(f.state.connectionMode,"paired");
  assert.notEqual(f.state.relayConnectionEpoch,"old-generation");
  assert.deepEqual(Array.from(f.state.linkedProviders),["manus"]);
});

test("local no-pairing session cannot be replaced by legacy approval reservation",async()=>{
  const f=fixture({connectionMode:"local",token:null,linkedProviders:["manus"]});
  const response=await f.send({type:"reserve-pending-approval",gateway});
  assert.equal(response.ok,false);
  assert.equal(f.state.connectionMode,"local");
  assert.equal(f.state.pendingApprovalRequest,undefined);
  assert.deepEqual(Array.from(f.state.linkedProviders),["manus"]);
});

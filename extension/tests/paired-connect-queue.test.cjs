const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");

function fixture(extra = {}) {
  const state = {
    gateway:"https://old.example",
    token:"a".repeat(64),
    connectionMode:"paired",
    linkedProviders:["chatgpt"],
    providerLinkEpochs:{chatgpt:"old-epoch"},
    providerLinkPendingAttempts:{chatgpt:"pending-a"},
    relaySnapshotVersions:{chatgpt:2},
    ...extra
  };
  let listener, canUseOrigin = true;
  const context = {
    Map, Set, Promise, Date, setTimeout, clearTimeout,
    importScripts() {},
    PROVIDERS:{chatgpt:["chatgpt.com"]},
    randomHex:()=>"test-epoch",
    normalizeGateway(value) {
      const u = new URL(value);
      if (u.username || u.password || u.pathname !== "/" || u.search || u.hash ||
          !["http:","https:"].includes(u.protocol)) throw new Error("Invalid origin");
      return u.origin;
    },
    isLocalGateway:gateway=>typeof gateway==="string" && gateway.startsWith("http://127.0.0.1:"),
    hasRelayConnection:s=>Boolean(s.gateway && (s.token || s.connectionMode==="local")),
    relayIdentity:s=>s.connectionMode==="local" ? "local" : s.token,
    chrome:{
      permissions:{contains:async()=>canUseOrigin},
      cookies:{onChanged:{addListener(){}}},
      runtime:{
        getURL:p=>"chrome-extension://fixture/"+p,
        onInstalled:{addListener(){}},onStartup:{addListener(){}},
        onMessage:{addListener(fn){listener=fn;}}
      },
      alarms:{onAlarm:{addListener(){}},clear:async()=>{}},
      storage:{local:{
        get:async keys=>Object.fromEntries((Array.isArray(keys)?keys:[keys]).map(k=>[k,state[k]])),
        set:async patch=>Object.assign(state,patch),
        remove:async keys=>{for(const k of Array.isArray(keys)?keys:[keys])delete state[k];}
      }}
    }
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname,"..","background.js"),"utf8")+
    "\nglobalThis.holdConsent=queueConsentMutation;",context);
  const expected=()=>({
    gateway:state.gateway,token:state.token,connectionMode:state.connectionMode,
    pendingConnection:state.pendingConnection
  });
  const connect=(gateway,token,old=expected(),sender={url:"chrome-extension://fixture/popup.html"})=>
    new Promise(resolve=>listener({type:"connect-paired-relay",gateway,token,connection:old},sender,resolve));
  return {state,connect,expected,hold:context.holdConsent,allow:v=>{canUseOrigin=v;}};
}
test("paired Connect serializes after provider consent and resets old epochs",async()=>{
  const f=fixture();
  const old=f.expected();
  let release;
  const gate=new Promise(resolve=>{release=resolve;});
  const worker=f.hold(async()=>{
    await gate;
    f.state.linkedProviders=["chatgpt","manus"];
    f.state.providerLinkEpochs={chatgpt:"old-epoch",manus:"other-epoch"};
  });
  const pending=f.connect("https://next.example","b".repeat(64),old);
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(f.state.gateway,"https://old.example");
  release();
  await worker;
  const result=await pending;
  assert.deepEqual(Object.keys(result),["ok"]);
  assert.equal(result.ok,true);
  assert.equal(f.state.gateway,"https://next.example");
  assert.equal(f.state.token,"b".repeat(64));
  assert.equal(f.state.connectionMode,"paired");
  assert.deepEqual(Array.from(f.state.linkedProviders),[]);
  assert.deepEqual(Object.keys(f.state.providerLinkEpochs),[]);
  assert.deepEqual(Object.keys(f.state.providerLinkPendingAttempts),[]);
  assert.deepEqual(Object.keys(f.state.relaySnapshotVersions),[]);
});
test("stale HTTPS pairing response cannot replace a newer gateway or token",async()=>{
  const f=fixture();
  const old=f.expected();
  f.state.gateway="https://latest.example";
  f.state.token="c".repeat(64);
  const result=await f.connect("https://old.example","b".repeat(64),old);
  assert.equal(result.ok,false);
  assert.equal(f.state.gateway,"https://latest.example");
  assert.equal(f.state.token,"c".repeat(64));
});
test("simultaneous pair attempts cannot both commit from the old identity",async()=>{
  const f=fixture();
  const expected=f.expected();
  const results=await Promise.all([
    f.connect("https://one.example","b".repeat(64),expected),
    f.connect("https://two.example","c".repeat(64),expected)
  ]);
  assert.deepEqual(results.map(r=>r.ok).sort(),[false,true]);
});
test("content-script sender, bad token, unapproved origin and local mode fail closed",async()=>{
  const f=fixture();
  let result=await f.connect("https://next.example","b".repeat(64),f.expected(),
    {url:"https://manus.im/app",tab:{id:1}});
  assert.equal(result.ok,false);
  result=await f.connect("https://next.example","bad",f.expected());
  assert.equal(result.ok,false);
  f.allow(false);
  result=await f.connect("https://next.example","b".repeat(64),f.expected());
  assert.equal(result.ok,false);
  f.allow(true);
  result=await f.connect("http://127.0.0.1:8080","b".repeat(64),f.expected());
  assert.equal(result.ok,false);
  assert.equal(f.state.gateway,"https://old.example");
  assert.equal(f.state.token,"a".repeat(64));
});

test("new pending approval cannot be cleared by a stale remote pair response",async()=>{
  const first={gateway:"https://old.example",request_id:"a".repeat(64),
    poll_secret:"b".repeat(64),expires:10000};
  const f=fixture({pendingConnection:first});
  const expected=f.expected();
  const latest={gateway:"https://old.example",request_id:"c".repeat(64),
    poll_secret:"d".repeat(64),expires:11000};
  f.state.pendingConnection=latest;
  const result=await f.connect("https://next.example","e".repeat(64),expected);
  assert.equal(result.ok,false);
  assert.equal(f.state.pendingConnection.request_id,"c".repeat(64));
  assert.equal(f.state.gateway,"https://old.example");
  assert.equal(f.state.token,"a".repeat(64));
});

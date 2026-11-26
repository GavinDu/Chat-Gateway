const test=require("node:test");
const assert=require("node:assert/strict");
const vm=require("node:vm");
const fs=require("node:fs");
const source=fs.readFileSync("extension/native_account_diagnostics.js","utf8");
const tick=()=>new Promise(resolve=>setImmediate(resolve));
const evidence=(extra={})=>({provider:"chatgpt",context:"chrome_native_diagnostic",authentication:"verified",reason:"first_party_session_confirmed",http_status:200,auth_mode:"session",checked_at:new Date().toISOString(),exported_snapshot_verified:false,...extra});
function fixture(options={}) {
  const owner={gateway:"http://127.0.0.1:8080",token:"PRIVATE",connectionMode:"local",linkedProviders:["chatgpt"],providerLinkEpochs:{chatgpt:"a"}};
  const tab={id:1,url:"https://chatgpt.com/",status:"complete",active:false};
  const calls={execute:0,create:0,remove:[]};let reads=0,permissions=0;
  const timers=new Map();let sequence=0;
  const context={URL,Date,
    hasRelayConnection:s=>!!s.gateway,requiredProviderOrigins:()=>["https://chatgpt.com/*"],
    relayDiagnostics:async()=>({ok:true,gateway:{providers:[{provider:"chatgpt",enabled:options.enabled!==false}]}}),
    probeActiveChatgptAccount:()=>{},
    setTimeout:(f,ms)=>{const id=++sequence;timers.set(id,{f,ms});return id;},clearTimeout:id=>timers.delete(id),
    chrome:{storage:{local:{get:async()=>{reads++;if(options.ownerChange&&reads>1)return {...owner,token:"CHANGED"};return {...owner,linkedProviders:options.unlinked?[]:["chatgpt"]};}}},
      permissions:{contains:async()=>{permissions++;return options.permission!==false&&!(options.revoke&&permissions>1);}},
      tabs:{query:async()=>options.noTab?[]:[{...tab}],get:async()=>({...tab,...(options.changedTab?{url:"https://chatgpt.com/c/private"}:{}),...(options.adopted?{active:true}:{})}),
        create:async spec=>{calls.create++;assert.equal(spec.url,"https://chatgpt.com/");assert.equal(spec.active,false);if(options.delayedCreate)return options.delayedCreate;return {...tab};},remove:async id=>calls.remove.push(id)},
      scripting:{executeScript:async spec=>{calls.execute++;assert.equal(spec.world,"MAIN");assert.equal(spec.target.allFrames,false);assert.deepEqual(Array.from(spec.args),["chrome_native_diagnostic"]);if(options.execute)return options.execute();return [{frameId:0,result:evidence()}];}}
    }};
  vm.createContext(context);vm.runInContext(source,context);
  return {calls,timers,run:()=>context.nativeChatgptAccountDiagnostic(options.lease),project:(r,now)=>context.projectNativeChatgptAccount(r,now)};
}
test("projection accepts only fresh exact reason/status/mode pairs and returns no account data",()=>{
  const f=fixture();
  for(const r of [evidence(),evidence({reason:"bearer_account_confirmed",auth_mode:"bearer"}),evidence({authentication:"unauthenticated",reason:"provider_rejected_session",http_status:401,auth_mode:"unknown"}),evidence({authentication:"unknown",reason:"session_reported_error",auth_mode:"unknown"})]){
    const out=f.project({...r,token:"PRIVATE",user:{id:"PRIVATE"},url:"PRIVATE"});
    assert.equal(out.reason,r.reason);assert.equal(Object.keys(out).length,7);assert.doesNotMatch(JSON.stringify(out),/PRIVATE|checked_at/);
  }
  for(const r of [null,[],evidence({checked_at:"2000-01-01T00:00:00.000Z"}),evidence({reason:"PRIVATE"}),evidence({auth_mode:"bearer"}),evidence({http_status:201}),evidence({exported_snapshot_verified:true}),evidence({authentication:"unknown",auth_mode:"bearer"}),evidence({authentication:"unauthenticated",http_status:200,reason:"provider_rejected_session",auth_mode:"unknown"})]){
    assert.equal(f.project(r).reason,"verification_unavailable");
  }
});
test("native account read reuses an existing official tab without creating or closing it",async()=>{
  const f=fixture();assert.equal((await f.run()).authentication,"verified");assert.equal(f.calls.execute,1);assert.equal(f.calls.create,0);assert.deepEqual(f.calls.remove,[]);assert.equal(f.timers.size,0);
});
test("consent, permission, enabled state and bridge lease fence all injection",async()=>{
  for(const [options,reason] of [[{unlinked:true},"not_linked"],[{permission:false},"permission_missing"],[{enabled:false},"provider_disabled"],[{ownerChange:true},"source_changed"],[{revoke:true},"permission_missing"],[{lease:async()=>false},"source_changed"]]){
    const f=fixture(options);assert.equal((await f.run()).reason,reason);assert.equal(f.calls.execute,0);assert.equal(f.calls.create,0);
  }
});
test("changed document after injection discards otherwise verified proof",async()=>{
  const f=fixture({changedTab:true});assert.equal((await f.run()).reason,"source_changed");assert.deepEqual(f.calls.remove,[]);
});
test("temporary inactive tab is closed, but a user-adopted tab is preserved",async()=>{
  for(const adopted of [false,true]){
    const f=fixture({noTab:true,adopted});assert.equal((await f.run()).authentication,"verified");assert.equal(f.calls.create,1);assert.deepEqual(f.calls.remove,adopted?[]:[1]);assert.equal(f.timers.size,0);
  }
});
test("deadline fences late tab creation and performs owned cleanup without injection",async()=>{
  let resolve;const delayedCreate=new Promise(r=>resolve=r);const f=fixture({noTab:true,delayedCreate});const pending=f.run();await tick();
  const deadline=[...f.timers.values()].find(t=>t.ms===35000);assert.ok(deadline);deadline.f();assert.equal((await pending).reason,"verification_timeout");
  resolve({id:1,url:"https://chatgpt.com/",status:"complete",active:false});await tick();assert.equal(f.calls.execute,0);assert.deepEqual(f.calls.remove,[1]);assert.equal(f.timers.size,0);
});
test("unrecognized frame results and exceptions cannot prove authentication",async()=>{
  for(const execute of [async()=>[{frameId:1,result:evidence()}],async()=>[{frameId:0,result:evidence()},{frameId:1,result:evidence()}],async()=>{throw Error("PRIVATE");}]){
    const f=fixture({execute});const r=await f.run();assert.equal(r.reason,"verification_unavailable");assert.doesNotMatch(JSON.stringify(r),/PRIVATE/);
  }
});

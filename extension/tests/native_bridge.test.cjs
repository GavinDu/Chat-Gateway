const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");
const source = fs.readFileSync(path.join(__dirname,"../native_bridge.js"),"utf8");
const tick = () => new Promise(resolve => setImmediate(resolve));
function event() {
  const listeners = [];
  return {addListener: fn => listeners.push(fn), fire: arg => listeners.forEach(fn => fn(arg))};
}
async function fixture(granted = true, unavailableAttempts = 0) {
  const incoming = event(), disconnect = event(), removed = event(), added = event(), alarms = event();
  const sent = [], saved = {}, calls = {connect:0,reload:0};
  const port = {onMessage:incoming,onDisconnect:disconnect,postMessage:value=>sent.push(JSON.parse(JSON.stringify(value))),disconnect:()=>disconnect.fire()};
  const tasks = new Map();
  const sandbox = {
    randomHex: () => "a".repeat(32), PROVIDERS:{chatgpt:[],metaai:[]},
    providerTasks:tasks,pendingConsentMutations:0,scheduledProviderRefresh:null,connectionPoll:null,
    relayDiagnostics:async()=>({ok:true, extension:{connected:true,connection_mode:"local",linked_providers:["chatgpt"],provider_sync:{chatgpt:{state:"uploaded",issue:"secret-token",stale:false}},permissions:{providers:{chatgpt:true}}},gateway:{providers:[{provider:"chatgpt",enabled:true,linked:true,authentication:"verified",availability:"ready",token:"SECRET",url:"https://private.invalid"}]}}),
    chrome:{
      permissions:{contains:async()=>granted,onAdded:added,onRemoved:removed},
      runtime:{connectNative:()=>{calls.connect++;if(calls.connect <= unavailableAttempts) throw new Error("host missing");return port;},getManifest:()=>({version:"0.3.0"}),reload:()=>calls.reload++,onStartup:event(),onInstalled:event()},
      storage:{local:{get:async()=>({...saved}),set:async obj=>Object.assign(saved,obj),remove:async key=>delete saved[key]}},
      alarms:{create:async()=>{},clear:async()=>{},onAlarm:alarms}
    }
  };
  vm.runInNewContext(source,sandbox);
  await tick();
  async function send(command,id="b".repeat(32),extra={}) {
    incoming.fire({v:1,id,command,...extra}); await tick();
    return sent.at(-1);
  }
  return {send,sent,saved,calls,tasks,disable(){granted=false;removed.fire({permissions:["nativeMessaging"]});},alarm(){alarms.fire({name:"native-bridge-reconnect"});},reconnect(){disconnect.fire();alarms.fire({name:"native-bridge-reconnect"});}};
}
test("permission absent: no host connect",async()=>{
  const f=await fixture(false); assert.equal(f.calls.connect,0);
});
test("diagnostic projection drops unknown values and credentials",async()=>{
  const f=await fixture();
  const r=await f.send("diagnostics");
  assert.equal(r.ok,true); assert.equal(r.result.providers[0].authentication,"verified");
  assert.equal(r.result.providers[0].sync_issue,"unknown");
  assert.equal(r.result.providers[1].saved_session,null);
  assert.doesNotMatch(JSON.stringify(r),/SECRET|secret-token|private.invalid/);
});
test("unknown and malformed commands never reload",async()=>{
  const f=await fixture();
  assert.equal((await f.send("arbitrary_shell")).error,"unsupported_command");
  await f.send("reload_extension","invalid");
  await f.send("reload_extension","b".repeat(32),{payload:"bad"});
  assert.equal(f.sent.length,1); assert.equal(f.calls.reload,0);
});
test("reload requires acknowledgement and matching commit; active tasks block",async()=>{
  const f=await fixture();
  assert.equal((await f.send("reload_commit")).ok,false);
  f.tasks.set("chatgpt",true);
  assert.equal((await f.send("reload_extension")).error,"busy");
  f.tasks.clear();
  assert.equal((await f.send("reload_extension")).result.stage,"accepted");
  assert.equal(f.calls.reload,0);
  assert.equal((await f.send("reload_extension")).error,"busy");
  await f.send("reload_commit");
  assert.equal(f.calls.reload,1);
  assert.equal(f.saved.nativeBridgeReloadReceipt,"b".repeat(32));
  await f.send("reload_commit"); assert.equal(f.calls.reload,1);
});
test("disconnect and permission removal fence stale commands",async()=>{
  const f=await fixture(); await f.send("reload_extension");
  f.disable(); await f.send("reload_commit");
  assert.equal(f.calls.reload,0);
  f.reconnect(); await tick(); assert.equal(f.calls.connect,1);
});

test("missing host retries on alarm without opening duplicate native ports",async()=>{
  const f=await fixture(true,1);
  assert.equal(f.calls.connect,1);
  f.alarm(); f.alarm(); f.alarm(); await tick();
  assert.equal(f.calls.connect,2);
  assert.equal((await f.send("ping")).ok,true);
  f.alarm(); await tick(); assert.equal(f.calls.connect,2);
});
test("host disconnect invalidates prepared reload and reconnects",async()=>{
  const f=await fixture();
  assert.equal((await f.send("reload_extension")).result.stage,"accepted");
  f.reconnect(); await tick(); assert.equal(f.calls.connect,2);
  assert.equal((await f.send("reload_commit")).error,"reload_not_prepared");
  assert.equal(f.calls.reload,0);
});

const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
function load(status, data, expires = Date.now() + 300000, beforeResponse = null) {
  const values = {pendingConnection:{gateway:'http://127.0.0.1:8080',request_id:'a'.repeat(64),poll_secret:'b'.repeat(64),expires}};
  const calls = [];
  const context = {Map, Set, Promise, Date, normalizeGateway:v=>v, PROVIDERS:{}, importScripts(){},
    hasRelayConnection: state => Boolean(state.gateway && (state.token || state.connectionMode === "local")),
    relayIdentity: state => state.connectionMode === "local" ? "local" : state.token, setTimeout, clearTimeout,
    fetch:async (url, options)=>{calls.push({url,options});if(beforeResponse)await beforeResponse();return {status,ok:status===200||status===202,json:async()=>data};},
    chrome:{cookies:{onChanged:{addListener(){}}},permissions:{contains:async()=>true},
      runtime:{onMessage:{addListener(){}},onInstalled:{addListener(){}},onStartup:{addListener(){}}},
      alarms:{onAlarm:{addListener(){}},clear:async()=>{}},
      storage:{local:{get:async()=>({...values}),set:async value=>Object.assign(values,value),remove:async key=>{delete values[key];}}}}};
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(require.resolve('../background.js'),'utf8')+'\nglobalThis.poll = pollConnection;', context);
  return {values,calls,poll:context.poll};
}
test('pending approval polling refuses redirects that could leak the secret',async()=>{
  const api=load(202,{pending:true});
  await api.poll();
  assert.equal(api.calls.length,1);
  assert.equal(api.calls[0].options.redirect,'error');
});
test('pending local approval never stores a token',async()=>{
  const api=load(202,{pending:true});await api.poll();assert.equal(api.values.token,undefined);assert.ok(api.values.pendingConnection);
});
test('approved exchange stores connection once and removes polling capability',async()=>{
  const api=load(200,{token:'c'.repeat(64)});await api.poll();assert.equal(api.values.token,'c'.repeat(64));assert.equal(api.values.pendingConnection,undefined);await api.poll();assert.equal(api.calls.length,1);
});
test('expired request never reaches the gateway',async()=>{
  const api=load(200,{token:'c'.repeat(64)},0);await assert.rejects(api.poll,/expired/);assert.equal(api.calls.length,0);assert.equal(api.values.pendingConnection,undefined);
});
test('malformed token is never persisted',async()=>{
  const api=load(200,{token:'bad'});await assert.rejects(api.poll,/Invalid connection token/);assert.equal(api.values.token,undefined);
});

test('reapproval keeps provider consent only for the same gateway',async()=>{
  const same=load(200,{token:'c'.repeat(64)});same.values.gateway='http://127.0.0.1:8080';same.values.linkedProviders=['chatgpt'];await same.poll();assert.deepEqual(Array.from(same.values.linkedProviders),['chatgpt']);
  const other=load(200,{token:'c'.repeat(64)});other.values.gateway='https://other.example';other.values.linkedProviders=['chatgpt'];await other.poll();assert.deepEqual(Array.from(other.values.linkedProviders),[]);
});

test('stale 410 poll never clears a replacement pending approval',async()=>{
  let release;
  const barrier=new Promise(resolve=>{release=resolve;});
  const api=load(410,{},Date.now()+300000,()=>barrier);
  const old=api.values.pendingConnection;
  const running=api.poll();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(api.calls.length,1,'the old request must be in flight');
  const replacement={...old,request_id:'d'.repeat(64),poll_secret:'e'.repeat(64)};
  api.values.pendingConnection=replacement;
  release();
  await assert.rejects(running,/Connection approval unavailable/);
  assert.equal(api.values.pendingConnection.request_id,replacement.request_id);
  assert.equal(api.values.pendingConnection.poll_secret,replacement.poll_secret);
});
test('410 for the current approval clears only that request',async()=>{
  const api=load(410,{});
  await assert.rejects(api.poll(),/Connection approval unavailable/);
  assert.equal(api.values.pendingConnection,undefined);
});

test('late successful response cannot exchange a replaced approval secret',async()=>{
  let release;
  const barrier=new Promise(resolve=>{release=resolve;});
  const api=load(200,{token:'c'.repeat(64)},Date.now()+300000,()=>barrier);
  const running=api.poll();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(api.calls.length,1);
  const original=api.values.pendingConnection;
  api.values.pendingConnection={...original,poll_secret:'d'.repeat(64)};
  release();
  await running;
  assert.equal(api.values.token,undefined,'stale success must not persist a new token');
  assert.equal(api.values.pendingConnection.poll_secret,'d'.repeat(64));
});

test('late successful response cannot approve an expired request',async()=>{
  let release;
  const barrier=new Promise(resolve=>{release=resolve;});
  const api=load(200,{token:'c'.repeat(64)},Date.now()+300000,()=>barrier);
  const running=api.poll();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(api.calls.length,1);
  api.values.pendingConnection.expires=Date.now()-1;
  release();
  await assert.rejects(running,/expired/);
  assert.equal(api.values.token,undefined,'expired approval must not become a connection');
  assert.equal(api.values.pendingConnection,undefined);
});

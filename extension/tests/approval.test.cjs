const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
function load(status, data, expires = Date.now() + 300000) {
  const values = {pendingConnection:{gateway:'http://127.0.0.1:8080',request_id:'a'.repeat(64),poll_secret:'b'.repeat(64),expires}};
  const calls = [];
  const context = {Map, Set, Promise, Date, normalizeGateway:v=>v, PROVIDERS:{}, importScripts(){}, setTimeout, clearTimeout,
    fetch:async (url, options)=>{calls.push({url,options});return {status,ok:status===200||status===202,json:async()=>data};},
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

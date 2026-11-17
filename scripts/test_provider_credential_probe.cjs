const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('scripts/provider_credential_probe.js', 'utf8');
async function probe({host='chatgpt.com', protocol='https:', port='', status=200, body={}, failure=false, timeout=false} = {}) {
  let calls=0;
  const output = await vm.runInNewContext(source, {
    location:{hostname:host,protocol,port}, Date, AbortController,
    setTimeout: timeout ? callback => {queueMicrotask(callback);return 1;} : setTimeout,
    clearTimeout: timeout ? () => {} : clearTimeout,
    fetch:async (url, options) => {
      calls++;
      assert.equal(url,'/api/auth/session');
      assert.equal(options.credentials,'include');
      assert.equal(options.redirect,'error');
      assert.equal(options.cache,'no-store');
      if (timeout) {
        await new Promise(resolve=>queueMicrotask(resolve));
        throw new Error('private upstream timeout');
      }
      if (failure) throw new Error('private upstream token');
      return {status,json:async()=>body};
    }
  });
  return {output,calls};
}
test('first-party session proof returns no identity or credential data',async()=>{
  const {output}=await probe({body:{user:{email:'private@example.test'},accessToken:'private bearer',expires:new Date(Date.now()+60000).toISOString()}});
  assert.equal(output.authentication,'verified');
  assert.equal(output.exported_snapshot_verified,false);
  assert(!JSON.stringify(output).includes('private'));
  assert.equal(typeof output.checked_at,'string');
});
test('only definitive unauthenticated responses reject the browser session',async()=>{
  for (const options of [{status:401},{body:{}},{body:{user:null}}]) {
    assert.equal((await probe(options)).output.authentication,'unauthenticated');
  }
});
test('blocks and incomplete, stale or malformed evidence remain unknown',async()=>{
  for (const options of [
    {status:403},{status:429},{status:500},{body:[]},{body:null},
    {body:{error:'private upstream details'}},
    {body:{user:{},accessToken:'private',expires:'2000-01-01T00:00:00Z'}},
    {body:{user:{},accessToken:'private',expires:'invalid'}},
    {body:{user:[],accessToken:'private',expires:new Date(Date.now()+60000).toISOString()}}
  ]) {
    const {output}=await probe(options);
    assert.equal(output.authentication,'unknown');
    assert(!JSON.stringify(output).includes('private'));
  }
});
test('Meta AI never invents authentication from editor or cookie presence',async()=>{
  const {output,calls}=await probe({host:'www.meta.ai'});
  assert.equal(output.provider,'metaai');
  assert.equal(output.authentication,'unknown');
  assert.equal(output.reason,'account_verifier_not_established');
  assert.equal(calls,0);
});
test('foreign, insecure and alternate-port origins are never contacted',async()=>{
  for(const options of [{host:'chatgpt.com.attacker.test'},{host:'meta.ai.attacker.test'},{protocol:'http:'},{port:'8443'}]){
    const {output,calls}=await probe(options);
    assert.equal(output.reason,'unsupported_origin');
    assert.equal(calls,0);
  }
});
test('network errors and deadlines do not accuse credentials or leak errors',async()=>{
  assert.equal((await probe({failure:true})).output.reason,'verification_unavailable');
  assert.equal((await probe({timeout:true})).output.reason,'verification_timeout');
});

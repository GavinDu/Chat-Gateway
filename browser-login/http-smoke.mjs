// Independent real HTTP checks. No browser/provider access, inherited Secrets,
// existing files, raw errors, response bodies or credential output.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, rm, stat } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import net from 'node:net';
import http from 'node:http';

const temp = await mkdtemp('/tmp/cg-http-mvp-');
const binary = resolve(process.argv[2]);
const owner = randomBytes(32).toString('hex');
const chat = randomBytes(32).toString('hex');
const scoped = randomBytes(32).toString('hex');
let child, stage = 'initialization', count = 0;
const delay = ms => new Promise(r => setTimeout(r, ms));
const pass = label => { count++; console.log('PASS ' + label); };
async function until(fn) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) { if (await fn()) return; await delay(50); }
  throw new Error('timeout');
}
async function freePort() {
  const server = net.createServer();
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  await new Promise(r => server.close(r));
  return port;
}
async function stop() {
  if (child?.exitCode === null) {
    child.kill('SIGTERM');
    try { await until(() => child.exitCode !== null); }
    catch { child.kill('SIGKILL'); }
  }
}
const visual = process.argv.includes('--visual');
const port = visual ? 8099 : await freePort();
const base = 'http://127.0.0.1:' + port;
const keys = [
  { name:'owner', scopes:['admin','chat'], secret_hash:createHash('sha256').update(owner).digest('hex') },
  { name:'chat', scopes:['chat'], secret_hash:createHash('sha256').update(chat).digest('hex') },
  { name:'restricted', scopes:['admin'], provider_allowlist:['claude'],
    secret_hash:createHash('sha256').update(scoped).digest('hex') }
];
async function start(name, auth, extra = {}) {
  const state = join(temp, name);
  await mkdir(state, { mode:0o700 });
  const config = join(state, 'config.yaml');
  await writeFile(config, JSON.stringify({auth:{keys:auth},providers:[],
    server:{port,chrome_profile_dir:join(state,'engine')},
    obscura:{storage_dir:join(state,'engine')}}), {mode:0o600});
  child = spawn(binary, ['--config',config], {cwd:state,stdio:'ignore',env:{
    PATH:process.env.PATH, HOME:state, PORT:String(port), RUST_LOG:'off',
    CHAT_GATEWAY_SESSION_STORE:'vault', CHAT_GATEWAY_VAULT_KEY:randomBytes(32).toString('hex'),
    CHAT_GATEWAY_MEMORY_PATH:join(state,'conversations.sqlite3'),
    GLIBC_TUNABLES:'glibc.rtld.optional_static_tls=1048576',
    LD_LIBRARY_PATH:'/nix/store/2ga5nd1m56n5cx2wh8vbf6nrdhqk2f0q-gcc-15.3.0-lib/lib',
    ...extra}});
  return state;
}
const headers = (key = owner) => ({Authorization:'Bearer '+key,Origin:base});
async function rejectedStream(ticket) {
  return new Promise((resolveStatus,reject) => {
    const req = http.request(base+'/login/api/chatgpt/browser/stream',{headers:{
      Origin:base,Connection:'Upgrade',Upgrade:'websocket','Sec-WebSocket-Version':'13',
      'Sec-WebSocket-Key':randomBytes(16).toString('base64'),
      'Sec-WebSocket-Protocol':'cg-login.'+ticket}}, response=>{
      response.resume(); response.on('end',()=>resolveStatus(response.statusCode));
    });
    req.on('upgrade',(_response,socket)=>{socket.destroy();resolveStatus(101);});
    req.on('error',reject);req.end();
  });
}
try {
  stage = 'HTTP startup';
  const state = await start('http',keys);
  await until(async()=>{try{return (await fetch(base+'/health')).status===200;}catch{return false;}});
  const health = await fetch(base+'/health');
  assert.equal(health.status,200);assert(health.headers.get('x-request-id'));
  const ready = await fetch(base+'/ready',{headers:headers()});
  assert.equal(ready.status,503);assert.equal((await ready.json()).reason,'no_enabled_provider');
  const models = await fetch(base+'/v1/models',{headers:headers()});
  assert.equal(models.status,200);assert(Array.isArray((await models.json()).data));
  pass('health 200 + request ID; readiness 503/no_enabled_provider; models 200');
  stage = 'request rejection';
  assert.equal((await fetch(base+'/v1/chat/completions',{method:'POST',body:'{}'})).status,401);
  const invalid = await fetch(base+'/v1/chat/completions',{method:'POST',
    headers:{...headers(),'Content-Type':'application/json'},body:'{'});
  assert.equal(invalid.status,400);assert(invalid.headers.get('x-request-id'));
  pass('unauthorized chat 401 and invalid JSON 400 with request ID');
  const remove = (key = owner, origin = base, provider = 'chatgpt') =>
    fetch(base+'/login/api/'+provider+'/browser/session',
      {method:'DELETE',headers:{Authorization:'Bearer '+key,Origin:origin}});
  assert.equal((await remove('invalid')).status,401);
  assert.equal((await remove(chat)).status,403);
  assert.equal((await remove(scoped)).status,403);
  assert.equal((await remove(owner,'https://evil.invalid')).status,403);
  assert.equal((await fetch(base+'/login/api/chatgpt/browser/session',
    {method:'DELETE',headers:{Authorization:'Bearer '+owner}})).status,403);
  assert.equal((await remove(owner,base,'unknown')).status,400);
  pass('revocation rejects invalid key, non-admin, wrong provider, hostile Origin and unknown provider');
  const issued = await fetch(base+'/login/api/chatgpt/browser/ticket',{method:'POST',headers:headers()});
  assert.equal(issued.status,200);
  const ticket = (await issued.json()).ticket;
  const removed = await remove();
  assert.equal(removed.status,200);
  assert.deepEqual(await removed.json(),{ok:true,removed:false});
  assert.equal(await rejectedStream(ticket),401);
  pass('disabled-provider revocation is offline/idempotent and invalidates issued browser tickets');
  stage = 'SQLite isolation';
  const database = join(state,'conversations.sqlite3');
  assert.equal((await stat(database)).mode & 0o777,0o600);
  const check = spawnSync('python3',['-B','-c',
    'import sqlite3,sys,json; c=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True); print(json.dumps([c.execute("pragma integrity_check").fetchone()[0],c.execute("pragma user_version").fetchone()[0]]))',database],
    {encoding:'utf8',env:{PATH:process.env.PATH}});
  assert.equal(check.status,0);
  const [integrity,version] = JSON.parse(check.stdout);
  assert.equal(integrity,'ok');assert(version>0);
  pass('isolated SQLite integrity/schema and 0600 permissions');
  if (visual) {
    console.log('VISUAL_CHECK_READY port=8099; HTTP-only inspection, not provider E2E');
    await delay(40000);
  }
  await stop();assert.equal(child.exitCode,0);
  pass('native SIGTERM exit 0');
  stage = 'process-local owner authentication';
  await start('owner-env',[],{CHAT_GATEWAY_OWNER_AUTH_FROM_ENV:'1',CHAT_GATEWAY_API_KEY:owner});
  await until(async()=>{try{return (await fetch(base+'/health')).status===200;}catch{return false;}});
  assert.equal((await remove()).status,200);
  assert.equal((await remove('invalid')).status,401);
  await stop();assert.equal(child.exitCode,0);
  pass('gateway hashes runtime owner key internally; authenticated admin API works without hash setting');
  for (const [name,extra] of [
    ['bad-owner',{CHAT_GATEWAY_OWNER_AUTH_FROM_ENV:'1',CHAT_GATEWAY_API_KEY:'too-short'}],
    ['bad-vault',{CHAT_GATEWAY_VAULT_KEY:'not-a-vault-key'}],
    ['missing-vault-key',{CHAT_GATEWAY_VAULT_KEY:undefined}]
  ]) {
    stage = name;
    await start(name,name==='bad-owner'?[]:keys,extra);
    await until(()=>child.exitCode!==null);
    assert.notEqual(child.exitCode,0);
  }
  pass('invalid dummy owner/vault keys and absent vault key fail startup before HTTP bind');
  for (const [name,mode] of [['missing-mode',undefined],['plaintext-mode','file']]) {
    stage = name;
    await start(name,keys,{CHAT_GATEWAY_SESSION_STORE:mode});
    await until(async()=>{try{return (await fetch(base+'/health')).status===200;}catch{return false;}});
    const denied = await fetch(base+'/login/api/chatgpt/browser/ticket',
      {method:'POST',headers:headers()});
    assert.equal(denied.status,503);
    const revoke = await remove();
    assert.equal(revoke.status,503);
    await stop();assert.equal(child.exitCode,0);
  }
  pass('missing/plaintext store mode refuses browser admission and revocation without plaintext fallback');
} catch {
  console.error('FAIL HTTP smoke stage='+stage);process.exitCode=1;
} finally {
  await stop();await rm(temp,{recursive:true,force:true});
  console.log('CLEANUP isolated HTTP fixtures/processes removed; observed groups='+count);
}

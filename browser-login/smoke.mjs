// Real gateway + real sandboxed Chromium + real UI/WebSocket. All provider
// network traffic is intercepted with local fixtures by a test-only node shim.
// No production backend test bypass, existing environment, or profile is used.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, readFile, rm, readdir, stat } from 'node:fs/promises';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import net from 'node:net';
import http from 'node:http';

const root = dirname(fileURLToPath(import.meta.url));
const playwrightModule = join(dirname(createRequire(import.meta.url).resolve('playwright/package.json')), 'index.mjs');
const temp = await mkdtemp('/tmp/cg-embedded-smoke-');
const source = resolve(root, '..');
const results = [];
let gateway, owner, page;
let controlDiagnostics = [];
let stage = 'initialization';
const delay = ms => new Promise(r => setTimeout(r, ms));
async function until(fn, label, timeout = 20000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await fn()) return;
    await delay(100);
  }
  throw new Error('timeout: ' + label);
}
function pass(name) { results.push(name); console.log('PASS ' + name); }
async function fixtureRasterPainted() {
  // Synthetic fixture heading must actually reach the canvas, not merely the
  // WebSocket or URL label. No pixels from real provider pages are inspected.
  return page.locator('#browser-viewport').evaluate(c => {
    if (c.width < 500 || c.height < 200) return false;
    const a = c.getContext('2d').getImageData(20,20,480,80).data;
    let dark = 0;
    for (let i=0;i<a.length;i+=4)
      if (a[i+3]>200 && a[i]<100 && a[i+1]<100 && a[i+2]<100) dark++;
    return dark > 100;
  });
}
async function port() {
  const server = net.createServer();
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const result = server.address().port;
  await new Promise(r => server.close(r));
  return result;
}
async function rawUpgrade(origin, token, provider = 'chatgpt') {
  return new Promise((resolve, reject) => {
    const req = http.request(base + '/login/api/' + provider + '/browser/stream', {
      headers: { Host: new URL(base).host, Origin: origin, Upgrade: 'websocket',
        Connection: 'Upgrade', 'Sec-WebSocket-Key': randomBytes(16).toString('base64'),
        'Sec-WebSocket-Version': '13', 'Sec-WebSocket-Protocol': 'cg-login.' + token }
    });
    req.on('response', res => { resolve(res.statusCode); res.resume(); });
    req.on('upgrade', (res, socket) => { socket.destroy(); resolve(res.statusCode); });
    req.on('error', reject);
    req.end();
  });
}
const key = randomBytes(32).toString('hex');
const chatKey = randomBytes(32).toString('hex');
const restrictedKey = randomBytes(32).toString('hex');
const fixtureVaultKey = randomBytes(32).toString('hex');
let base;
try {
  await mkdir(join(temp, 'bin'), { mode: 0o700 });
  await mkdir(join(temp, 'home'), { mode: 0o700 });
  const loader = join(temp, 'fixture-loader.mjs');
  // The fixture never reads a real page or real profile. Instrumentation writes
  // only booleans/viewport sizes into this freshly owned temporary directory.
  await writeFile(loader, `
import { chromium } from ${JSON.stringify(playwrightModule)};
import { LoginEngine } from ${JSON.stringify(join(root, 'engine.mjs'))};
for (const name of ['init','register','activate','status','dispatch']) {
 const original = LoginEngine.prototype[name];
 LoginEngine.prototype[name] = async function(...args) {
  try { return await original.apply(this,args); }
  catch(error) {
   const location = String(error.stack).match(/engine.mjs:(\\d+)/);
   await appendFile(${JSON.stringify(join(temp, 'events'))},
    'fixture-engine-error-'+name+'-line-'+(location?.[1]||'unknown')+'\\n');
   const category = String(error.message).match(/net::(ERR_[A-Z_]+)/);
   if(category) await appendFile(${JSON.stringify(join(temp, 'events'))},
    'fixture-network-category-'+category[1]+'\\n');
   throw error;
  }
 };
}
import { appendFile } from 'node:fs/promises';
const launch = chromium.launchPersistentContext.bind(chromium);
chromium.launchPersistentContext = async (...args) => {
 const context = await launch(...args);
 await appendFile(${JSON.stringify(join(temp, 'events'))}, 'context-created\\n');
 await appendFile(${JSON.stringify(join(temp, 'events'))},
   'fixture-profile:'+process.env.CHAT_GATEWAY_LOGIN_TMPDIR+'\\n');
 await context.route('**/*', async route => {
  const u = new URL(route.request().url());
  if (u.pathname === '/worker-stop') process.exit(23);
  if (u.pathname === '/redirect-escape') return route.fulfill({status:302,headers:{location:'https://not-approved.invalid/'}});
  if (u.pathname === '/challenge-redirect') return route.fulfill({status:302,headers:{location:'https://not-approved.invalid/'}});
  return route.fulfill({contentType:'text/html',body:\`<!doctype html><html><body style="margin:0;background:#fff;color:#111;font:20px sans-serif;height:2000px">
  <h1 style="margin:20px">Isolated login fixture</h1>
  <input id="fixture-input" aria-label="Fixture input" style="position:absolute;left:40px;top:100px;width:300px;height:50px">
  <button id="fixture-save" style="position:absolute;left:40px;top:200px;width:220px;height:50px" onclick="document.cookie='fixture_session=dummy;Path=/;Secure;SameSite=Lax';document.body.style.background='#ddffdd'">Create fixture session</button>
  <a href="/second" style="position:absolute;left:40px;top:290px">Second fixture page</a>
  <button style="position:absolute;left:40px;top:380px;width:220px;height:50px" onclick="window.open('https://accounts.google.com/popup-child?code=fixture','fixture-login')">Open fixture SSO</button>
  \${u.pathname === '/challenge-parent' ? '<iframe src="https://challenges.cloudflare.com/widget"></iframe>' : ''}
  \${u.pathname === '/challenge-escape-parent' ? '<iframe src="https://challenges.cloudflare.com/challenge-redirect"></iframe>' : ''}
  <script>console.log('fixture-ready');
  document.addEventListener('mousedown',e=>console.log(e.target.tagName==='INPUT'?'fixture-click-input':'fixture-click-other'));
  document.addEventListener('input',()=>{document.body.dataset.typed='yes'; console.log('fixture-input-received');if(location.hostname==='accounts.google.com')console.log('fixture-popup-input')});
  document.addEventListener('keydown',e=>{if(e.key==='Backspace')console.log('fixture-backspace-received')});
  document.addEventListener('scroll',()=>console.log('fixture-scroll-received'));
  if(document.cookie.includes('fixture_session='))console.log('fixture-restored');else console.log('fixture-clean');
  if(location.hostname==='accounts.google.com'&&opener)opener.postMessage('fixture-popup','https://chatgpt.com');
  window.addEventListener('message',e=>{if(e.data==='fixture-popup'&&e.origin==='https://accounts.google.com')console.log('fixture-popup-roundtrip')});
  if(location.hostname==='challenges.cloudflare.com')console.log('fixture-challenge-ready');
  if(location.hostname==='not-approved.invalid')console.log('fixture-escape');
  <\\/script></body></html>\`});
 });
 const attach = p => {
  p.on('console', m => {
   if (/^fixture-(input|backspace|scroll|restored|ready|click|popup|challenge|escape|clean)/.test(m.text()))
    void appendFile(${JSON.stringify(join(temp, 'events'))}, m.text()+'\\n');
  });
 };
 for (const initial of context.pages()) attach(initial);
 context.on('page', attach);
 return context;
};
`);
  await writeFile(join(temp, 'bin', 'node'),
    '#!/bin/sh\nexec ' + JSON.stringify(process.execPath) + ' --import ' +
      JSON.stringify(loader) + ' "$@"\n', { mode: 0o700 });
  const apiKeys = [
    { name: 'test-owner', secret_hash: createHash('sha256').update(key).digest('hex'), scopes: ['admin', 'chat'] },
    { name: 'test-chat-only', secret_hash: createHash('sha256').update(chatKey).digest('hex'), scopes: ['chat'] },
    { name: 'test-restricted', secret_hash: createHash('sha256').update(restrictedKey).digest('hex'),
      scopes: ['admin'], provider_allowlist: ['claude'] }
  ];
  await writeFile(join(temp, 'config.yaml'), JSON.stringify({
    auth: { keys: apiKeys },
    server: { port: 8081, chrome_profile_dir: join(temp, 'engine') },
    providers: [],
    obscura: { storage_dir: join(temp, 'engine') }
  }), { mode: 0o600 });
  const visual = process.argv.includes('--visual');
  const listen = visual && !process.argv.includes('--random-port') ? 8099 : await port();
  base = 'http://127.0.0.1:' + listen;
  const startGateway = () => spawn(resolve(process.argv[2]), ['--config', join(temp, 'config.yaml')], {
    cwd: temp,
    env: { PATH: join(temp, 'bin') + ':' + process.env.PATH,
      HOME: join(temp, 'home'), PORT: String(listen), RUST_LOG: 'off',
      CHAT_GATEWAY_SESSION_STORE: 'vault', CHAT_GATEWAY_VAULT_KEY: fixtureVaultKey,
      CHAT_GATEWAY_MEMORY_PATH: join(temp, 'memory.sqlite3'),
      CHAT_GATEWAY_LEGACY_LOGIN_RELAY: '0',
      CHAT_GATEWAY_CHROMIUM_PATH: '/repl/tools/bin/chromium',
      GLIBC_TUNABLES: 'glibc.rtld.optional_static_tls=1048576',
      LD_LIBRARY_PATH: '/nix/store/2ga5nd1m56n5cx2wh8vbf6nrdhqk2f0q-gcc-15.3.0-lib/lib' },
    stdio: 'ignore'
  });
  gateway = startGateway();
  await until(async () => {
    if (gateway.exitCode !== null) throw new Error('gateway exited before health');
    try { return (await fetch(base + '/health')).status === 200; } catch { return false; }
  }, 'health');
  pass('actual gateway health HTTP 200');
  assert.equal((await fetch(base + '/ready', { headers: { Authorization: 'Bearer ' + key } })).status, 503);
  assert.equal((await fetch(base + '/v1/models', { headers: { Authorization: 'Bearer ' + key } })).status, 200);
  pass('readiness truthfully rejects disabled routing; authenticated model listing works');
  async function ticket(secret = key, provider = 'chatgpt', origin = base) {
    return fetch(base + '/login/api/' + provider + '/browser/ticket', {
      method: 'POST', headers: { Authorization: 'Bearer ' + secret, Origin: origin }
    });
  }
  assert.equal((await ticket('invalid')).status, 401);
  assert.equal((await ticket(chatKey)).status, 403);
  assert.equal((await ticket(restrictedKey)).status, 403);
  assert.equal((await ticket(key, 'chatgpt', 'https://evil.invalid')).status, 403);
  assert.equal((await ticket(key, 'chatgpt', 'null')).status, 403);
  assert.equal((await fetch(base + '/login/api/chatgpt/browser/ticket',
    { method:'POST', headers:{Authorization:'Bearer '+key} })).status, 403);
  assert.equal(await rawUpgrade(base, 'invalid'), 401);
  const bound = await (await ticket()).json();
  assert.equal(await rawUpgrade('https://evil.invalid', bound.ticket), 403);
  assert.equal(await rawUpgrade(base, bound.ticket, 'claude'), 401);
  assert.equal(await rawUpgrade(base, bound.ticket), 401);
  pass('HTTP/WS invalid key, non-admin, provider scope, CSRF, ticket binding and replay rejected');
  stage = 'owner Chromium launch';
  owner = await chromium.launch({
    executablePath: '/repl/tools/bin/chromium', headless: true, chromiumSandbox: true,
    env: { PATH: process.env.PATH, HOME: join(temp, 'home'), LANG: 'C.UTF-8' }
  });
  stage = 'owner browser context';
  const ownerContext = await owner.newContext({ viewport: { width: 1400, height: 1000 } });
  page = await ownerContext.newPage();
  // Owner UI is only allowed to contact the local test gateway.
  await page.route('**/*', route => route.request().url().startsWith(base + '/')
    ? route.continue() : route.abort());
  const messages = [];
  controlDiagnostics = messages;
  page.on('websocket', ws => ws.on('framereceived', event => {
    try {
      const m = JSON.parse(String(event.payload));
      // Record control metadata only; never images or cookie-bearing IPC.
      messages.push({ type: m.type, code: m.code, active: m.active,
        width: m.width, height: m.height, phase: m.phase });
    } catch {}
  }));
  stage = 'owner login shell navigation';
  await page.goto(base + '/login');
  stage = 'owner authorization UI';
  await page.locator('#gateway-authorization').fill(key);
  await page.locator('#authorization-form button[type="submit"]').click();
  await until(() => page.locator('#start-session').isEnabled(), 'owner authentication');
  await page.locator('#start-session').click();
  stage = 'embedded Chromium frame';
  await until(() => messages.some(m => m.type === 'frame'), 'screencast');
  await until(fixtureRasterPainted, 'synthetic fixture pixels painted in canvas');
  await until(() => page.locator('#safe-url').textContent().then(t => t.includes('chatgpt.com')), 'fixture navigation');
  await until(() => readFile(join(temp, 'events'), 'utf8').then(s => s.includes('fixture-ready')), 'fixture JS ready');
  pass('real in-app Chromium viewport: Playwright/CDP -> Rust -> WebSocket -> canvas');
  const canvas = page.locator('#browser-viewport');
  async function clickViewport(x, y) {
    // Mouse coordinates do not auto-scroll like locator.click(). The canvas
    // can extend below the owner's viewport after resize/navigation.
    await canvas.scrollIntoViewIfNeeded();
    const bounds = await canvas.boundingBox();
    const size = await canvas.evaluate(c => ({ width: c.width, height: c.height }));
    const scale = Math.min(bounds.width / size.width, bounds.height / size.height);
    const px = bounds.x + (bounds.width - size.width * scale) / 2 + x * scale;
    const py = bounds.y + (bounds.height - size.height * scale) / 2 + y * scale;
    const viewport = page.viewportSize();
    assert(px >= 0 && py >= 0 && px < viewport.width && py < viewport.height,
      'fixture pointer target must be visible in the owner viewport');
    await page.mouse.click(px, py);
  }
  await clickViewport(100, 125);
  await page.keyboard.type('fixture');
  await page.keyboard.press('Backspace');
  async function events() { return readFile(join(temp, 'events'), 'utf8').catch(() => ''); }
  await until(() => events().then(s => s.includes('fixture-input-received')), 'viewport keyboard');
  await until(() => events().then(s => s.includes('fixture-backspace-received')), 'viewport backspace');
  await page.mouse.wheel(0, 600);
  await until(() => events().then(s => s.includes('fixture-scroll-received')), 'viewport scroll');
  pass('actual mouse, printable keyboard, Backspace and wheel dispatched to embedded page');
  await page.setViewportSize({ width: 1200, height: 900 });
  await until(() => messages.filter(m => m.type === 'frame').some(m => m.width !== 1000), 'resize');
  pass('viewport resize produced screencast frames');
  await page.locator('#address').fill('https://chatgpt.com/second');
  await page.locator('#address-go').click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/second')), 'navigate');
  await page.locator('#go-back').click();
  await until(() => page.locator('#safe-url').textContent().then(t => !t.endsWith('/second')), 'back');
  await page.locator('#go-forward').click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/second')), 'forward');
  await page.locator('#reload-page').click();
  pass('address navigation, back, forward and refresh');
  await page.locator('#address').fill('https://not-approved.invalid/');
  await page.locator('#address-go').click();
  await until(() => messages.some(m => m.code === 'navigation_blocked'), 'navigation rejection');
  assert((await page.locator('#safe-url').textContent()).includes('chatgpt.com'));
  pass('unapproved navigation rejected without leaving provider');
  const redirectMark = messages.length;
  await page.locator('#address').fill('https://chatgpt.com/redirect-escape');
  await page.locator('#address-go').click();
  await until(() => messages.slice(redirectMark).some(m => m.code === 'navigation_blocked'),
    'automatic redirect allowlist');
  await page.locator('#address').fill('https://chatgpt.com/second');
  await page.locator('#address-go').click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/second')), 'return to allowed fixture');
  pass('automatic redirect to unapproved domain blocked before loading');
  await page.locator('#address').fill('https://chatgpt.com/popup-parent');
  await page.locator('#address-go').click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/popup-parent')), 'popup parent');
  await clickViewport(100, 405);
  await until(() => page.getByRole('tab').count().then(n => n === 2), 'embedded popup tabs');
  await until(() => page.locator('#safe-url').textContent().then(t => t.includes('accounts.google.com')), 'popup active viewport');
  await clickViewport(100, 125);
  await page.keyboard.type('fixture-popup');
  await until(() => events().then(s => s.includes('fixture-popup-input')), 'popup input dispatch');
  await page.getByRole('tab', { name: /chatgpt.com/ }).click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/popup-parent')), 'switch back to provider');
  await until(() => events().then(s => s.includes('fixture-popup-roundtrip')), 'popup opener postMessage');
  pass('SSO popup remains in protected app viewport with tab switching, input and opener messaging');
  await page.locator('#address').fill('https://chatgpt.com/challenge-parent');
  await page.locator('#address-go').click();
  await until(() => events().then(s => s.includes('fixture-challenge-ready')), 'approved CAPTCHA frame');
  const frameMark = messages.length;
  await page.locator('#address').fill('https://chatgpt.com/challenge-escape-parent');
  await page.locator('#address-go').click();
  await until(() => messages.slice(frameMark).some(m => m.code === 'navigation_blocked'), 'CAPTCHA redirect rejected');
  assert(!(await events()).includes('fixture-escape'));
  pass('approved CAPTCHA iframe loads; iframe redirect escape rejected before document load');
  await page.locator('#address').fill('https://chatgpt.com/second');
  await page.locator('#address-go').click();
  await until(() => page.locator('#safe-url').textContent().then(t => t.endsWith('/second')), 'return after SSO fixtures');
  await clickViewport(100, 125);
  const pasteCount = (await events()).split('fixture-input-received').length;
  await canvas.evaluate(c => {
    const data = new DataTransfer();
    data.setData('text/plain', 'synthetic clipboard fixture');
    c.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
  });
  await until(async () => (await events()).split('fixture-input-received').length > pasteCount, 'canvas paste input');
  pass('explicit paste event reaches provider viewport without a gateway credential form');
  await clickViewport(100, 225);
  const other = await ownerContext.newPage();
  await other.goto(base + '/login');
  await other.locator('#gateway-authorization').fill(key);
  await other.locator('#authorization-form button[type="submit"]').click();
  await until(() => other.locator('#start-session').isEnabled(), 'second owner page');
  await other.getByRole('button', { name: /Claude/ }).click();
  let otherReady = false, otherEmpty = false;
  other.on('websocket', ws => ws.on('framereceived', event => {
    try {
      const m = JSON.parse(String(event.payload));
      if (m.type === 'status') otherReady = true;
      if (m.code === 'no_provider_session') otherEmpty = true;
    } catch {}
  }));
  await other.locator('#start-session').click();
  await until(() => otherReady, 'independent provider context');
  await other.locator('#save-session').click();
  await until(() => otherEmpty, 'other provider has no first-provider session');
  assert.equal((await ticket()).status, 409);
  await other.locator('#cancel-session').click();
  await other.close();
  pass('concurrent providers use independent Chromium contexts; same-provider overlap rejected');
  const before = (await events()).split('context-created').length;
  await page.locator('#cancel-session').click();
  await until(async () => (await ticket()).status === 200, 'cancel releases provider');
  await page.locator('#start-session').click();
  await until(async () => (await events()).split('context-created').length > before, 'fresh context');
  await until(() => page.locator('#safe-url').textContent().then(t => t.includes('chatgpt.com')), 'fresh page');
  pass('cancel closes stream; next Start creates independent browser context');
  const mark = messages.length;
  await page.locator('#save-session').click();
  await until(() => messages.slice(mark).some(m => m.code === 'no_provider_session'), 'empty session rejected');
  pass('empty session save rejected without vault write');
  await clickViewport(100, 225);
  await page.locator('#save-session').click();
  await until(() => messages.some(m => m.type === 'saved'), 'encrypted save');
  assert(messages.find(m => m.type === 'saved').active === false);
  const saved = (await readdir(join(temp, 'sessions'))).filter(n => n.includes('chatgpt') && n.endsWith('.vault'));
  assert.equal(saved.length, 1);
  assert.equal((await stat(join(temp, 'sessions', saved[0]))).mode & 0o777, 0o600);
  assert.equal((await stat(join(temp, 'sessions'))).mode & 0o777, 0o700);
  assert(!await readFile(join(temp, 'sessions', saved[0]), 'utf8').then(s => s.includes('fixture_session')));
  assert(!messages.some(m => m.type === 'snapshot'));
  pass('save persisted encrypted vault 0600, no snapshot on UI stream; activation reported separately');
  await until(() => page.locator('#start-session').isEnabled(), 'saved session closed');
  await page.locator('#start-session').click();
  await until(() => events().then(s => s.includes('fixture-restored')), 'vault restore');
  pass('next isolated browser restored cookies through existing encrypted vault');
  const restoredBeforeRestart = (await events()).split('fixture-restored').length;
  gateway.kill('SIGTERM');
  await until(() => gateway.exitCode !== null, 'gateway restart shutdown');
  assert.equal(gateway.exitCode, 0);
  gateway = startGateway();
  await until(async () => {
    if (gateway.exitCode !== null) throw new Error('gateway restart failed');
    try { return (await fetch(base + '/health')).status === 200; } catch { return false; }
  }, 'gateway restart health');
  await page.reload();
  await page.locator('#gateway-authorization').fill(key);
  await page.locator('#authorization-form button[type="submit"]').click();
  await until(() => page.locator('#start-session').isEnabled(), 'restart owner authorization');
  await page.locator('#start-session').click();
  await until(async () => (await events()).split('fixture-restored').length > restoredBeforeRestart,
    'vault restore after actual gateway process restart');
  pass('actual gateway stop/restart restores provider session from the same encrypted vault');
  const revoke = (secret, origin = base) => fetch(base + '/login/api/chatgpt/browser/session',
    { method: 'DELETE', headers: { Authorization: 'Bearer ' + secret, Origin: origin } });
  assert.equal((await revoke('invalid')).status, 401);
  assert.equal((await revoke(chatKey)).status, 403);
  assert.equal((await revoke(restrictedKey)).status, 403);
  assert.equal((await revoke(key, 'https://evil.invalid')).status, 403);
  page.once('dialog', d => d.accept());
  await page.getByRole('button', { name: 'Forget saved session' }).click();
  await until(() => page.locator('#start-session').isEnabled(), 'revocation closes live browser');
  await until(async () => !(await readdir(join(temp, 'sessions'))).includes(saved[0]), 'vault deletion confirmed');
  pass('authenticated revocation closes live browser before deleting vault; authorization/CSRF enforced');
  const cleanCount = (await events()).split('fixture-clean').length;
  await page.locator('#start-session').click();
  await until(async () => (await events()).split('fixture-clean').length > cleanCount, 'session absent after revocation');
  pass('fresh login after revocation cannot restore the deleted provider session');
  const crashMark = messages.length;
  await page.locator('#address').fill('https://chatgpt.com/worker-stop');
  await page.locator('#address-go').click();
  await until(() => messages.slice(crashMark).some(m => m.code === 'browser_runtime_unavailable'), 'worker failure reported');
  await until(() => page.locator('#start-session').isEnabled(), 'worker failure releases UI');
  const recoveryStart = messages.length;
  await page.locator('#start-session').click();
  await until(() => messages.slice(recoveryStart).some(m=>m.type==='frame'), 'recovery stream frame');
  await until(() => page.locator('#safe-url').textContent().then(t =>
    t.includes('chatgpt.com') && !t.includes('/worker-stop')), 'worker crash recovery');
  await until(fixtureRasterPainted, 'recovery fixture painted');
  pass('worker crash fails closed and a subsequent Start recovers in a new context');
  if (visual) {
    await page.screenshot({ path: join(dirname(source), 'embedded-browser-fixture.png'), fullPage: true });
    console.log('VISUAL_CHECK_READY port='+listen+' (synthetic fixture only)');
    await delay(40000);
  }
  // Shutdown while the restored stream is still active, not only after Cancel.
  gateway.kill('SIGTERM');
  await until(() => gateway.exitCode !== null, 'graceful gateway shutdown');
  assert.equal(gateway.exitCode, 0);
  pass('gateway SIGTERM exit 0');
  const profiles = (await events()).split('\n').filter(s =>
    /^fixture-profile:\/dev\/shm\/chat-gateway-login-[a-f0-9-]+$/.test(s))
    .map(s => s.slice('fixture-profile:'.length));
  assert(profiles.length > 1);
  await until(async () => (await Promise.all(profiles.map(p =>
    stat(p).then(() => false, e => e.code === 'ENOENT')))).every(Boolean), 'owned profile cleanup');
  pass('all test-owned provider profile directories removed after cancel/save/crash/revoke/shutdown');
} catch (error) {
  // No raw errors/objects: browser tooling can include request or input values.
  console.error('FAIL stage=' + stage + ' ' + (String(error.message).startsWith('timeout:')
    ? error.message : 'smoke assertion/runtime failure'));
  const publicCodes = controlDiagnostics.filter(m => m.type === 'error')
    .map(m => m.code).filter(c => ['browser_runtime_unavailable','navigation_blocked',
      'no_provider_session','invalid_input','page_limit','input_queue_full','session_revoked'].includes(c));
  console.error('Safe control error categories: ' + [...new Set(publicCodes)].join(','));
  for (const phase of new Set(controlDiagnostics.map(m => m.phase).filter(p =>
    ['target_attach','page_enable','document_guard','child_attach','target_resume'].includes(p))))
    console.error('Browser guard phase: ' + phase);
  const markers = await readFile(join(temp,'events'),'utf8').catch(() => '');
  for (const marker of markers.split('\n').filter(s => /^fixture-engine-error-(init|register|activate|status|dispatch)-line-(unknown|[0-9]+)$/.test(s)))
    console.error(marker);
  for (const marker of markers.split('\n').filter(s => /^fixture-network-category-ERR_[A-Z_]+$/.test(s)))
    console.error(marker);
  for (const [name, pattern] of Object.entries({
    timeout: /Timeout|timeout/, closed: /closed|crashed/, sandbox: /sandbox|namespace/,
    permission: /Permission denied|Operation not permitted/, resource: /quota|space|memory|pthread/,
    missing: /Executable doesn't exist|ENOENT/, navigation: /net::ERR|Navigation failed/
  })) if (pattern.test(String(error.message))) console.error('Diagnostic category: ' + name);
  process.exitCode = 1;
} finally {
  await page?.close().catch(() => {});
  await owner?.close().catch(() => {});
  if (gateway?.exitCode === null) {
    gateway.kill('SIGTERM');
    await delay(1500);
    if (gateway.exitCode === null) gateway.kill('SIGKILL');
  }
  await rm(temp, { recursive: true, force: true });
  console.log('CLEANUP temporary gateway, owner browser, fixture state and generated test credentials removed');
  console.log('Observed checks passed: ' + results.length);
}

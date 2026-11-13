// Actual Chromium extension against an isolated loopback-only gateway.
// No provider credentials, pairing secrets, or user-visible approval pages.
import {chromium} from "playwright";
import assert from "node:assert/strict";
import {spawn} from "node:child_process";
import {mkdtemp, cp, writeFile, rm} from "node:fs/promises";
import {resolve, dirname, join} from "node:path";
import {fileURLToPath} from "node:url";
import {randomBytes} from "node:crypto";
import net from "node:net";

const root = dirname(fileURLToPath(import.meta.url));
const temp = await mkdtemp("/tmp/cg-local-relay-");
const binary = resolve(process.argv[2]);
const reservation = net.createServer();
await new Promise(resolve => reservation.listen(0, "127.0.0.1", resolve));
const port = reservation.address().port;
await new Promise(resolve => reservation.close(resolve));
const base = "http://127.0.0.1:" + port;
let context, gateway;
try {
  await writeFile(join(temp, "config.json"), JSON.stringify({providers:[]}));
  await cp(root, join(temp, "extension"), {recursive:true});
  gateway = spawn(binary, ["--config", join(temp, "config.json")], {
    cwd:temp, stdio:"ignore",
    env:{
      PATH:process.env.PATH, HOME:temp, PORT:String(port),
      TOKIO_WORKER_THREADS:"2",
      CHAT_GATEWAY_LOCAL_RELAY_NO_PAIRING:"1",
      CHAT_GATEWAY_SESSION_STORE:"vault",
      CHAT_GATEWAY_VAULT_KEY:randomBytes(32).toString("hex"),
      CHAT_GATEWAY_MEMORY_PATH:join(temp, "memory.sqlite3"),
      CHAT_GATEWAY_EMBEDDED_BROWSER:"disabled",
      ...(process.env.LD_LIBRARY_PATH ? {LD_LIBRARY_PATH:process.env.LD_LIBRARY_PATH} : {}),
    },
  });
  let healthy = false;
  for (let i = 0; i < 200; i++) {
    if (gateway.exitCode !== null) throw new Error("Gateway exited during local relay acceptance");
    try { healthy = (await fetch(base + "/health")).ok; } catch {}
    if (healthy) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.ok(healthy, "isolated gateway must start");
  const mode = await (await fetch(base + "/relay/v1/mode")).json();
  assert.deepEqual(mode, {mode:"local", no_pairing:true});
  const plainStatus = await fetch(base + "/relay/v1/diagnostics");
  assert.equal(plainStatus.status, 200, "read-only localhost diagnostics must not need token");
  assert.equal((await plainStatus.json()).providers.length, 9);
  console.log("PASS no-Origin read-only diagnostics require no token");

  const malicious = await fetch(base + "/relay/v1/session/chatgpt", {
    method:"POST",
    headers:{"Origin":"https://evil.example","Content-Type":"application/json"},
    body:"{}",
  });
  assert.equal(malicious.status, 403, "website-origin session writes must be blocked");
  const localExtension = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";
  const validHeaders = {
    "Origin":localExtension, "Content-Type":"application/json",
    "X-Relay-Timestamp":String(Math.floor(Date.now()/1000)),
    "X-Relay-Nonce":randomBytes(16).toString("hex"),
  };
  const emptyImport = await fetch(base + "/relay/v1/session/chatgpt", {
    method:"POST", headers:validHeaders, body:"{}",
  });
  assert.notEqual(emptyImport.status, 401, "extension import must not require install token");
  assert.notEqual(emptyImport.status, 403, "valid extension Origin must pass local boundary");
  console.log("PASS website-origin write rejected and tokenless extension Origin accepted");

  context = await chromium.launchPersistentContext(join(temp, "profile"), {
    channel:"chromium", headless:true, chromiumSandbox:true,
    ...(process.env.CHAT_GATEWAY_CHROMIUM_PATH ? {executablePath:process.env.CHAT_GATEWAY_CHROMIUM_PATH} : {}),
    ignoreDefaultArgs:["--disable-extensions"],
    args:["--disable-extensions-except="+join(temp,"extension"),
      "--load-extension="+join(temp,"extension")],
  });
  const worker = context.serviceWorkers()[0] || await context.waitForEvent("serviceworker");
  const id = new URL(worker.url()).host;
  // The CI gateway listens on a random isolated port, not the popup's normal
  // 8080 default. Seed only its URL; never provide a token or pairing secret.
  await worker.evaluate(gatewayUrl => chrome.storage.local.set({gateway:gatewayUrl}), base);
  const popup = await context.newPage();
  await popup.goto("chrome-extension://" + id + "/popup.html");
  try {
    await popup.locator("#linked:not([hidden])").waitFor({timeout:20000});
  } catch (error) {
    console.log("Local popup diagnostic:", await popup.locator("#message").innerText());
    console.log("Local popup mode:", await popup.locator("#connection-help").innerText());
    console.log("Local popup storage:", await worker.evaluate(() => chrome.storage.local.get(
      ["gateway", "connectionMode", "localDisconnected"])));
    throw error;
  }
  await popup.locator("#status li").first().waitFor({timeout:10000});
  assert.equal(await popup.locator("#status li").count(),9);
  const stored = await worker.evaluate(() => chrome.storage.local.get(
    ["gateway","token","connectionMode","pendingConnection"]));
  assert.equal(stored.connectionMode,"local");
  assert.equal(stored.gateway,base);
  assert.equal(stored.token,undefined);
  assert.equal(stored.pendingConnection,undefined);
  const openUrls = context.pages().map(page => page.url());
  assert.equal(openUrls.filter(url => url.includes("/relay/approve/")).length, 0,
    "no pairing or approval page should open");
  assert.equal(openUrls.filter(url => url.startsWith("chrome-extension://")).length, 1,
    "only the requested extension popup should be opened");
  const diagnostics = await popup.evaluate(() => chrome.runtime.sendMessage({
    type:"relay-diagnostics", force:true
  }));
  assert.equal(diagnostics.ok,true);
  assert.equal(diagnostics.extension.connected,true);
  assert.equal(diagnostics.gateway.providers.length,9);
  assert.ok(!JSON.stringify(diagnostics).includes("Bearer"));
  console.log("PASS actual extension auto-connected with no pairing page, install token, or credentials");
} finally {
  await context?.close();
  if (gateway && gateway.exitCode === null) {
    const stopped = new Promise(resolve => gateway.once("exit", resolve));
    gateway.kill("SIGTERM");
    const timer = setTimeout(() => gateway.kill("SIGKILL"), 30000);
    await stopped; clearTimeout(timer);
  }
  await rm(temp, {recursive:true,force:true});
}

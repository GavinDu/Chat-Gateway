const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");

function loadPopup({pollResult = {ok:true}} = {}) {
  const elements = new Map();
  const actions = new Map();
  const values = {};
  const requestId = "a".repeat(64);
  const pollSecret = "b".repeat(64);

  function element(id) {
    if (!elements.has(id)) {
      const item = {
        value: id === "gateway" ? "http://127.0.0.1:8080" : "",
        textContent: "",
        hidden: false,
        children: [],
        addEventListener(type, handler) {
          actions.set(id + ":" + type, handler);
        },
        append(child) { this.children.push(child); },
        click() {},
      };
      elements.set(id, item);
    }
    return elements.get(id);
  }

  const chrome = {
    permissions: { request: async () => true, contains: async () => true },
    storage: {local: {
      async get(keys) {
        return Object.fromEntries(keys.map(key => [key, values[key]]));
      },
      async set(next) { Object.assign(values, next); },
      async remove(keys) { for (const key of Array.isArray(keys) ? keys : [keys]) delete values[key]; },
    }},
    runtime: { sendMessage: async () => pollResult },
    // Intentionally no tabs or alarms API: reproduces reading 'create' of undefined.
  };
  const context = {
    chrome, Promise, Set, Array, Date, document: {
      getElementById: element,
      createElement: () => ({textContent: "", href: "", target: "", rel: ""}),
    },
    window: {addEventListener() {}},
    setInterval() {},
    normalizeGateway: value => value,
    PROVIDER_LOGIN_URLS: {metaai:"https://www.meta.ai/"},
    PROVIDERS: {},
    relayFetch: async () => ({ok: true,json: async () => ({providers:[],vault_mode:true})}),
    fetch: async () => ({ok:true,status:200,json:async()=>({request_id:requestId,poll_secret:pollSecret})}),
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, "..", "popup.js"), "utf8") +
    "\nglobalThis.openProviderLoginForTest = openProviderLogin; globalThis.updateConnectionForTest = updateConnection;",context);
  return {elements,actions,values,requestId,context};
}

test("local approval remains possible without tabs.create or alarms.create", async () => {
  const popup = loadPopup();
  await popup.actions.get("pair:click")();
  const pending = popup.values.pendingConnection;
  assert.equal(pending.request_id, popup.requestId);
  assert.equal(popup.elements.get("message").children.at(-1).href,
    "http://127.0.0.1:8080/relay/approve/" + popup.requestId);
  assert.equal(popup.elements.get("message").children.at(-1).target, "_blank");
});

test("provider login offers an explicit safe link when tabs API is missing", async () => {
  const popup = loadPopup();
  await popup.context.openProviderLoginForTest("metaai");
  const link = popup.elements.get("message").children.at(-1);
  assert.equal(link.href, "https://www.meta.ai/");
  assert.equal(link.rel, "noopener noreferrer");
});


test("approval exchange errors remain visible instead of being replaced by Waiting", async () => {
  const popup=loadPopup({pollResult:{ok:false,error:"Gateway permission is missing."}});
  await popup.actions.get("pair:click")();
  await popup.context.updateConnectionForTest();
  assert.equal(popup.elements.get("message").textContent,"Gateway permission is missing.");
  assert.ok(popup.values.pendingConnection,"a temporary error keeps the approval available for retry");
});

test("an old or missing worker acknowledgement offers reload instead of indefinite Waiting", async () => {
  const popup=loadPopup();
  popup.context.chrome.runtime.sendMessage=async()=>undefined;
  await popup.actions.get("pair:click")();
  await popup.context.updateConnectionForTest();
  assert.match(popup.elements.get("message").textContent,/worker did not respond.*Reload/);
});


test("Connect completes an approved pending exchange before reopening its used page", async () => {
  const popup=loadPopup();
  await popup.actions.get("pair:click")();
  popup.context.chrome.runtime.sendMessage=async()=>{
    popup.values.token="c".repeat(64);
    delete popup.values.pendingConnection;
    return {ok:true};
  };
  await popup.actions.get("pair:click")();
  assert.equal(popup.values.pendingConnection,undefined);
  assert.equal(popup.elements.get("pairing").hidden,true);
  assert.equal(popup.elements.get("linked").hidden,false);
});


test("Connect preserves a failed pending check instead of reopening the used approval URL", async () => {
  const popup=loadPopup({pollResult:{ok:false,error:"Gateway permission is missing."}});
  await popup.actions.get("pair:click")();
  const links=popup.elements.get("message").children.length;
  await popup.actions.get("pair:click")();
  assert.equal(popup.elements.get("message").textContent,"Gateway permission is missing.");
  assert.equal(popup.elements.get("message").children.length,links);
});

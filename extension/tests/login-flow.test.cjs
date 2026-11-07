const test = require("node:test");
const assert = require("node:assert/strict");
const vm = require("node:vm");
const fs = require("node:fs");
const path = require("node:path");

function fixture({connected = false, linked = false, cookies = true} = {}) {
  const nodes = new Map();
  const values = connected ? {gateway:"http://127.0.0.1:8080", token:"c".repeat(64)} : {};
  const network = [], openedTabs = [], messages = [];
  const newNode = tag => ({
    tag, value:"", hidden:false, disabled:false, textContent:"", children:[], handlers:{},
    append(...items) { this.children.push(...items); },
    replaceChildren(...items) { this.children = [...items]; },
    addEventListener(name, fn) { this.handlers[name] = fn; },
    click() { return this.handlers.click?.(); },
  });
  function node(id) {
    if (!nodes.has(id)) {
      const n = newNode(id);
      if (id === "gateway") n.value = "http://127.0.0.1:8080";
      nodes.set(id, n);
    }
    return nodes.get(id);
  }

  const chrome = {
    storage:{local:{
      async get(keys) { return Object.fromEntries(keys.map(key => [key, values[key]])); },
      async set(data) { Object.assign(values, data); },
      async remove(keys) { for (const key of Array.isArray(keys) ? keys : [keys]) delete values[key]; },
    }},
    permissions:{request:async()=>true,contains:async()=>true},
    cookies:{getAll:async()=>cookies ? [{name:"example"}] : []},
    tabs:{create:async ({url}) => {openedTabs.push(url);}},
    alarms:{create:async()=>{},clear:async()=>{}},
    runtime:{sendMessage:async message=>{
      messages.push(message);
      if (message.type === "logout-provider") {
        linked = false;
        values.linkedProviders = (values.linkedProviders || []).filter(value => value !== message.provider);
      }
      if (message.type === "sync-provider") linked = true;
      return {ok:true};
    }},
  };
  const context = {
    chrome, Set, Promise, Date, Array,
    document:{getElementById:node,createElement:newNode},
    window:{addEventListener(){}}, setInterval(){},
    confirm:()=>true,
    normalizeGateway:value=>value,
    PROVIDER_LOGIN_URLS:{metaai:"https://www.meta.ai/"},
    PROVIDERS:{metaai:["meta.ai"]},
    providerCookieSnapshot:async()=>({
      cookies:cookies?[{name:"session",value:"fake-cookie"}]:[],
      local_storage:{},ts:"2026-11-07T12:00:00Z",
    }),
    relayFetch:async (uri, opts)=>{
      network.push({uri,method:opts.method});
      if (uri === "/relay/v1/status") return {ok:true,status:200,json:async()=>({
        providers:[{provider:"metaai",enabled:true,linked,authentication:"unknown",availability:"unknown"}],
        vault_mode:true,
      })};
      if (uri === "/relay/v1/session/metaai" && opts.method === "POST") {
        linked = true;return {ok:true,status:200};
      }
      throw new Error("Unexpected relay request " + uri);
    },
    fetch:async url=>{
      network.push({uri:url,method:"POST"});
      return {ok:true,json:async()=>({request_id:"a".repeat(64),poll_secret:"b".repeat(64)})};
    },
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, "..", "popup.js"), "utf8") +
    "\nglobalThis.refreshStatusForTest=refreshStatus;globalThis.connectProviderForTest=connectProvider;globalThis.logoutProviderForTest=logoutProvider;",
    context);
  return {context,nodes,values,network,openedTabs,messages};
}
const settle = () => new Promise(resolve=>setImmediate(resolve));
function providerButtons(f) {
  const card = f.nodes.get("status").children[0];
  const actions = card.children.find(child => child.className === "provider-actions");
  return actions.children;
}

test("Connect opens approval and reuses the pending request after popup reopen", async () => {
  const f = fixture();
  await settle();
  assert.equal(f.nodes.get("pairing").hidden,false);
  await f.nodes.get("pair").click();
  await f.nodes.get("pair").click();
  assert.equal(f.network.filter(call => call.uri.endsWith("/relay/v1/connect/request")).length,1);
  assert.equal(f.openedTabs.length,2);
  assert.equal(f.openedTabs[0],"http://127.0.0.1:8080/relay/approve/" + "a".repeat(64));
  assert.equal(f.values.token,undefined);
});

test("connected gateway displays provider card and starts official website login", async () => {
  const f = fixture({connected:true,linked:false,cookies:false});
  await settle();
  await f.context.refreshStatusForTest();
  assert.equal(f.nodes.get("linked").hidden,false);
  assert.equal(f.nodes.get("pairing").hidden,true);
  assert.equal(f.nodes.get("gateway").disabled,true);
  const buttons = providerButtons(f);
  assert.equal(buttons[0].textContent,"Log in / Connect");
  assert.equal(buttons.some(button=>button.textContent==="Log out"),false);
  await buttons[0].click();
  assert.deepEqual(Array.from(f.values.linkedProviders),["metaai"]);
  assert.equal(f.openedTabs.at(-1),"https://www.meta.ai/");
  assert.equal(f.network.filter(x=>x.uri==="/relay/v1/session/metaai").length,0);
});

test("provider login sync exposes compact Log out and permits reconnect", async () => {
  const f = fixture({connected:true,linked:false,cookies:true});
  await settle();
  await f.context.refreshStatusForTest();
  await providerButtons(f)[0].click();
  assert.equal(f.messages.filter(x=>x.type==="sync-provider" && x.provider==="metaai").length,1);
  const buttons = providerButtons(f);
  const logout = buttons.find(button=>button.textContent==="Log out");
  assert.ok(logout,"linked provider must expose small logout");
  assert.match(logout.className,/quiet-danger/);
  await logout.click();
  assert.equal(f.messages.at(-1).type,"logout-provider");
  assert.deepEqual(Array.from(f.values.linkedProviders),[]);
  assert.equal(providerButtons(f)[0].textContent,"Log in / Connect");
  await providerButtons(f)[0].click();
  assert.equal(f.messages.filter(x=>x.type==="sync-provider" && x.provider==="metaai").length,2);
});

test("provider logout does not attempt to sign out from the official site", async () => {
  const f = fixture({connected:true,linked:true,cookies:true});
  await settle();
  await f.context.refreshStatusForTest();
  const logout = providerButtons(f).find(button => button.textContent === "Log out");
  await logout.click();
  assert.equal(f.openedTabs.length,0);
  assert.ok(f.messages.some(message => message.type === "logout-provider"));
});

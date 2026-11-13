const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadPopup({oauth = false, failCookieDomain = null, includeSecondProvider = false, manualVerification = false, localRelay = false} = {}) {
  const elements = new Map();
  const chromeCalls = [];
  const values = {gateway:"http://127.0.0.1:8080",linkedProviders:[]};
  const server = {linked:false};
  function node() {
    return {
      children:[], textContent:"", value:"", hidden:false, disabled:false,
      handlers:{},
      append(...children){this.children.push(...children);},
      replaceChildren(...children){this.children = children;},
      addEventListener(type,fn){this.handlers[type]=fn;},
      click(){this.handlers.click?.();},
    };
  }
  function byId(id) {
    if(!elements.has(id)) {
      const element=node();
      if(id==="gateway")element.value="http://127.0.0.1:8080";
      elements.set(id,element);
    }
    return elements.get(id);
  }
  const runtime = { async sendMessage(request) {
    chromeCalls.push(request.type + ":" + (request.provider||""));
    if(request.type==="logout-provider") {
      server.linked=false;
      values.linkedProviders=values.linkedProviders.filter(p=>p!==request.provider);
    }
    if(request.type==="sync-provider") server.linked=true;
    return {ok:true};
  }};
  const context = {
    Promise, Date, Set, Array,
    confirm:()=>true,
    document:{getElementById:byId,createElement:node},
    window:{addEventListener(){}},
    setInterval(){},
    isLocalGateway: value => value.startsWith("http://127.0.0.1:") || value.startsWith("http://localhost:"),
    hasRelayConnection: state => Boolean(state.gateway && (state.token || state.connectionMode === "local")),
    normalizeGateway: v=>v,
    PROVIDER_LOGIN_URLS:{metaai:"https://www.meta.ai/"},
    PROVIDERS:{metaai:["meta.ai"],claude:["claude.ai"]},
    providerCookieSnapshot:async()=>({cookies:[{name:"token",value:"test-only"}]}),
    relayFetch:async()=>({ok:true,status:200,json:async()=>({
      providers:[
        {provider:"metaai",connection_method:oauth?"chatgpt_oauth":"browser",enabled:true,linked:server.linked,authentication:server.linked?"unknown":"needs_login",
          availability:manualVerification?"degraded":"unknown",manual_verification_required:manualVerification},
        ...(includeSecondProvider ? [{provider:"claude",enabled:true,linked:false,authentication:"needs_login",availability:"unknown"}] : []),
      ],
      vault_mode:true,
    })}),
    fetch:async url => ({ok:true,json:async()=>url.endsWith("/relay/v1/mode")
      ? {mode:localRelay?"local":"paired",no_pairing:localRelay}
      : {request_id:"a".repeat(64),poll_secret:"b".repeat(64)}}),
    chrome:{
      tabs:{async create({url}) {chromeCalls.push("tab:"+url);}},
      permissions:{request:async()=>true,contains:async()=>true},
      cookies:{getAll:async({domain})=>{
        if(domain===failCookieDomain)throw new Error("simulated browser API failure");
        return [{name:"token",value:"test-only"}];
      }},
      runtime,
      alarms:{create:async()=>{},clear:async name=>{chromeCalls.push("alarm-clear:"+name);}},
      storage:{local:{
        async get(keys){return Object.fromEntries(keys.map(k=>[k,values[k]]));},
        async set(next){Object.assign(values,next);},
        async remove(keys){for(const k of Array.isArray(keys)?keys:[keys])delete values[k];},
      }},
    },
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname,"..","popup.js"),"utf8")+
    "\nglobalThis.testControls={refreshStatus,connectProvider,logoutProvider};",context);
  return {values,server,elements,chromeCalls,controls:context.testControls};
}

test("Connect to gateway opens local approval page automatically",async()=>{
  const popup=loadPopup();
  await popup.elements.get("pair").handlers.click();
  assert.ok(popup.chromeCalls.includes("tab:http://127.0.0.1:8080/relay/approve/"+"a".repeat(64)));
  assert.equal(popup.values.pendingConnection.request_id,"a".repeat(64));
});

test("connected dashboard exposes login and small logout, then allows reconnection",async()=>{
  const popup=loadPopup();
  popup.values.token="c".repeat(64);
  await popup.controls.refreshStatus();
  let row=popup.elements.get("status").children[0];
  let buttons=row.children.find(n=>n.className==="provider-actions").children;
  assert.ok(buttons.some(b=>b.textContent==="Log in / Connect"));
  assert.equal(buttons.some(b=>b.textContent==="Log out"),false);
  await popup.controls.connectProvider("metaai");
  assert.equal(popup.server.linked,true);
  await popup.controls.refreshStatus();
  row=popup.elements.get("status").children[0];
  buttons=row.children.find(n=>n.className==="provider-actions").children;
  assert.ok(buttons.some(b=>b.textContent==="Log out"));
  const logout=buttons.find(b=>b.textContent==="Log out");
  assert.equal(logout.className,"quiet-danger provider-action");
  await popup.controls.logoutProvider("metaai");
  assert.equal(popup.server.linked,false);
  assert.deepEqual(Array.from(popup.values.linkedProviders),[]);
  await popup.controls.connectProvider("metaai");
  assert.equal(popup.server.linked,true);
  assert.ok(popup.chromeCalls.includes("logout-provider:metaai"));
  assert.equal(popup.chromeCalls.filter(x=>x==="sync-provider:metaai").length,2);
});

test("provider challenge is shown as manual verification, not an expired session or automatic retry",async()=>{
  const popup=loadPopup({manualVerification:true});
  popup.server.linked=true;
  await popup.controls.refreshStatus();
  const row=popup.elements.get("status").children[0];
  const status=row.children.find(child=>child.className==="provider-detail").textContent;
  assert.match(status,/Manual provider verification required; open website/);
  assert.doesNotMatch(status,/Gateway check unavailable; retry/);
  assert.doesNotMatch(status,/Login needed/);
  const actions=row.children.find(child=>child.className==="provider-actions").children;
  assert.ok(actions.some(button=>button.textContent==="Open website"));
});

test("provider dashboard keeps healthy providers actionable when one cookie probe rejects",async()=>{
  const popup=loadPopup({failCookieDomain:"meta.ai",includeSecondProvider:true});
  await popup.controls.refreshStatus();
  const [failed,healthy]=popup.elements.get("status").children;
  assert.equal(failed.children[0].textContent,"metaai");
  assert.match(failed.children[1].textContent,/Browser cookie check unavailable/);
  assert.equal(healthy.children[0].textContent,"claude");
  assert.match(healthy.children[1].textContent,/Browser cookies present/);
  const buttons=healthy.children.find(child=>child.className==="provider-actions").children;
  assert.equal(buttons.find(button=>button.textContent==="Log in / Connect").disabled,false);
  assert.ok(buttons.some(button=>button.textContent==="Open website"));
});

test("explicit gateway disconnect removes pending approval and stops polling",async()=>{
  const popup=loadPopup();
  await Promise.resolve();
  popup.values.token="c".repeat(64);
  popup.values.linkedProviders=["metaai"];
  popup.values.pendingConnection={
    gateway:"http://127.0.0.1:8080",
    request_id:"a".repeat(64),
    poll_secret:"b".repeat(64),
    expires:Date.now()+300000
  };
  await popup.elements.get("unpair").handlers.click();
  assert.equal(popup.values.token,undefined);
  assert.equal(popup.values.pendingConnection,undefined);
  assert.deepEqual(Array.from(popup.values.linkedProviders||[]),[]);
  assert.ok(popup.chromeCalls.includes("alarm-clear:relay-connection"));
});

test("local-only gateway automatically connects without pairing, approval or token", async () => {
  const popup = loadPopup({localRelay:true});
  for (let i = 0; i < 6; i++) await new Promise(resolve => setImmediate(resolve));
  assert.equal(popup.values.connectionMode, "local");
  assert.equal(popup.values.token, undefined);
  assert.equal(popup.values.pendingConnection, undefined);
  assert.equal(popup.elements.get("linked").hidden, false);
  assert.equal(popup.elements.get("pairing").hidden, true);
  assert.equal(popup.chromeCalls.some(value => value.startsWith("tab:")), false);
  await popup.controls.connectProvider("metaai");
  assert.equal(popup.server.linked, true);
  await popup.controls.logoutProvider("metaai");
  assert.equal(popup.server.linked, false);
});


test("OAuth dashboard directs account management to gateway without reading cookies",async()=>{
  const popup=loadPopup({oauth:true});
  popup.values.token="c".repeat(64);
  popup.server.linked=true;
  await popup.controls.refreshStatus();
  const row=popup.elements.get("status").children[0];
  const buttons=row.children.find(n=>n.className==="provider-actions").children;
  assert.ok(buttons.some(b=>b.textContent==="Managed by gateway" && b.disabled));
  assert.equal(buttons.some(b=>b.textContent==="Log out"),false);
  assert.equal(popup.chromeCalls.some(call=>call.startsWith("cookies:")),false);
});

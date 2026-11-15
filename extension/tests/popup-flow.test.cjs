const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadPopup({failCookieDomain = null, includeSecondProvider = false, manualVerification = false, localRelay = false, activation = null, diagnosticsSync = null, diagnosticsFailure = null, pairedDisconnectReject = false} = {}) {
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
    if(request.type==="relay-diagnostics" && diagnosticsFailure) return diagnosticsFailure;
    if(request.type==="relay-diagnostics" && diagnosticsSync) {
      return {ok:false,extension:{provider_sync:{metaai:diagnosticsSync}}};
    }
    if(request.type==="connect-local-relay") {
      if (values.localDisconnected && !request.manual) {
        return {ok:false,error:"Local gateway was explicitly disconnected."};
      }
      const unchanged=values.gateway===request.gateway &&
        values.connectionMode==="local" && !values.token;
      const oldGateway=values.gateway;
      values.gateway=request.gateway;
      values.connectionMode="local";
      values.linkedProviders=oldGateway===request.gateway ? (values.linkedProviders || []) : [];
      values.localDisconnected=false;
      delete values.token;
      delete values.pendingConnection;
      delete values.relaySnapshotVersions;
      return {ok:true,unchanged};
    }
    if(request.type==="disconnect-local-relay") {
      const expected=request.connection;
      if(expected.gateway!==values.gateway || expected.token!==values.token ||
          expected.connectionMode!==values.connectionMode || values.connectionMode!=="local") {
        return {ok:false,error:"Local gateway connection changed before disconnect."};
      }
      delete values.connectionMode;
      delete values.token;
      delete values.linkedProviders;
      delete values.relaySnapshotVersions;
      delete values.pendingConnection;
      values.localDisconnected=true;
      return {ok:true};
    }
    if(request.type==="connect-paired-relay") {
      const previous=request.connection;
      if(previous.gateway!==values.gateway || previous.token!==values.token ||
          previous.connectionMode!==values.connectionMode) {
        return {ok:false,error:"Remote pairing was superseded."};
      }
      values.gateway=request.gateway;
      values.token=request.token;
      values.connectionMode="paired";
      values.linkedProviders=[];
      values.providerLinkEpochs={};
      values.providerLinkPendingAttempts={};
      values.relaySnapshotVersions={};
      delete values.pendingConnection;
      return {ok:true};
    }
    if(request.type==="disconnect-paired-relay") {
      if(pairedDisconnectReject) return {ok:false,error:"Paired gateway connection changed before disconnect."};
      const expected=request.connection;
      if(expected.gateway!==values.gateway || expected.token!==values.token ||
          expected.connectionMode!==values.connectionMode) {
        return {ok:false,error:"Paired gateway connection changed before disconnect."};
      }
      values.token=null;
      values.connectionMode=null;
      values.linkedProviders=[];
      values.relaySnapshotVersions={};
      if(values.pendingConnection?.request_id===expected.pendingRequest?.request_id &&
          values.pendingConnection?.expires===expected.pendingRequest?.expires) {
        values.pendingConnection=null;
      }
      return {ok:true};
    }
    if(request.type==="link-provider") {
      const expected=request.connection;
      if(expected.gateway!==values.gateway || expected.token!==values.token ||
          expected.connectionMode!==values.connectionMode) {
        return {ok:false,error:"Gateway connection changed before provider link."};
      }
      const alreadyLinked=(values.linkedProviders || []).includes(request.provider);
      const previous=(values.providerLinkPendingAttempts || {})[request.provider];
      const tentative=!alreadyLinked || Boolean(previous);
      const epochs=values.providerLinkEpochs || {};
      const epoch=tentative && previous!==request.attemptId
        ? "test-link-"+chromeCalls.length
        : (epochs[request.provider] || "test-link-"+chromeCalls.length);
      values.linkedProviders=Array.from(new Set([...(values.linkedProviders || []),request.provider]));
      values.providerLinkEpochs={...epochs,[request.provider]:epoch};
      if(tentative) values.providerLinkPendingAttempts={
        ...(values.providerLinkPendingAttempts || {}),[request.provider]:request.attemptId
      };
      return {ok:true,alreadyLinked:alreadyLinked&&!tentative,tentative};
    }
    if(request.type==="commit-provider-link" || request.type==="rollback-provider-link") {
      const current=(values.providerLinkPendingAttempts || {})[request.provider];
      const expected=request.connection;
      const owned=expected.gateway===values.gateway && expected.token===values.token &&
        expected.connectionMode===values.connectionMode && current===request.attemptId;
      if(owned) {
        const remaining={...(values.providerLinkPendingAttempts || {})};
        delete remaining[request.provider];
        values.providerLinkPendingAttempts=remaining;
        if(request.type==="rollback-provider-link") {
          values.linkedProviders=(values.linkedProviders || []).filter(p=>p!==request.provider);
          values.providerLinkEpochs={...(values.providerLinkEpochs || {}),
            [request.provider]:"test-revoked-"+chromeCalls.length};
        }
      }
      return {ok:true,[request.type==="commit-provider-link" ? "committed":"rolledBack"]:owned};
    }
    if(request.type==="logout-provider") {
      server.linked=false;
      values.linkedProviders=values.linkedProviders.filter(p=>p!==request.provider);
      values.providerLinkEpochs={...(values.providerLinkEpochs || {}),
        [request.provider]:"test-logout-"+chromeCalls.length};
      const pending={...(values.providerLinkPendingAttempts || {})};
      delete pending[request.provider];
      values.providerLinkPendingAttempts=pending;
    }
    if(request.type==="sync-provider") server.linked=true;
    return {ok:true};
  }};
  const context = {
    Promise, Date, Set, Array,
    randomHex: (() => { let counter = 0; return () => "test-epoch-" + (++counter); })(),
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
    relayFetch:async()=>{chromeCalls.push("http:relay-status");return {ok:true,status:200,json:async()=>({
      providers:[
        {provider:"metaai",enabled:true,linked:server.linked,authentication:server.linked?"unknown":"needs_login",
          availability:manualVerification?"degraded":"unknown",manual_verification_required:manualVerification,activation},
        ...(includeSecondProvider ? [{provider:"claude",enabled:true,linked:false,authentication:"needs_login",availability:"unknown"}] : []),
      ],
      vault_mode:true,
    })};},
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
  return {values,server,elements,chromeCalls,controls:context.testControls,context};
}

test("worker diagnostic failure avoids a second redundant popup status HTTP request", async () => {
  for (const [gateway_issue, message] of [
    ["gateway_timeout", /timed out/],
    ["gateway_request_failed", /status request failed/],
    ["not_connected", /not connected/],
    ["relay_unavailable", /status is unavailable/],
    ["unknown_code_from_future_worker", /status is unavailable/],
  ]) {
    const popup=loadPopup({diagnosticsFailure:{ok:false,gateway_issue}});
    await assert.rejects(() => popup.controls.refreshStatus(), message);
    assert.equal(popup.chromeCalls.filter(x=>x==="relay-diagnostics:").length,1);
    assert.equal(popup.chromeCalls.filter(x=>x==="http:relay-status").length,0,
      "an explicit worker failure already reflects a completed or intentionally skipped health probe");
  }
});

test("known worker unauthorized preserves local mode and revokes paired credentials without HTTP retry", async () => {
  const local=loadPopup({diagnosticsFailure:{ok:false,gateway_issue:"gateway_unauthorized"}});
  local.values.connectionMode="local";
  local.values.token="must-not-be-removed";
  await assert.rejects(() => local.controls.refreshStatus(), /Local relay access rejected/);
  assert.equal(local.values.token,"must-not-be-removed");
  assert.equal(local.chromeCalls.filter(x=>x==="http:relay-status").length,0);

  const paired=loadPopup({diagnosticsFailure:{ok:false,gateway_issue:"gateway_unauthorized"}});
  paired.values.connectionMode="paired";
  paired.values.token="temporary-test-token";
  await assert.rejects(() => paired.controls.refreshStatus(), /expired or was revoked/);
  assert.equal(paired.values.token,undefined, "paired authorization revocation must still clear local token");
  assert.equal(paired.elements.get("pairing").hidden,false);
  assert.equal(paired.chromeCalls.filter(x=>x==="http:relay-status").length,0);
});

test("older worker without a recognized gateway_issue keeps one direct status fallback", async () => {
  const popup=loadPopup({diagnosticsFailure:{ok:false}});
  await popup.controls.refreshStatus();
  assert.equal(popup.chromeCalls.filter(x=>x==="http:relay-status").length,1);
});

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

test("explicit provider link persists one epoch and only re-link after logout rotates it", async () => {
  const popup=loadPopup();
  popup.values.token="c".repeat(64);
  await popup.controls.connectProvider("metaai");
  const first = popup.values.providerLinkEpochs.metaai;
  assert.ok(first);
  await popup.controls.connectProvider("metaai");
  assert.equal(popup.values.providerLinkEpochs.metaai,first,
    "manual Sync of already linked provider must not revoke an in-flight generation");

  // The background worker rotates the epoch on logout, then clears consent.
  popup.values.linkedProviders = [];
  popup.values.providerLinkEpochs.metaai = "test-revocation-epoch";
  await popup.controls.connectProvider("metaai");
  assert.notEqual(popup.values.providerLinkEpochs.metaai,first);
  assert.notEqual(popup.values.providerLinkEpochs.metaai,"test-revocation-epoch");
});

test("browser activation statuses stay distinct from login verification", async () => {
  for (const [state, expected] of [
    ["pending", "Browser activation pending"],
    ["ready", "Browser activation completed"],
    ["failed", "Browser activation failed"],
    ["not_attempted", "Browser activation not attempted"]
  ]) {
    const popup = loadPopup({activation:state});
    popup.server.linked = true;
    await popup.controls.refreshStatus();
    const row = popup.elements.get("status").children[0];
    const status = row.children.find(child => child.className==="provider-detail").textContent;
    assert.match(status, /Login unverified/, "session activation cannot imply authentication");
    assert.ok(status.includes(expected), "show only fixed activation categories");
    assert.doesNotMatch(status, /Login verified/);
  }
});

test("popup displays last extension sync and stale status independently of auth", async () => {
  const last = "2026-11-14T17:00:00.000Z";
  const popup = loadPopup({diagnosticsSync:{
    state:"uploaded",issue:"gateway_request_failed",updated_at:last,stale:true
  }});
  popup.server.linked = true;
  await popup.controls.refreshStatus();
  const row = popup.elements.get("status").children[0];
  const status = row.children.find(child => child.className==="provider-detail").textContent;
  assert.match(status,/Login unverified/);
  assert.ok(status.includes("Last extension sync attempt: "+last));
  assert.match(status,/Extension sync status stale/);
  assert.match(status,/Extension sync: gateway request failed/);
  assert.doesNotMatch(status,/Login verified/);
  assert.doesNotMatch(status,/private|token|cookie=secret/);
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
  popup.values.gateway="https://private.example";
  popup.values.connectionMode="paired";
  popup.values.token="c".repeat(64);
  popup.values.linkedProviders=["metaai"];
  popup.values.pendingConnection={
    gateway:"https://private.example",
    request_id:"a".repeat(64),
    poll_secret:"b".repeat(64),
    expires:Date.now()+300000
  };
  await popup.elements.get("unpair").handlers.click();
  assert.equal(popup.values.token,null);
  assert.equal(popup.values.pendingConnection,null);
  assert.deepEqual(Array.from(popup.values.linkedProviders||[]),[]);
  assert.ok(popup.chromeCalls.includes("alarm-clear:relay-connection"));
  assert.ok(popup.chromeCalls.some(x=>x==="disconnect-paired-relay:"),
    "paired Unpair must use a worker-owned consent transition");
});


test("missing paired worker acknowledgement never falls back to popup consent deletion",async()=>{
  const popup=loadPopup({pairedDisconnectReject:true});
  await Promise.resolve();
  popup.values.gateway="https://private.example";
  popup.values.connectionMode="paired";
  popup.values.token="c".repeat(64);
  popup.values.linkedProviders=["metaai"];
  await popup.elements.get("unpair").handlers.click();
  assert.equal(popup.values.token,"c".repeat(64));
  assert.deepEqual(Array.from(popup.values.linkedProviders),["metaai"]);
  assert.match(popup.elements.get("message").textContent,/connection changed/);
});

test("local connection and explicit disconnect are worker-owned rather than popup map writes",async()=>{
  const popup=loadPopup({localRelay:true});
  for(let i=0;i<6;i++)await new Promise(resolve=>setImmediate(resolve));
  assert.equal(popup.values.connectionMode,"local");
  assert.ok(popup.chromeCalls.some(x=>x==="connect-local-relay:"),
    "local connection must be committed by worker message");
  popup.values.linkedProviders=["metaai"];
  await popup.elements.get("unpair").handlers.click();
  assert.ok(popup.chromeCalls.some(x=>x==="disconnect-local-relay:"),
    "explicit local disconnect must be committed by worker message");
  assert.equal(popup.values.localDisconnected,true);
  assert.equal(popup.values.linkedProviders,undefined);
  assert.equal(popup.values.connectionMode,undefined);
  assert.equal(popup.values.token,undefined);
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

test("legacy HTTPS paired Connect commits only through worker-owned mutation",async()=>{
  const popup=loadPopup();
  popup.context.document.getElementById("gateway").value="https://gateway.example";
  popup.context.document.getElementById("code").value="12345678";
  popup.values.linkedProviders=["metaai"];
  popup.values.token="a".repeat(64);
  popup.values.connectionMode="paired";
  popup.values.providerLinkEpochs={metaai:"old-epoch"};
  popup.context.fetch=async url=>({
    ok:true,json:async()=>url.endsWith("/relay/v1/pair")
      ? {token:"b".repeat(64)}:{no_pairing:false}
  });
  await popup.elements.get("pair").handlers.click();
  assert.ok(popup.chromeCalls.includes("connect-paired-relay:"));
  assert.equal(popup.values.gateway,"https://gateway.example");
  assert.equal(popup.values.token,"b".repeat(64));
  assert.equal(popup.values.connectionMode,"paired");
  assert.deepEqual(Array.from(popup.values.linkedProviders),[]);
  assert.deepEqual(Object.keys(popup.values.providerLinkEpochs),[]);
});

test("delayed HTTPS pair response cannot supersede newer local connection",async()=>{
  const popup=loadPopup();
  popup.context.document.getElementById("gateway").value="https://gateway.example";
  popup.context.document.getElementById("code").value="12345678";
  let release;
  let notifyStarted;
  const startedRequest=new Promise(resolve=>{notifyStarted=resolve;});
  const gate=new Promise(resolve=>{release=resolve;});
  popup.context.fetch=async url=>{
    if(url.endsWith("/relay/v1/pair")) {
      notifyStarted();
      await gate;
      return {ok:true,json:async()=>({token:"b".repeat(64)})};
    }
    return {ok:true,json:async()=>({no_pairing:false})};
  };
  const pairing=popup.elements.get("pair").handlers.click();
  await startedRequest;
  popup.values.gateway="http://127.0.0.1:8080";
  popup.values.connectionMode="local";
  delete popup.values.token;
  release();
  await pairing;
  assert.equal(popup.values.gateway,"http://127.0.0.1:8080");
  assert.equal(popup.values.connectionMode,"local");
  assert.equal(popup.values.token,undefined);
});

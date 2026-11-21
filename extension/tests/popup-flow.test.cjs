const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadPopup({failCookieDomain = null, includeSecondProvider = false, manualVerification = false, localRelay = false, activation = null, activationIssue = null, diagnosticsSync = null, diagnosticsFailure = null, pairedDisconnectReject = false, pairedConnectReject = false, remotePairToken = "b".repeat(64), syncStatus = undefined} = {}) {
  const elements = new Map();
  const chromeCalls = [];
  const intervals = [];
  const connectDeadlines = new Map();
  const clearedConnectDeadlines = new Set();
  let nextConnectDeadlineId = 0;
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
    if(request.type==="reserve-pending-approval") {
      if(values.pendingApprovalRequest || values.pendingConnection) return {ok:false};
      values.pendingApprovalRequest={gateway:request.gateway,generation:"f".repeat(32)};
      return {ok:true,generation:"f".repeat(32)};
    }
    if(request.type==="install-pending-approval") {
      if(request.generation!==values.pendingApprovalRequest?.generation ||
          request.gateway!==values.pendingApprovalRequest.gateway) return {ok:false};
      const pending={gateway:request.gateway,request_id:request.request_id,
        poll_secret:request.poll_secret,expires:Date.now()+300000,
        approvalGeneration:request.generation};
      values.pendingConnection=pending;
      values.pendingApprovalRequest=null;
      return {ok:true,pending:{gateway:pending.gateway,request_id:pending.request_id,expires:pending.expires}};
    }
    if(request.type==="release-pending-approval") {
      if(values.pendingApprovalRequest?.generation===request.generation) values.pendingApprovalRequest=null;
      return {ok:true,released:true};
    }
    if(request.type==="relay-diagnostics" && diagnosticsFailure) return diagnosticsFailure;
    if(request.type==="relay-diagnostics" && diagnosticsSync) {
      return {ok:false,extension:{provider_sync:{metaai:diagnosticsSync}}};
    }
    if(request.type==="invalidate-paired-relay") {
      const expected=request.connection;
      const valid=expected && values.token && values.connectionMode!=="local" &&
        values.gateway===expected.gateway && values.token===expected.token &&
        values.connectionMode===expected.connectionMode &&
        (values.relayConnectionEpoch||null)===(expected.relayConnectionEpoch||null);
      if(!valid) return {ok:true,invalidated:false};
      values.token=null;
      values.connectionMode=null;
      values.relayConnectionEpoch="revoked-epoch";
      values.linkedProviders=[];
      values.providerLinkEpochs={};
      values.providerLinkPendingAttempts={};
      values.relaySnapshotVersions={};
      return {ok:true,invalidated:true};
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
      if(pairedConnectReject) return {ok:false,error:"Paired connection changed while pairing."};
      const expected=request.expected;
      if(expected.gateway!==values.gateway || expected.token!==values.token ||
          expected.connectionMode!==values.connectionMode ||
          expected.relayConnectionEpoch!==values.relayConnectionEpoch) {
        return {ok:false,error:"Paired connection changed while pairing."};
      }
      values.gateway=request.gateway;
      values.token=request.token;
      values.connectionMode="paired";
      values.relayConnectionEpoch="new-remote-epoch";
      values.linkedProviders=[];
      values.pendingConnection=null;
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
    if(request.type==="sync-provider") {server.linked=true;return {ok:true,syncStatus};}
    return {ok:true};
  }};
  const context = {
    Promise, Date, Set, Array, AbortController, URL,
    setTimeout(fn,ms) {
      const id = ++nextConnectDeadlineId;
      connectDeadlines.set(id,{fn,ms});
      return id;
    },
    clearTimeout(id) {clearedConnectDeadlines.add(id);},
    randomHex: (() => { let counter = 0; return () => "test-epoch-" + (++counter); })(),
    confirm:()=>true,
    document:{getElementById:byId,createElement:node},
    window:{addEventListener(){}},
    setInterval(fn,ms){intervals.push({fn,ms});},
    isLocalGateway: value => value.startsWith("http://127.0.0.1:") || value.startsWith("http://localhost:"),
    hasRelayConnection: state => Boolean(state.gateway && (state.token || state.connectionMode === "local")),
    normalizeGateway: v=>v,
    PROVIDER_LOGIN_URLS:{metaai:"https://www.meta.ai/",manus:"https://manus.im/app"},
    PROVIDERS:{metaai:["meta.ai"],claude:["claude.ai"],manus:["manus.im"]},
    providerCookieSnapshot:async()=>({cookies:[{name:"token",value:"test-only"}]}),
    relayFetch:async()=>{chromeCalls.push("http:relay-status");return {ok:true,status:200,json:async()=>({
      providers:[
        {provider:"metaai",enabled:true,linked:server.linked,authentication:server.linked?"unknown":"needs_login",
          availability:manualVerification?"degraded":"unknown",manual_verification_required:manualVerification,activation,activation_issue:activationIssue},
        ...(includeSecondProvider ? [{provider:"claude",enabled:true,linked:false,authentication:"needs_login",availability:"unknown"}] : []),
      ],
      vault_mode:true,
    })};},
    fetch:async url => ({ok:true,json:async()=>url.endsWith("/relay/v1/mode")
      ? {mode:localRelay?"local":"paired",no_pairing:localRelay}
      : url.endsWith("/relay/v1/pair") ? {token:remotePairToken}
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
        async get(keys){
          const names=Array.isArray(keys) ? keys : [keys];
          return Object.fromEntries(names.map(k=>[k,values[k]]));
        },
        async set(next){Object.assign(values,next);},
        async remove(keys){for(const k of Array.isArray(keys)?keys:[keys])delete values[k];},
      }},
    },
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname,"..","credential_probe.js"),"utf8"),context);
  vm.runInContext(fs.readFileSync(path.join(__dirname,"..","popup.js"),"utf8")+
    "\nglobalThis.testControls={refreshStatus,connectProvider,logoutProvider,initialize,connectLocalGateway,popupConnectionJson,safeStatusErrorMessage,safeProviderActionError,safeGatewayActionError,verifyChatgptActiveTab,compareChatgptBrowserAndGateway,safeWarmChatgptAccountRecord,safeLastFailedRequest};",context);
  return {values,server,elements,byId,chromeCalls,intervals,connectDeadlines,clearedConnectDeadlines,controls:context.testControls,context};
}

test("ChatGPT active-tab first-party proof rejects empty user shape without leaking session", async () => {
  // Isolate the explicit script: the popup itself has unrelated async polling
  // that must never count as first-party account-verifier network traffic.
  const context = {
    Date, Promise, AbortController, setTimeout, clearTimeout,
    location:{protocol:"https:",hostname:"chatgpt.com",port:"",href:"https://chatgpt.com/"}
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname,"..","credential_probe.js"),"utf8"),context);
  const future = () => new Date(Date.now() + 60_000).toISOString();
  const session = user => ({user,accessToken:"private-fixture-token",expires:future()});
  for(const [payload,status,authentication,reason] of [
    [session({}),200,"unknown","incomplete_or_expired_session_evidence"],
    [session({fixture:true}),200,"verified","first_party_session_confirmed"],
    [{},200,"unauthenticated","provider_reports_no_session"],
    [{user:null},200,"unauthenticated","provider_reports_no_session"],
    [session({fixture:true}),401,"unauthenticated","provider_rejected_session"],
    [session({fixture:true}),403,"unknown","provider_or_network_blocked"],
  ]) {
    let fetchCount = 0;
    context.fetch = async (url,options) => {
      ++fetchCount;
      assert.equal(url,"/api/auth/session");
      assert.equal(options.credentials,"include");
      assert.equal(options.redirect,"error");
      return {status,json:async()=>payload};
    };
    const result = await vm.runInContext("probeActiveChatgptAccount()",context);
    assert.equal(result.authentication,authentication);
    assert.equal(result.reason,reason);
    assert.equal(result.exported_snapshot_verified,false);
    assert.equal(fetchCount,1);
    const safe = JSON.stringify(result);
    assert(!safe.includes("private-fixture-token"));
    assert(!safe.includes("fixture:true"));
  }
});

test("standalone ChatGPT probe agrees that an empty user cannot verify credentials", async () => {
  const code = fs.readFileSync(path.join(__dirname,"..","..","scripts","provider_credential_probe.js"),"utf8");
  const ctx = {
    location:{protocol:"https:",hostname:"chatgpt.com",port:"",href:"https://chatgpt.com/"},
    Date,AbortController,Promise,setTimeout,clearTimeout,
    fetch:async()=>({status:200,json:async()=>({
      user:{},accessToken:"private-fixture-token",expires:new Date(Date.now()+60000).toISOString()
    })})
  };
  const partial = await vm.runInNewContext(code,ctx);
  assert.equal(partial.authentication,"unknown");
  assert.equal(partial.reason,"incomplete_or_expired_session_evidence");
  assert(!JSON.stringify(partial).includes("private-fixture-token"));
  ctx.fetch = async()=>({status:200,json:async()=>({
    user:{fixture:true},accessToken:"private-fixture-token",expires:new Date(Date.now()+60000).toISOString()
  })});
  const positive = await vm.runInNewContext(code,ctx);
  assert.equal(positive.authentication,"verified");
  assert.equal(positive.exported_snapshot_verified,false);
});

test("popup status refresh and local startup reject raw credential-bearing exceptions", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  const leak = "fetch rejected url=https://private.invalid/status?token=private&poll_secret=private";
  popup.context.relayFetch = async () => {throw new Error(leak);};
  await popup.byId("refresh").handlers.click();
  assert.equal(popup.byId("message").textContent,
    "Gateway status unavailable. Check the connection and retry.");
  assert.ok(!popup.byId("message").textContent.includes("private"));

  // Preserve only exact source-authored diagnostic messages. Neither a
  // malicious suffix/prefix nor an arbitrary raw HTTP body is ever safe.
  const safe = popup.controls.safeStatusErrorMessage;
  assert.equal(safe(new Error("Gateway status check timed out.")),
    "Gateway status check timed out.");
  for (const payload of [
    leak, "Gateway status check timed out. token=private",
    "Status request failed (401). token=private", "private URL credential"
  ]) {
    assert.equal(safe(new Error(payload)),
      "Gateway status unavailable. Check the connection and retry.");
  }

  const initial = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  initial.values.localDisconnected = true;
  initial.values.connectionMode = "paired";
  initial.values.token = "synthetic-credential";
  initial.context.relayFetch = async () => {throw new Error(leak);};
  await initial.controls.initialize();
  assert.equal(initial.byId("message").textContent,
    "Gateway status unavailable. Check the connection and retry.");

  // A local mode probe can throw before any explicit status fetch.
  const local = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  local.context.fetch = async () => {throw new Error(leak);};
  await local.controls.initialize();
  assert.equal(local.byId("message").textContent,
    "Local gateway connection unavailable. Check the gateway and retry.");
  assert.ok(!local.byId("message").textContent.includes("private"));
});

test("popup pending connection status never renders untrusted worker or storage secrets", async () => {
  for (const [response, expected] of [
    [{ok:false,error:"token=private secret_url=https://secret.invalid"}, /worker did not respond/],
    [{ok:false,error:"Gateway permission is missing."}, /Gateway permission is missing/],
    [{ok:false,error:"Gateway permission is missing. token=private"}, /worker did not respond/],
    [{ok:false,errorCode:"connection_poll_unavailable",
      error:"poll_secret=private-request"}, /approval check unavailable/],
    [undefined, /worker did not respond/]
  ]) {
    const popup = loadPopup();
    await new Promise(resolve => setImmediate(resolve));
    popup.values.pendingConnection = {gateway:"http://127.0.0.1:8080",
      request_id:"a".repeat(64),poll_secret:"b".repeat(64),expires:Date.now()+30_000};
    const original = popup.context.chrome.runtime.sendMessage;
    popup.context.chrome.runtime.sendMessage = async request =>
      request.type==="poll-connection" ? response : original(request);
    assert.equal(await popup.context.checkConnection(),false);
    const rendered = popup.byId("message").textContent;
    assert.match(rendered,expected);
    assert.ok(!rendered.includes("private"));
    assert.ok(!rendered.includes("poll_secret"));
    assert.ok(!rendered.includes("secret.invalid"));
  }

  const thrown = loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  thrown.context.chrome.runtime.sendMessage = async () => {
    throw new Error("fetch rejected token=private gateway=https://secret.invalid");
  };
  assert.equal(await thrown.context.checkConnection(),false);
  assert.equal(thrown.byId("message").textContent,
    "Gateway connection check unavailable. Retry or reopen the extension.");

  const storage = loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  storage.context.chrome.storage.local.get = async () => {
    throw new Error("storage failed with poll_secret=private");
  };
  assert.equal(await storage.context.checkConnection(),false);
  assert.equal(storage.byId("message").textContent,
    "Gateway connection check unavailable. Retry or reopen the extension.");
});

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
  assert.equal(paired.values.token,null, "paired authorization revocation must still clear current local token");
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

test("provider link and logout buttons never display raw worker or Chrome errors", async () => {
  const popup = loadPopup();
  popup.values.token="c".repeat(64);
  await popup.controls.refreshStatus();
  const controls=()=>popup.byId("status").children[0]
    .children.find(n=>n.className==="provider-actions").children;
  const link=controls().find(b=>b.textContent==="Log in / Connect");
  const secret="Chrome runtime failed https://private.invalid/relay?token=private&poll_secret=private";
  const publicLinkError="Provider connection failed. Check browser permissions and gateway status, then retry.";

  popup.context.chrome.permissions.request=async()=>{throw new Error(secret);};
  await link.handlers.click();
  assert.equal(popup.byId("message").textContent,publicLinkError);
  assert.deepEqual(Array.from(popup.values.linkedProviders),[],
    "failed permission request must not link provider consent");

  popup.context.chrome.permissions.request=async()=>false;
  await link.handlers.click();
  assert.equal(popup.byId("message").textContent,"Provider permissions were not granted.");

  popup.context.chrome.permissions.request=async()=>{throw new Error("Provider permissions were not granted. token=private");};
  await link.handlers.click();
  assert.equal(popup.byId("message").textContent,publicLinkError);

  // Existing popup can receive a response from an old extension worker.
  // Treat its arbitrary returned error as untrusted even after the new worker
  // API was changed to emit only fixed diagnostic codes.
  popup.server.linked=true;
  popup.values.linkedProviders=["metaai"];
  await popup.controls.refreshStatus();
  const logout=controls().find(b=>b.textContent==="Log out");
  const original=popup.context.chrome.runtime.sendMessage;
  popup.context.chrome.runtime.sendMessage=async request=>
    request.type==="logout-provider" ? {ok:false,error:secret} : original(request);
  await logout.handlers.click();
  assert.equal(popup.byId("message").textContent,
    "Provider logout unavailable. Check gateway status and retry.");
  assert.ok(!popup.byId("message").textContent.includes("private"));
  assert.deepEqual(Array.from(popup.values.linkedProviders),["metaai"]);

  const safe=popup.controls.safeProviderActionError;
  assert.equal(safe(new Error("Gateway sync timed out. Check the connection and retry."),"fallback"),
    "Gateway sync timed out. Check the connection and retry.");
  assert.equal(safe(new Error("Gateway sync timed out. Check the connection and retry. token=private"),"fallback"),
    "fallback");
  assert.equal(safe(new Error(secret),"fallback"),"fallback");
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

test("dashboard separates typed pending and failed activation reasons from login verification", async () => {
  for (const [phase, reason, detail] of [
    ["pending", "wait_expired", "Activation detail: wait expired"],
    ["failed", "browser_worker_unavailable", "Activation detail: browser worker unavailable"],
    ["failed", "refresh_failed", "Activation detail: refresh failed"],
    ["failed", "provider_session_expired", "Activation detail: provider session expired"],
    ["failed", "provider_session_no_applicable_material",
      "Activation detail: provider session no applicable material"],
    ["failed", "manual_verification_required", "Activation detail: manual verification required"]
  ]) {
    const popup=loadPopup({activation:phase, activationIssue:reason});
    popup.server.linked=true;
    await popup.controls.refreshStatus();
    const detailText=popup.byId("status").children[0].children
      .find(child=>child.className==="provider-detail").textContent;
    assert.ok(detailText.includes(detail));
    assert.ok(detailText.includes("Login unverified"), "activation cannot verify credentials");
    assert.ok(!detailText.includes("Login needed"), "activation cannot demand re-login");
    assert.equal(popup.chromeCalls.filter(call=>call==="sync-provider:metaai").length,0);
  }
});

test("activation diagnostics fail closed for contradictory or untrusted status", async () => {
  const secret="https://private.invalid/?token=secret&poll_secret=private";
  for (const [phase, reason, expected] of [
    ["failed", "wait_expired", "Activation detail: unverified status"],
    ["pending", "provider_session_expired", "Activation detail: unverified status"],
    ["pending", "refresh_failed", "Activation detail: unverified status"],
    ["failed", "refresh_failed "+secret, "Activation detail: unverified status"],
    ["failed", {error:secret}, "Activation detail: unverified status"],
    ["failed", 42, "Activation detail: unverified status"],
    ["ready", "provider_session_expired", null],
    ["cancelled", "refresh_failed", null],
    ["not_attempted", "refresh_failed", null]
  ]) {
    const popup=loadPopup({activation:phase,activationIssue:reason});
    popup.server.linked=true;
    await popup.controls.refreshStatus();
    const detailText=popup.byId("status").children[0].children
      .find(child=>child.className==="provider-detail").textContent;
    if(expected===null) assert.ok(!detailText.includes("Activation detail:"));
    else assert.ok(detailText.includes(expected));
    for(const token of ["private.invalid", "poll_secret", "token=secret"]) {
      assert.ok(!detailText.includes(token), "never echo raw activation errors");
    }
    assert.ok(detailText.includes("Login unverified"));
  }
  for (const linked of [false, true]) {
    const popup=loadPopup({activation:"failed",activationIssue:"refresh_failed"});
    popup.server.linked=linked;
    if(linked) {
      popup.context.relayFetch=async()=>({ok:true,status:200,json:async()=>({
        providers:[{provider:"metaai",enabled:false,linked:true,authentication:"unknown",
          activation:"failed",activation_issue:"refresh_failed"}],vault_mode:true
      })});
    }
    await popup.controls.refreshStatus();
    const detail=popup.byId("status").children[0].children
      .find(child=>child.className==="provider-detail").textContent;
    assert.ok(!detail.includes("Activation detail:"),
      "unlinked or disabled providers cannot display stale activation errors");
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

test("popup renders only the fixed partition-scope sync diagnosis", async () => {
  const popup = loadPopup({diagnosticsSync:{
    state:"failed",issue:"partition_scope_unsupported",
    updated_at:"2026-11-17T23:00:00.000Z",stale:true
  }});
  await new Promise(resolve=>setImmediate(resolve));
  await popup.controls.refreshStatus();
  const row = popup.byId("status").children[0];
  const detail = row.children.find(n=>n.className==="provider-detail").textContent;
  assert.match(detail,/Extension sync: partition scope unsupported/);
  assert.doesNotMatch(detail,/topLevelSite|cookie=secret|private.example/);
});

test("popup shows canonical UTC sync timestamps only, never raw worker strings", async () => {
  const shown = async updated_at => {
    const popup=loadPopup({diagnosticsSync:{state:"failed",issue:"gateway_timeout",updated_at}});
    await new Promise(resolve=>setImmediate(resolve));
    await popup.controls.refreshStatus();
    return popup.byId("status").children[0]
      .children.find(n=>n.className==="provider-detail").textContent;
  };
  const canonical="2026-11-15T18:00:00.000Z";
  const valid=await shown(canonical);
  assert.ok(valid.includes("Last extension sync attempt: "+canonical));
  assert.ok(valid.includes("Extension sync: gateway timeout"));

  for (const bad of [
    canonical+" token=private",
    "Nov 15 2026",
    "2026-11-15T18:00:00Z",
    "2026-11-15T18:00:00.000+00:00",
    "2026-02-30T18:00:00.000Z",
    1763239200000,
    {text:"secret"},
    null
  ]) {
    const detail=await shown(bad);
    assert.ok(!detail.includes("Last extension sync attempt:"),
      "invalid date must not display any raw timestamp");
    assert.ok(!detail.includes("private"));
    assert.ok(!detail.includes("secret"));
    assert.ok(detail.includes("Extension sync: gateway timeout"),
      "invalid timestamp does not suppress otherwise valid typed diagnostics");
  }
});

test("provider dashboard renders only exact known issue codes from gateway and worker", async () => {
  const view = async (gatewayIssue, extensionIssue) => {
    const popup = loadPopup({includeSecondProvider:true,diagnosticsSync:{
      state:"failed",issue:extensionIssue,updated_at:"2026-11-15T18:00:00.000Z"
    }});
    await new Promise(resolve=>setImmediate(resolve));
    popup.context.relayFetch = async () => ({ok:true,status:200,json:async()=>({
      providers:[
        {provider:"metaai",enabled:true,linked:true,authentication:"unknown",
          availability:"degraded",issue:gatewayIssue},
        {provider:"claude",enabled:true,linked:false,authentication:"needs_login",
          availability:"unknown",issue:"session_not_linked"}
      ],vault_mode:true
    })});
    await popup.controls.refreshStatus();
    const rows=popup.byId("status").children;
    const detail=rows[0].children.find(x=>x.className==="provider-detail").textContent;
    const healthy=rows[1].children.find(x=>x.className==="provider-detail").textContent;
    assert.match(healthy,/Gateway issue: session not linked/,
      "a bad issue for one provider must not corrupt another row");
    return detail;
  };

  const known=await view("provider_document_denied","gateway_timeout");
  assert.match(known,/Gateway issue: provider document denied/);
  assert.match(known,/Extension sync: gateway timeout/);
  assert.match(known,/Login unverified/,
    "issue codes do not establish authenticated login");

  const leaked="token=private https://private.invalid/relay?poll_secret=secret";
  const untrusted=await view("provider_document_denied "+leaked,leaked);
  assert.match(untrusted,/Gateway issue: unverified status/);
  assert.match(untrusted,/Extension sync: unverified status/);
  assert.ok(!untrusted.includes("private"));
  assert.ok(!untrusted.includes("poll_secret"));
  assert.ok(!untrusted.includes("token="));

  for(const malformed of [42,{token:"secret"},"provider_degraded token=secret"]) {
    const detail=await view(malformed,malformed);
    assert.match(detail,/Gateway issue: unverified status/);
    assert.match(detail,/Extension sync: unverified status/);
    assert.ok(!detail.includes("secret"));
  }
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
  assert.equal(failed.children[0].textContent,"Meta AI");
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



test("Pair and Unpair popup failures never render raw gateway secrets", async () => {
  const leak="fetch failed https://private.example/relay/v1/unpair?token=private&poll_secret=private";
  const paired=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  paired.byId("gateway").value="https://private.example";
  paired.byId("code").value="12345678";
  paired.context.fetch=async()=>{throw new Error(leak);};
  await paired.byId("pair").handlers.click();
  assert.equal(paired.byId("message").textContent,
    "Gateway connection request failed. Check browser permissions and retry.");
  assert.equal(paired.values.token,undefined);
  assert.equal(paired.values.gateway,"http://127.0.0.1:8080");
  assert.ok(!paired.byId("message").textContent.includes("private.example"));

  // Existing fixed user guidance remains intact for invalid pairing codes.
  paired.byId("code").value="not-eight";
  await paired.byId("pair").handlers.click();
  assert.equal(paired.byId("message").textContent,
    "Enter the fresh 8-digit code from your private remote gateway.");

  const staleWorker=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  staleWorker.byId("gateway").value="https://private.example";
  staleWorker.byId("code").value="12345678";
  const send=staleWorker.context.chrome.runtime.sendMessage;
  staleWorker.context.chrome.runtime.sendMessage=async msg =>
    msg.type==="connect-paired-relay" ? {ok:false,error:leak} : send(msg);
  await staleWorker.byId("pair").handlers.click();
  assert.equal(staleWorker.byId("message").textContent,
    "Gateway connection request failed. Check browser permissions and retry.");
  assert.equal(staleWorker.values.token,undefined);

  const unpair=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  unpair.values.gateway="https://private.example";
  unpair.values.connectionMode="paired";
  unpair.values.token="c".repeat(64);
  unpair.values.linkedProviders=["metaai"];
  unpair.context.relayFetch=async()=>{throw new Error(leak);};
  await unpair.byId("unpair").handlers.click();
  assert.equal(unpair.byId("message").textContent,
    "Gateway disconnect failed. Check the connection and retry.");
  assert.equal(unpair.values.token,"c".repeat(64));
  assert.deepEqual(Array.from(unpair.values.linkedProviders),["metaai"]);
  assert.ok(!unpair.chromeCalls.some(x=>x==="disconnect-paired-relay:"));
  assert.ok(!unpair.byId("message").textContent.includes("poll_secret"));

  const safe=unpair.controls.safeGatewayActionError;
  assert.equal(safe(new Error("Could not revoke the install token."),"fallback"),
    "Could not revoke the install token.");
  for(const message of [leak, "Could not revoke the install token. token=private",
      "Paired gateway connection changed before disconnect. account=private"]) {
    assert.equal(safe(new Error(message),"fallback"),"fallback");
  }
});

test("remote legacy Pair uses worker-owned generation-fenced commit",async()=>{
  const popup=loadPopup();
  await Promise.resolve();
  popup.elements.get("gateway").value="https://private.example";
  popup.byId("code").value="12345678";
  await popup.elements.get("pair").handlers.click();
  assert.equal(popup.values.gateway,"https://private.example");
  assert.equal(popup.values.connectionMode,"paired");
  assert.equal(popup.values.token,"b".repeat(64));
  assert.ok(popup.chromeCalls.some(x=>x==="connect-paired-relay:"));
  assert.equal(popup.byId("code").value,"");
});

test("remote Pair worker rejection never becomes a popup-side connection write",async()=>{
  const popup=loadPopup({pairedConnectReject:true});
  await Promise.resolve();
  popup.elements.get("gateway").value="https://private.example";
  popup.byId("code").value="12345678";
  await popup.elements.get("pair").handlers.click();
  assert.equal(popup.values.token,undefined);
  assert.equal(popup.values.gateway,"http://127.0.0.1:8080");
  assert.equal(popup.values.linkedProviders.length,0);
  assert.match(popup.elements.get("message").textContent,/connection changed/);
});

test("remote Pair rejects malformed worker token before it can be committed",async()=>{
  const popup=loadPopup({remotePairToken:"not-a-token"});
  await Promise.resolve();
  popup.elements.get("gateway").value="https://private.example";
  popup.byId("code").value="12345678";
  await popup.elements.get("pair").handlers.click();
  assert.equal(popup.values.token,undefined);
  assert.ok(!popup.chromeCalls.some(x=>x==="connect-paired-relay:"));
  assert.match(popup.elements.get("message").textContent,/Invalid connection response/);
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


test("stale remote 401 must not revoke a newer paired install",async()=>{
  const popup=loadPopup();
  popup.values.gateway="https://first.example";
  popup.values.token="a".repeat(64);
  popup.values.connectionMode="paired";
  popup.values.relayConnectionEpoch="gen-a";
  let release, started;
  const waiting=new Promise(resolve=>{started=resolve;});
  const gate=new Promise(resolve=>{release=resolve;});
  const original=popup.context.chrome.runtime.sendMessage;
  popup.context.chrome.runtime.sendMessage=async request=>{
    if(request.type==="relay-diagnostics") {
      started();
      return gate;
    }
    return original(request);
  };
  const pending=popup.controls.refreshStatus();
  await waiting;
  popup.values.gateway="https://new.example";
  popup.values.token="b".repeat(64);
  popup.values.connectionMode="paired";
  popup.values.relayConnectionEpoch="gen-b";
  popup.values.linkedProviders=["metaai"];
  release({ok:false,gateway_issue:"gateway_unauthorized"});
  await assert.rejects(pending,/connection changed during status check/);
  assert.equal(popup.values.gateway,"https://new.example");
  assert.equal(popup.values.token,"b".repeat(64));
  assert.deepEqual(Array.from(popup.values.linkedProviders),["metaai"]);
  assert.ok(popup.chromeCalls.includes("invalidate-paired-relay:"));
});

test("paired 401 worker unavailable cannot trigger popup-side token deletion",async()=>{
  const popup=loadPopup({diagnosticsFailure:{ok:false,gateway_issue:"gateway_unauthorized"}});
  popup.values.gateway="https://private.example";
  popup.values.token="a".repeat(64);
  popup.values.connectionMode="paired";
  const original=popup.context.chrome.runtime.sendMessage;
  popup.context.chrome.runtime.sendMessage=async request=>
    request.type==="invalidate-paired-relay" ? {ok:false} : original(request);
  await assert.rejects(()=>popup.controls.refreshStatus(),/could not be applied safely/);
  assert.equal(popup.values.token,"a".repeat(64));
  assert.ok(popup.chromeCalls.filter(x=>x==="http:relay-status").length===0);
});

test("local 401 remains an actionable mismatch without worker invalidation",async()=>{
  const popup=loadPopup({diagnosticsFailure:{ok:false,gateway_issue:"gateway_unauthorized"}});
  popup.values.gateway="http://127.0.0.1:8080";
  popup.values.connectionMode="local";
  popup.values.linkedProviders=["metaai"];
  await assert.rejects(()=>popup.controls.refreshStatus(),/Local relay access rejected/);
  assert.deepEqual(Array.from(popup.values.linkedProviders),["metaai"]);
  assert.equal(popup.chromeCalls.filter(x=>x==="invalidate-paired-relay:").length,0);
});

test("popup uses independent 30-second connected and 5-second approval timers",async()=>{
  const popup=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  assert.deepEqual(popup.intervals.map(x=>x.ms),[30_000,5_000]);
  const connected=popup.intervals.find(x=>x.ms===30_000);
  const approval=popup.intervals.find(x=>x.ms===5_000);
  popup.chromeCalls.length=0;
  await connected.fn();
  await approval.fn();
  assert.equal(popup.chromeCalls.length,0,
    "disconnected popup must not poll or request diagnostics every 10 seconds");

  popup.values.connectionMode="local";
  await connected.fn();
  assert.equal(popup.chromeCalls.filter(x=>x==="relay-diagnostics:").length,1,
    "an active local dashboard still checks gateway status within 30 seconds");
  popup.chromeCalls.length=0;
  await approval.fn();
  assert.equal(popup.chromeCalls.length,0,
    "five-second watcher does not poll when there is no pending approval");
});

test("pending approval gets fast completion checks without a duplicate connected heartbeat",async()=>{
  const popup=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  popup.values.pendingConnection={gateway:"http://127.0.0.1:8080",
    request_id:"a".repeat(64),poll_secret:"b".repeat(64),expires:Date.now()+300_000};
  popup.chromeCalls.length=0;
  await popup.intervals.find(x=>x.ms===30_000).fn();
  assert.equal(popup.chromeCalls.length,0,
    "the connected timer must defer to the pending approval watcher");
  await popup.intervals.find(x=>x.ms===5_000).fn();
  assert.equal(popup.chromeCalls.filter(x=>x==="poll-connection:").length,1,
    "approval polling stays bounded at five seconds");
});

test("popup timer storage read errors do not cause unhandled worker requests",async()=>{
  const popup=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  popup.context.chrome.storage.local.get=async()=>{throw new Error("storage unavailable")};
  popup.chromeCalls.length=0;
  await Promise.all(popup.intervals.map(x=>x.fn()));
  assert.equal(popup.chromeCalls.length,0);
});


test("popup rejects inherited provider names without requesting permissions or opening URLs", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  for (const invalid of ["constructor","toString","__proto__",["metaai"],null]) {
    await assert.rejects(() => popup.controls.connectProvider(invalid), /Unsupported provider/);
    await assert.rejects(() => popup.controls.logoutProvider(invalid), /Unsupported provider/);
    await assert.rejects(
      () => vm.runInContext("openProviderLogin(" + JSON.stringify(invalid) + ")", popup.context),
      /not yet supported/
    );
  }
  assert.ok(!popup.chromeCalls.some(call => call.startsWith("tab:")),
    "unsupported names must not open a provider or inherited URL");
});


test("popup quarantines malformed provider rows and refuses truthy fake saved-session flags", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  let cookieChecks = 0;
  popup.context.chrome.cookies.getAll = async () => {
    cookieChecks++;
    return [];
  };
  popup.context.relayFetch = async () => ({ok:true,status:200,json:async()=>({
    vault_mode:true,
    providers:[
      null, "invalid row",
      {provider:"constructor",linked:true,enabled:true},
      {provider:["metaai"],linked:true,enabled:true},
      {provider:"metaai",linked:"false",enabled:"true",
        manual_verification_required:"false",authentication:"unknown"},
      {provider:"claude",linked:true,enabled:true,
        manual_verification_required:false,authentication:"verified"}
    ]
  })});
  await popup.controls.refreshStatus(true);
  const rows = popup.byId("status").children;
  assert.equal(rows.length,2,"invalid rows must not suppress valid providers");
  assert.equal(rows[0].children[0].textContent,"Meta AI");
  assert.equal(rows[1].children[0].textContent,"claude");
  const metaStatus = rows[0].children[1].textContent;
  assert.match(metaStatus,/Gateway enablement unverified/);
  assert.match(metaStatus,/Not linked/);
  assert.doesNotMatch(metaStatus,/Session saved|Manual provider verification required/);
  const metaActions = rows[0].children[2].children;
  assert.equal(metaActions.length,2,"fake saved-session flag must not show Log out");
  assert.equal(metaActions[0].disabled,true,"unknown enabled flag cannot authorize linking");
  assert.equal(metaActions[0].textContent,"Log in / Connect");
  const claudeStatus = rows[1].children[1].textContent;
  assert.match(claudeStatus,/Enabled in gateway/);
  assert.match(claudeStatus,/Session saved/);
  assert.equal(rows[1].children[2].children.length,3);
  assert.equal(rows[1].children[2].children[2].textContent,"Log out");
  assert.equal(cookieChecks,2,"only the two registered provider rows may check cookies");
});

test("popup treats non-array gateway provider status as fixed unavailable error", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  const initialRows = popup.byId("status").children;
  for (const malformed of [null, {}, {providers:{}}, {providers:"private credential"}, {providers:false}]) {
    popup.context.relayFetch = async () => ({ok:true,status:200,json:async()=>malformed});
    await assert.rejects(
      () => popup.controls.refreshStatus(true),
      error => error.message === "Gateway status is unavailable."
    );
    assert.equal(popup.byId("status").children,initialRows,
      "malformed refresh must not replace existing healthy rows");
  }
});


test("duplicate Manus status rows cannot display conflicting saved-session controls", async () => {
  const popup=loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  let cookieReads=0;
  popup.context.chrome.cookies.getAll=async()=>{cookieReads++;return [];};
  const typed = (provider,linked) => ({
    provider,enabled:true,linked,
    authentication:linked?"verified":"needs_login",manual_verification_required:false
  });
  popup.context.relayFetch=async()=>({ok:true,status:200,json:async()=>({
    vault_mode:true,
    providers:[typed("manus",true), typed("manus",false), typed("metaai",false),
      null, {provider:"constructor",linked:true,enabled:true}]
  })});
  await popup.controls.refreshStatus(true);
  const rows=popup.byId("status").children;
  assert.equal(rows.length,1,"both conflicting Manus copies must be quarantined");
  assert.equal(rows[0].children[0].textContent,"Meta AI");
  assert.equal(rows[0].children[2].children[0].textContent,"Log in / Connect");
  assert.equal(cookieReads,1,"do not inspect browser cookies for ambiguous Manus rows");

  popup.context.relayFetch=async()=>({ok:true,status:200,json:async()=>({
    vault_mode:true,
    providers:[typed("manus",true), typed("manus",true), typed("claude",false)]
  })});
  await popup.controls.refreshStatus(true);
  const second=popup.byId("status").children;
  assert.equal(second.length,1,"even identical duplicate status rows are not unique");
  assert.equal(second[0].children[0].textContent,"claude");
  assert.equal(cookieReads,2);

  popup.context.relayFetch=async()=>({ok:true,status:200,json:async()=>({
    vault_mode:true,
    providers:[typed("manus",true),typed("metaai",false)]
  })});
  await popup.controls.refreshStatus(true);
  const unique=popup.byId("status").children;
  assert.equal(unique.length,2,"unique Manus remains available");
  assert.equal(unique[0].children[0].textContent,"manus");
  assert.match(unique[0].children[1].textContent,/Session saved/);
  assert.equal(unique[0].children[2].children.length,3,"unambiguous linked Manus permits Log out");
  assert.equal(unique[0].children[2].children[2].textContent,"Log out");
});

async function currentConnectionDeadline(popup) {
  for (let attempt = 0; attempt < 16; attempt++) {
    const active = [...popup.connectDeadlines.entries()].find(([id, timer]) =>
      timer.ms === 12_000 && !popup.clearedConnectDeadlines.has(id));
    if (active) return active;
    await new Promise(resolve => setImmediate(resolve));
  }
  assert.fail("popup connection request never registered its deadline");
}

test("stalled local gateway mode probe rejects and preserves disconnected state", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  let seen;
  popup.context.fetch = async (_url, options) => {
    seen = options;
    return new Promise(() => {});
  };
  const task = popup.controls.connectLocalGateway("http://127.0.0.1:8080", true);
  const [id, timer] = await currentConnectionDeadline(popup);
  assert.equal(seen.redirect, "error");
  assert.equal(seen.signal.aborted, false);
  timer.fn();
  await assert.rejects(task, /Gateway connection request timed out/);
  assert.equal(seen.signal.aborted, true);
  assert.equal(popup.clearedConnectDeadlines.has(id), true);
  assert.equal(popup.values.connectionMode, undefined);
  assert.ok(!popup.chromeCalls.some(x=>x.startsWith("connect-local-relay:")));
});

test("successful local mode headers with stuck JSON body are bounded", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  let signal;
  popup.context.fetch = async (_url, options) => {
    signal = options.signal;
    return {ok:true,status:200,json:async()=>new Promise(()=>{})};
  };
  const task = popup.controls.connectLocalGateway("http://127.0.0.1:8080", true);
  const [id,timer] = await currentConnectionDeadline(popup);
  timer.fn();
  await assert.rejects(task, /Gateway connection request timed out/);
  assert.equal(signal.aborted, true);
  assert.equal(popup.clearedConnectDeadlines.has(id), true);
  assert.equal(popup.values.connectionMode, undefined);
});

test("local Pair network timeout releases only its own provisional approval", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  const requests = [];
  popup.context.fetch = async (url, options) => {
    requests.push({url,options});
    if (url.endsWith("/relay/v1/mode")) return {
      ok:true,status:200,json:async()=>({no_pairing:false})
    };
    return new Promise(() => {});
  };
  const task = popup.byId("pair").handlers.click();
  let timer;
  for (let index = 0; index < 20; index++) {
    const latest = [...popup.connectDeadlines.entries()].find(([id,entry]) =>
      entry.ms === 12_000 && !popup.clearedConnectDeadlines.has(id) &&
      requests.some(req=>req.url.endsWith("/relay/v1/connect/request")));
    if (latest) { timer=latest; break; }
    await new Promise(resolve=>setImmediate(resolve));
  }
  assert.ok(timer, "a reserved local Pair request must have the timeout");
  assert.equal(popup.values.pendingApprovalRequest.generation, "f".repeat(32));
  timer[1].fn();
  await task;
  assert.equal(popup.clearedConnectDeadlines.has(timer[0]), true);
  assert.equal(popup.values.pendingApprovalRequest, null,
    "worker must release the generation this failed popup owns");
  assert.equal(popup.values.pendingConnection, undefined);
  assert.equal(popup.values.token, undefined);
  assert.ok(!popup.byId("message").textContent.includes("private"));
});

test("remote Pair stuck JSON body never commits a connection token", async () => {
  const popup = loadPopup();
  await new Promise(resolve=>setImmediate(resolve));
  popup.byId("gateway").value = "https://private.example";
  popup.byId("code").value = "12345678";
  let signal;
  popup.context.fetch = async (_url, options) => {
    signal = options.signal;
    return {ok:true,status:200,json:async()=>new Promise(()=>{})};
  };
  const task = popup.byId("pair").handlers.click();
  const [id,timer] = await currentConnectionDeadline(popup);
  timer.fn();
  await task;
  assert.equal(signal.aborted, true);
  assert.equal(popup.clearedConnectDeadlines.has(id), true);
  assert.ok(!popup.chromeCalls.some(x=>x.startsWith("connect-paired-relay:")));
  assert.equal(popup.values.token, undefined);
  assert.ok(!popup.byId("message").textContent.includes("private.example"));
});


test("persisted but activation-pending tentative link commits consent without claiming login", async () => {
  const popup = loadPopup({localRelay:true,syncStatus:"saved_activation_pending"});
  for (let i=0;i<8;i++) await new Promise(resolve=>setImmediate(resolve));
  await popup.controls.connectProvider("metaai");
  assert.ok(popup.values.linkedProviders.includes("metaai"));
  assert.equal(popup.server.linked,true);
  assert.ok(popup.chromeCalls.includes("commit-provider-link:metaai"));
  assert.ok(!popup.chromeCalls.includes("rollback-provider-link:metaai"));
  assert.match(popup.byId("message").textContent, /Session saved, but the provider browser could not activate/);
  assert.match(popup.byId("message").textContent, /authentication is not verified/);
});

test("unverified sync status still rolls back tentative consent", async () => {
  const popup = loadPopup({localRelay:true,syncStatus:"unverified_status"});
  for (let i=0;i<8;i++) await new Promise(resolve=>setImmediate(resolve));
  await assert.rejects(()=>popup.controls.connectProvider("metaai"),
    /Provider session was not uploaded/);
  assert.ok(!popup.values.linkedProviders.includes("metaai"));
  assert.ok(popup.chromeCalls.includes("rollback-provider-link:metaai"));
});


test("stale ChatGPT tab-query completion cannot overwrite a newer verified account check", async () => {
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  let releaseOld;
  let queries=0;
  popup.context.chrome.tabs.query = async () => {
    if (++queries===1) return new Promise(resolve=>{releaseOld=resolve;});
    return [{id:42,url:"https://chatgpt.com/"}];
  };
  let injections=0;
  popup.context.chrome.scripting = {async executeScript() {
    injections++;
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
      reason:"first_party_session_confirmed",checked_at:new Date().toISOString(),
      exported_snapshot_verified:false
    }}];
  }};
  const older=popup.controls.verifyChatgptActiveTab();
  const newer=popup.controls.verifyChatgptActiveTab();
  await newer;
  assert.equal(injections,1,"stale queued tab query must not inject on another tab");
  assert.match(popup.byId("message").textContent,/Gateway credential replay remains unverified/);
  assert.equal(popup.byId("verify-chatgpt").disabled,false);
  releaseOld([{id:24,url:"https://meta.ai/"}]);
  await older;
  assert.match(popup.byId("message").textContent,/Gateway credential replay remains unverified/);
  assert.equal(popup.byId("verify-chatgpt").disabled,false);
});

test("stale Chrome scripting success and thrown tab errors cannot replace newer verdict", async () => {
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  popup.context.chrome.tabs.query = async () => [{id:42,url:"https://chatgpt.com/"}];
  let startOld;
  const oldStarted=new Promise(resolve=>{startOld=resolve;});
  let releaseOld;
  let calls=0;
  popup.context.chrome.scripting = {async executeScript() {
    if (++calls===1) {
      startOld();
      return new Promise(resolve=>{releaseOld=resolve;});
    }
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
      reason:"first_party_session_confirmed",checked_at:new Date().toISOString(),
      exported_snapshot_verified:false
    }}];
  }};
  const older=popup.controls.verifyChatgptActiveTab();
  await oldStarted;
  await popup.controls.verifyChatgptActiveTab();
  releaseOld([{result:{
    provider:"chatgpt",context:"chrome_active_tab",authentication:"unauthenticated",
    reason:"provider_rejected_session",checked_at:new Date().toISOString(),
    exported_snapshot_verified:false
  }}]);
  await older;
  assert.equal(calls,2);
  assert.match(popup.byId("message").textContent,/account verified in this Chrome tab/);
  assert.doesNotMatch(popup.byId("message").textContent,/no active account/);

  let rejectOld;
  let queries=0;
  popup.context.chrome.tabs.query = async () => {
    if (++queries===1) return new Promise((_,reject)=>{rejectOld=reject;});
    return [{id:42,url:"https://chatgpt.com/"}];
  };
  const third=popup.controls.verifyChatgptActiveTab();
  await popup.controls.verifyChatgptActiveTab();
  rejectOld(new Error("private token=secret"));
  await third;
  assert.match(popup.byId("message").textContent,/account verified in this Chrome tab/);
  assert.doesNotMatch(popup.byId("message").textContent,/secret|unavailable/);
  assert.equal(popup.byId("verify-chatgpt").disabled,false);
});

test("user-initiated ChatGPT verification is tab-scoped, read-only and does not sync", async () => {
  const popup = loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  popup.context.chrome.tabs.query = async query => {
    assert.equal(query.active,true);
    assert.equal(query.currentWindow,true);
    return [{id:42,url:"https://chatgpt.com/"}];
  };
  popup.context.chrome.scripting = {async executeScript(options) {
    assert.equal(options.target.tabId,42);
    assert.equal(options.target.allFrames,false);
    assert.equal(options.world,"MAIN");
    assert.equal(typeof options.func,"function");
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
      reason:"first_party_session_confirmed",checked_at:new Date().toISOString(),
      exported_snapshot_verified:false
    }}];
  }};
  const priorCalls = [...popup.chromeCalls];
  await popup.controls.verifyChatgptActiveTab();
  assert.match(popup.byId("message").textContent, /Gateway credential replay remains unverified/);
  assert.equal(popup.byId("verify-chatgpt").disabled,false);
  assert.deepEqual(popup.chromeCalls,priorCalls,"verification must not sync or modify gateway link");
});


function pairChatgptFixture(popup) {
  popup.values.connectionMode="local";
  popup.values.gateway="http://127.0.0.1:8080";
  popup.values.linkedProviders=["chatgpt"];
  popup.values.providerLinkEpochs={chatgpt:"paired-link-epoch"};
  popup.values.relayConnectionEpoch="paired-relay-epoch";
  popup.context.chrome.tabs.query=async()=>[{id:42,url:"https://chatgpt.com/c/private-chat-id"}];
  popup.context.chrome.scripting={executeScript:async options=>{
    assert.equal(options.target.tabId,42);
    assert.equal(options.target.allFrames,false);
    assert.equal(options.world,"MAIN");
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",
      authentication:"verified",reason:"first_party_session_confirmed",
      http_status:200,checked_at:new Date().toISOString(),
      exported_snapshot_verified:false
    }}];
  }};
}
function warmChatgptFixture(overrides={}) {
  return {
    schema_version:1,provider:"chatgpt",context:"obscura_warm_page",
    authentication:"verified",reason:"first_party_session_confirmed",
    checked_at_utc:new Date().toISOString().replace(".000Z","Z")
      .replace(/\.\d{3}Z$/,"Z"),
    attempt_id:"123e4567-e89b-42d3-a456-426614174000",
    worker_generation:3,
    exported_snapshot_verified:false,applied_snapshot_verified:false,
    ...overrides
  };
}

test("explicit paired account comparison uses no automatic probe, upload or parity claim",async()=>{
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  pairChatgptFixture(popup);
  const requests=[];
  popup.context.relayFetch=async (path,options,expected)=>{
    requests.push({path,options,expected});
    return {ok:true,status:200,json:async()=>warmChatgptFixture()};
  };
  assert.equal(requests.length,0);
  const priorCalls=[...popup.chromeCalls];
  await popup.controls.compareChatgptBrowserAndGateway();
  assert.equal(requests.length,1);
  assert.equal(requests[0].path,"/relay/v1/account-verification/chatgpt");
  assert.equal(requests[0].options.method,"POST");
  assert.equal(requests[0].options.body,"{}");
  assert.equal(requests[0].expected.provider,"chatgpt");
  assert.equal(requests[0].expected.linkEpoch,"paired-link-epoch");
  assert.equal(requests[0].expected.gateway,popup.values.gateway);
  assert.match(popup.byId("message").textContent,/Chrome tab: verified/);
  assert.match(popup.byId("message").textContent,/Obscura warm page: verified/);
  assert.match(popup.byId("message").textContent,/applied-snapshot match NOT verified/);
  assert.doesNotMatch(popup.byId("message").textContent,/same account|snapshot restored|private-chat-id|123e4567/);
  assert.equal(popup.byId("verify-chatgpt-gateway").disabled,false);
  assert.equal(popup.byId("verify-chatgpt").disabled,false);
  assert.deepEqual(popup.chromeCalls,priorCalls,"no mutation/sync worker call");
  // The ordinary connected heartbeat uses status only and must never call
  // the account-verification endpoint without a direct user click.
  requests.length=0;
  await popup.intervals.find(x=>x.ms===30_000).fn();
  assert(!requests.some(req=>req.path.includes("account-verification")));
});

test("paired comparison discards Chrome proof that expires while gateway call is pending",async()=>{
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  pairChatgptFixture(popup);
  const started=Date.now();
  let now=started;
  // This clock belongs only to the VM. Never change process-wide Date.now.
  popup.context.Date=class extends Date {static now(){return now;}};
  popup.context.chrome.scripting={executeScript:async()=>[{result:{
    provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
    reason:"first_party_session_confirmed",http_status:200,
    checked_at:new Date(started-25_000).toISOString(),
    exported_snapshot_verified:false
  }}]};
  let fetched=0;
  popup.context.relayFetch=async()=>{
    ++fetched;
    // Chrome was fresh on entry, but becomes stale before the final result.
    now+=10_000;
    return {ok:true,json:async()=>warmChatgptFixture({
      checked_at_utc:new Date(now).toISOString().slice(0,19)+"Z"
    })};
  };
  const calls=[...popup.chromeCalls];
  await popup.controls.compareChatgptBrowserAndGateway();
  assert.equal(fetched,1);
  const shown=popup.byId("message").textContent;
  assert.match(shown,/observations expired or differ in time/);
  assert.doesNotMatch(shown,/Chrome tab: verified|Obscura warm page: verified/);
  assert.deepEqual(popup.chromeCalls,calls,"must not resync or mutate credentials");
});

test("paired comparison rejects divergent source clocks even when each record is fresh",async()=>{
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  pairChatgptFixture(popup);
  const now=Date.now();
  popup.context.Date=class extends Date {static now(){return now;}};
  popup.context.chrome.scripting={executeScript:async()=>[{result:{
    provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
    reason:"first_party_session_confirmed",http_status:200,
    checked_at:new Date(now+25_000).toISOString(),
    exported_snapshot_verified:false
  }}]};
  popup.context.relayFetch=async()=>({ok:true,json:async()=>warmChatgptFixture({
    checked_at_utc:new Date(now-25_000).toISOString().slice(0,19)+"Z"
  })});
  const calls=[...popup.chromeCalls];
  await popup.controls.compareChatgptBrowserAndGateway();
  const shown=popup.byId("message").textContent;
  assert.match(shown,/observations expired or differ in time/);
  assert.doesNotMatch(shown,/Chrome tab: verified|Obscura warm page: verified/);
  assert.deepEqual(popup.chromeCalls,calls,"no automatic retry or resync");
});

test("paired comparison refuses unlinked or pending account consent before probing either side",async()=>{
  for(const state of [
    {},
    {connectionMode:"local"},
    {connectionMode:"local",linkedProviders:["chatgpt"]},
    {connectionMode:"local",linkedProviders:["chatgpt"],
      providerLinkEpochs:{chatgpt:"epoch"},providerLinkPendingAttempts:{chatgpt:"pending"}}
  ]){
    const popup=loadPopup();
    for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
    pairChatgptFixture(popup);
    for(const key of ["connectionMode","linkedProviders","providerLinkEpochs","providerLinkPendingAttempts"]){
      delete popup.values[key];
    }
    Object.assign(popup.values,state);
    let fetched=0,injected=0;
    popup.context.relayFetch=async()=>{fetched++;throw Error("private fetch");};
    popup.context.chrome.scripting={executeScript:async()=>{injected++;return [];}};
    await popup.controls.compareChatgptBrowserAndGateway();
    assert.equal(fetched,0);
    assert.equal(injected,0);
    assert.match(popup.byId("message").textContent,/Connect and link ChatGPT/);
  }
});

test("paired comparison rejects changed link epoch and same-origin tab navigation after async work",async()=>{
  for(const mode of ["link_epoch","tab_route"]){
    const popup=loadPopup();
    for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
    pairChatgptFixture(popup);
    let count=0;
    const base=popup.context.chrome.tabs.query;
    popup.context.chrome.tabs.query=async()=>{
      const tabs=await base();
      count++;
      return mode==="tab_route" && count>=3
        ? [{id:42,url:"https://chatgpt.com/c/private-other-account"}] : tabs;
    };
    popup.context.relayFetch=async()=>{
      if(mode==="link_epoch")popup.values.providerLinkEpochs.chatgpt="new-login-epoch";
      return {ok:true,json:async()=>warmChatgptFixture()};
    };
    await popup.controls.compareChatgptBrowserAndGateway();
    const shown=popup.byId("message").textContent;
    assert.match(shown,/Result discarded/);
    assert.doesNotMatch(shown,/Chrome tab: verified|Obscura warm page: verified|private-other-account/);
  }
});

test("paired comparison quarantines untrusted gateway proof, leaks no URL or token",async()=>{
  const invalids=[
    {exported_snapshot_verified:true},
    {applied_snapshot_verified:true},
    {account_id:"private account"},
    {worker_generation:-1},
    {checked_at_utc:"1999-01-01T00:00:00Z"},
    {authentication:"verified",reason:"first_party_check_inconclusive"},
    {reason:"token=private upstream body"}
  ];
  for(const invalid of invalids){
    const popup=loadPopup();
    for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
    pairChatgptFixture(popup);
    popup.context.relayFetch=async()=>({ok:true,json:async()=>warmChatgptFixture(invalid)});
    await popup.controls.compareChatgptBrowserAndGateway();
    const shown=popup.byId("message").textContent;
    assert.match(shown,/Gateway account verification returned unsupported evidence/);
    assert.doesNotMatch(shown,/private|token=|account_id|snapshot restored|Chrome tab: verified/);
  }
  const popup=loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  pairChatgptFixture(popup);
  popup.context.relayFetch=async()=>{throw Error("token=private https://private.invalid/");};
  await popup.controls.compareChatgptBrowserAndGateway();
  assert.equal(popup.byId("message").textContent,
    "Paired account checks unavailable. No sessions changed or resynced.");
});

test("paired comparison displays independent negative and unknown account verdicts",async()=>{
  for(const [gateway,reason,label] of [
    ["unauthenticated","provider_rejected_session","not signed in"],
    ["unknown","first_party_check_inconclusive","unknown"],
    ["unsupported","account_verifier_not_established","unsupported"]
  ]){
    const popup=loadPopup();
    for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
    pairChatgptFixture(popup);
    popup.context.relayFetch=async()=>({ok:true,json:async()=>warmChatgptFixture({
      authentication:gateway,reason
    })});
    await popup.controls.compareChatgptBrowserAndGateway();
    assert.match(popup.byId("message").textContent,
      new RegExp("Obscura warm page: " + label));
    assert.match(popup.byId("message").textContent,/match NOT verified/);
    assert.doesNotMatch(popup.byId("message").textContent,/same account/);
  }
});

test("same-origin tab route drift after injected probe invalidates account verdict without URL disclosure", async () => {
  const initial = "https://chatgpt.com/c/private-initial";
  const destinations = [
    "https://chatgpt.com/c/private-next",
    "https://chatgpt.com/c/private-initial?account=private-other",
    "https://chatgpt.com/c/private-initial#private-other"
  ];
  for (const authentication of ["verified", "unauthenticated"]) {
    for (const currentUrl of destinations) {
      const popup=loadPopup();
      for (let i=0; i<5; i++) await new Promise(resolve=>setImmediate(resolve));
      let requests=0, scriptInvocations=0;
      popup.context.chrome.tabs.query = async () => [{
        id:42,url:++requests===1 ? initial : currentUrl
      }];
      popup.context.chrome.scripting={async executeScript(options) {
        assert.equal(options.target.tabId,42);
        scriptInvocations++;
        return [{result:{
          provider:"chatgpt",context:"chrome_active_tab",authentication,
          reason:authentication==="verified" ? "first_party_session_confirmed" : "provider_rejected_session",
          checked_at:new Date().toISOString(), exported_snapshot_verified:false
        }}];
      }};
      const priorCalls=[...popup.chromeCalls];
      await popup.controls.verifyChatgptActiveTab();
      assert.equal(scriptInvocations,1);
      assert.match(popup.byId("message").textContent,/Tab changed during verification/);
      assert.doesNotMatch(popup.byId("message").textContent,/verified in this Chrome tab|no active account/);
      assert.doesNotMatch(popup.byId("message").textContent,/private-initial|private-next|private-other/);
      assert.deepEqual(popup.chromeCalls,priorCalls,"stale read-only check must not sync credentials");
    }
  }

  const unchanged=loadPopup();
  for (let i=0; i<5; i++) await new Promise(resolve=>setImmediate(resolve));
  unchanged.context.chrome.tabs.query=async()=>[{id:42,url:initial}];
  unchanged.context.chrome.scripting={executeScript:async()=>[{result:{
    provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
    reason:"first_party_session_confirmed",checked_at:new Date().toISOString(),
    exported_snapshot_verified:false
  }}]};
  await unchanged.controls.verifyChatgptActiveTab();
  assert.match(unchanged.byId("message").textContent,/account verified in this Chrome tab/);
  assert.doesNotMatch(unchanged.byId("message").textContent,/private-initial/);
});

test("active-tab verification refuses foreign origins and stale tab changes", async () => {
  const popup = loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  let queried=0,injected=0;
  popup.context.chrome.tabs.query = async () => {
    queried++;
    return [{id:42,url:queried===1?"https://chatgpt.com/":"https://meta.ai/"}];
  };
  popup.context.chrome.scripting = {async executeScript() {
    injected++;
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
      reason:"first_party_session_confirmed",checked_at:new Date().toISOString(),
      exported_snapshot_verified:false
    }}];
  }};
  await popup.controls.verifyChatgptActiveTab();
  assert.equal(injected,1);
  assert.match(popup.byId("message").textContent,/Tab changed/);
  popup.context.chrome.tabs.query = async () => [{id:25,url:"https://chatgpt.com.attacker.test/"}];
  await popup.controls.verifyChatgptActiveTab();
  assert.equal(injected,1,"foreign tab must never receive injected account code");
  assert.match(popup.byId("message").textContent,/Open an existing signed-in/);
});

test("forged Chrome result and unavailable scripting are never called verified", async () => {
  const popup = loadPopup();
  for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));
  popup.context.chrome.tabs.query = async () => [{id:42,url:"https://chatgpt.com/"}];
  popup.context.chrome.scripting = {async executeScript() {
    return [{result:{
      provider:"chatgpt",context:"chrome_active_tab",authentication:"verified",
      reason:"private token=secret",checked_at:new Date().toISOString(),
      exported_snapshot_verified:true
    }}];
  }};
  await popup.controls.verifyChatgptActiveTab();
  assert.match(popup.byId("message").textContent,/unsupported evidence/);
  assert.doesNotMatch(popup.byId("message").textContent,/secret/);
  delete popup.context.chrome.scripting;
  await popup.controls.verifyChatgptActiveTab();
  assert.match(popup.byId("message").textContent,/unavailable/);
});

test("last-failure UI accepts only fresh fixed server evidence for two active providers", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  const current = new Date(Math.floor(Date.now() / 1000) * 1000)
    .toISOString().replace(".000Z", "Z");
  const valid = {
    source_failure_schema_version: 1,
    request_id: "5f855e0d-5b22-4a93-b925-98db841a1ee1",
    stage: "first_stream_event", reason: "backend_unavailable",
    duration_ms: 120_000, observed_at_utc: current
  };
  const safe = popup.controls.safeLastFailedRequest;
  assert.match(safe("chatgpt", valid), /first stream event \/ backend unavailable/);
  assert.match(safe("metaai", valid), /5f855e0d-5b22-4a93-b925-98db841a1ee1/);
  assert.equal(safe("grok", valid), null, "deferred provider has no diagnostic display");
  assert.equal(safe("chatgpt", null), null, "no evidence is not proof of success");
  for (const [stage, reason] of [
    ["candidate_admission", "provider_busy"],
    ["candidate_admission", "candidate_skipped"],
    ["backend_admission", "timeout"],
    ["first_stream_event", "unclassified"],
    ["json_completion", "unclassified"]
  ]) {
    assert.match(safe("chatgpt", {...valid, stage, reason}), /Last gateway failure:/);
  }
  const invalid = [
    {...valid,source_failure_schema_version:0},
    {...valid,source_failure_schema_version:2},
    {...valid,source_failure_schema_version:null},
    {...valid,source_failure_schema_version:"1"},
    {...valid,source_failure_schema_version:undefined},
    {...valid,stage:"json_completion",reason:"backend_unavailable"},
    {...valid,stage:"candidate_admission",reason:"backend_unavailable"},
    {...valid,stage:"backend_admission",reason:"candidate_skipped"},
    {...valid,stage:"first_stream_event",reason:"candidate_skipped"},
    {...valid,stage:"backend_admission",reason:"unclassified"},
    {...valid,account_id:"private account identifier"},
    {...valid,request_id:"token=private"},
    {...valid,stage:"json_completion token=private"},
    {...valid,reason:"https://private.example/token?cookie=secret"},
    {...valid,duration_ms: Infinity}, {...valid,duration_ms: -1},
    {...valid,duration_ms: 120_001}, {...valid,duration_ms: 1.5},
    {...valid,observed_at_utc:current+" token=private"},
    {...valid,observed_at_utc:"2000-01-01T00:00:00Z"},
    {...valid,observed_at_utc:"2099-01-01T00:00:00Z"},
    {...valid,observed_at_utc:"2026-02-30T00:00:00Z"},
    {request_id: valid.request_id,stage:valid.stage,reason:valid.reason,observed_at_utc:current}
  ];
  for (const record of invalid) {
    assert.equal(safe("chatgpt", record), null, "untrusted field must discard entire record");
  }
  // The general popup fixture initially registers Meta AI/Claude/Manus only.
  // Register the two additional provider definitions before asserting their rows.
  popup.context.PROVIDERS.chatgpt = ["chatgpt.com"];
  popup.context.PROVIDERS.grok = ["grok.com"];
  popup.context.relayFetch = async () => ({
    ok:true,status:200,json:async()=>({vault_mode:true,providers:[
      {provider:"chatgpt",enabled:true,linked:true,authentication:"unknown",last_failed_request:valid},
      {provider:"metaai",enabled:true,linked:true,authentication:"unknown",
        last_failed_request:{...valid,reason:"private cookie=secret"}},
      {provider:"grok",enabled:false,linked:false,last_failed_request:valid}
    ]})
  });
  await popup.controls.refreshStatus(true);
  const rows = popup.byId("status").children;
  const texts = Object.fromEntries(rows.map(row => [
    row.children[0].textContent,
    row.children.find(child=>child.className==="provider-detail").textContent
  ]));
  assert.match(texts.ChatGPT,/Last gateway failure: first stream event/);
  assert.match(texts.ChatGPT,/Login unverified/);
  assert.doesNotMatch(texts["Meta AI"],/Last gateway failure|private|cookie=secret/);
  assert.doesNotMatch(texts.grok,/Last gateway failure/);
});

test("provider status names and disabled account checks are distinct from verified login", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  popup.context.PROVIDERS.chatgpt = ["chatgpt.com"];
  popup.context.relayFetch = async () => ({
    ok:true,status:200,json:async()=>({vault_mode:true,providers:[
      {provider:"chatgpt",enabled:true,linked:true,authentication:"verified"},
      {provider:"metaai",enabled:true,linked:true,authentication:"unknown"},
      {provider:"claude",enabled:false,linked:true,authentication:"verified"},
      {provider:"manus",enabled:null,linked:true,authentication:"unknown"}
    ]})
  });
  await popup.controls.refreshStatus(true);
  const rows = popup.byId("status").children;
  const cards = Object.fromEntries(rows.map(row => [
    row.children[0].textContent,
    row.children.find(child => child.className === "provider-detail").textContent
  ]));
  assert.match(cards.ChatGPT,/Login verified/);
  assert.match(cards["Meta AI"],/Login unverified/);
  assert.match(cards.claude,/Disabled in gateway config/);
  assert.match(cards.claude,/Account check deferred \(provider disabled\)/);
  assert.doesNotMatch(cards.claude,/Login verified|Login needed|Login unverified/);
  assert.match(cards.manus,/Gateway enablement unverified/);
  assert.match(cards.manus,/Login unverified/);
  // Display-name mapping is UI only; action routing still uses provider IDs.
  const chatgptRow = rows.find(row => row.children[0].textContent === "ChatGPT");
  assert.ok(chatgptRow);
  assert.match(chatgptRow.children[1].textContent,/Session saved/);
});


test("disabled provider policy does not masquerade as a gateway outage", async () => {
  const popup = loadPopup();
  await new Promise(resolve => setImmediate(resolve));
  popup.context.relayFetch = async () => ({
    ok:true,status:200,json:async()=>({vault_mode:true,providers:[
      {provider:"metaai",enabled:true,linked:true,authentication:"unknown",
        availability:"degraded",issue:"provider_degraded",manual_verification_required:true},
      {provider:"claude",enabled:false,linked:true,authentication:"verified",
        availability:"degraded",issue:"provider_disabled",manual_verification_required:true},
      {provider:"manus",enabled:null,linked:false,authentication:"unknown",
        availability:"degraded",issue:"provider_degraded",manual_verification_required:true}
    ]})
  });
  await popup.controls.refreshStatus(true);
  const rows = Object.fromEntries(popup.byId("status").children.map(row => [
    row.children[0].textContent,row
  ]));
  const detail = row => row.children.find(child =>
    child.className === "provider-detail").textContent;
  // The existing precedence shows manual verification instead of the
  // secondary degraded retry hint when both apply.
  assert.match(detail(rows["Meta AI"]),/Gateway issue: provider degraded/);
  assert.match(detail(rows["Meta AI"]),/Manual provider verification required/);
  assert.match(detail(rows.claude),/Disabled in gateway config/);
  assert.match(detail(rows.claude),/Account check deferred \(provider disabled\)/);
  assert.match(detail(rows.claude),/Session saved/);
  assert.doesNotMatch(detail(rows.claude),
    /Gateway issue|Gateway check unavailable|Manual provider verification required|Login verified/);
  const disabledActions = rows.claude.children.find(child =>
    child.className === "provider-actions").children;
  assert.equal(disabledActions[0].disabled,true,"Sync remains disabled by policy");
  assert.equal(disabledActions[1].textContent,"Open website");
  assert.equal(disabledActions[2].textContent,"Log out");
  // Unknown enablement is not permission to suppress real warning evidence.
  assert.match(detail(rows.manus),/Gateway enablement unverified/);
  assert.match(detail(rows.manus),/Gateway issue: provider degraded/);
});

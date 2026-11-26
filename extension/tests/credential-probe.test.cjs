const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

const source = fs.readFileSync("extension/credential_probe.js","utf8");

async function probe({hostname="chatgpt.com",protocol="https:",port="",status=200,
                      session={},reject=false,deadline=false,
                      changeAfter=null, navigation="same_origin"} = {}) {
  let calls=0;
  const initialHref = protocol + "//" + hostname + (port ? ":" + port : "") +
    "/c/initial-account-route";
  const location = {hostname,protocol,port,href:initialHref};
  const navigate = () => {
    if (navigation === "cross_origin") {
      location.hostname = "private-foreign.invalid";
      location.href = "https://private-foreign.invalid/account/secret";
    } else if (navigation === "new_port") {
      location.port = "8443";
      location.href = "https://chatgpt.com:8443/account/secret";
    } else {
      // A same-tab SPA route can change account context without changing origin.
      location.href = "https://chatgpt.com/c/different-account-secret";
    }
  };
  const context = {
    location, Date, AbortController,
    setTimeout:deadline ? callback=>{queueMicrotask(callback);return 1;} : setTimeout,
    clearTimeout: deadline ? ()=>{} : clearTimeout,
    fetch:async (route,options)=>{
      calls++;
      assert.equal(route,"/api/auth/session");
      assert.equal(options.credentials,"include");
      assert.equal(options.cache,"no-store");
      assert.equal(options.redirect,"error");
      if(deadline){await Promise.resolve();throw new Error("private timeout");}
      if(reject)throw new Error("secret bearer token");
      if (changeAfter === "fetch") navigate();
      return {status,json:async()=>{
        if (changeAfter === "json") navigate();
        return session;
      }};
    }
  };
  const value=await vm.runInNewContext(source+"\nglobalThis.probeActiveChatgptAccount()",context);
  return {value,calls};
}

test("supported first-party account response proves Chrome only, no secret returned",async()=>{
  const {value,calls}=await probe({session:{
    user:{email:"private@example.test"},accessToken:"hidden-bearer-token",
    expires:new Date(Date.now()+60_000).toISOString()
  }});
  assert.equal(calls,1);
  assert.equal(value.authentication,"verified");
  assert.equal(value.reason,"first_party_session_confirmed");
  assert.equal(value.exported_snapshot_verified,false);
  const json=JSON.stringify(value);
  assert(!json.includes("private"));
  assert(!json.includes("hidden-bearer-token"));
  assert.equal(value.context,"chrome_active_tab");
});

test("exact first-party host and HTTPS only; no foreign fetch",async()=>{
  for(const x of [
    {hostname:"chatgpt.com.attacker.test"},
    {hostname:"meta.ai"}, {hostname:"openai.com"}, {protocol:"http:"},
    {port:"8080"}
  ]){
    const {value,calls}=await probe(x);
    assert.equal(calls,0);
    assert.equal(value.reason,"unsupported_origin");
  }
});

test("only supported explicit rejection is unauthenticated; failures are unknown",async()=>{
  for(const params of [{status:401},{session:{}},{session:{user:null}}]){
    const {value}=await probe(params);
    assert.equal(value.authentication,"unauthenticated");
  }
  for(const params of [
    {status:403},{status:429},{status:500},{session:[]},
    {session:{user:{},accessToken:"secret",expires:"2000-01-01"}},
    {session:{error:"private"}},
    {reject:true},{deadline:true}
  ]){
    const {value}=await probe(params);
    assert.equal(value.authentication,"unknown");
    assert(!JSON.stringify(value).includes("private"));
  }
});

test("ambiguous nonempty 200 account schemas cannot force re-login or leak provider errors",async()=>{
  const cases=[
    {user:null,error:"private:temporary-provider-error"},
    {user:null,accessToken:null,expires:"unknown"},
    {user:null,accountStatus:"unknown"},
    {user:null,challenge:"private-cookie-reason"},
    {user:undefined,accessToken:"",expires:new Date(Date.now()+60_000).toISOString()}
  ];
  for(const session of cases){
    const {value,calls}=await probe({session});
    assert.equal(calls,1);
    assert.equal(value.authentication,"unknown");
    assert.equal(value.reason,"incomplete_or_expired_session_evidence");
    assert.equal(value.http_status,200);
    const serialized=JSON.stringify(value);
    for(const secret of ["private", "accountStatus", "challenge", "accessToken"]) {
      assert(!serialized.includes(secret));
    }
  }
  assert.equal((await probe({session:{user:null}})).value.authentication,"unauthenticated");
  assert.equal((await probe({session:{}})).value.authentication,"unauthenticated");
  assert.equal((await probe({status:401})).value.authentication,"unauthenticated");
});

test("first-party proof is discarded when same tab navigates across either awaited boundary",async()=>{
  const session = {
    user:{email:"private@example.test"},accessToken:"bearer-private-secret",
    expires:new Date(Date.now()+60_000).toISOString()
  };
  for (const changeAfter of ["fetch","json"]) {
    for (const navigation of ["same_origin","cross_origin","new_port"]) {
      const {value,calls} = await probe({session,changeAfter,navigation});
      assert.equal(calls,1, changeAfter + "/" + navigation);
      assert.equal(value.authentication,"unknown",changeAfter + "/" + navigation);
      assert.equal(value.reason,"tab_changed",changeAfter + "/" + navigation);
      assert.equal(value.exported_snapshot_verified,false);
      const serialized = JSON.stringify(value);
      for (const secret of ["bearer-private-secret","different-account-secret",
                             "private-foreign.invalid","8443","private@example.test"]) {
        assert(!serialized.includes(secret), "private location or account leaked");
      }
    }
  }
});

test("negative first-party claims also fail closed if page changes before status is handled",async()=>{
  const after = await probe({status:401,changeAfter:"fetch"});
  assert.equal(after.value.authentication,"unknown");
  assert.equal(after.value.reason,"tab_changed");
  const before = await probe({status:401});
  assert.equal(before.value.authentication,"unauthenticated");
});

async function nativeProbe({session,account={id:"private-id"},status=200,changeAfter=null}={}) {
  const calls=[];const location={protocol:"https:",hostname:"chatgpt.com",port:"",href:"https://chatgpt.com/"};
  const context={location,Date,AbortController,setTimeout,clearTimeout,atob,
    fetch:async(route,options)=>{calls.push({route,options});return {status:route.includes("/me")?status:200,json:async()=>{if(route.includes("/me")&&changeAfter)location.href="https://chatgpt.com/c/private";return route.includes("/me")?account:session;}};}};
  const value=await vm.runInNewContext(source+'\nprobeActiveChatgptAccount("chrome_native_diagnostic")',context);
  return {value,calls};
}
const nativeSession=()=>({user:{id:"private-id"},accessToken:"private-token",expires:new Date(Date.now()+60000).toISOString()});
test("native account error path requires a matching real account response",async()=>{
  const {value,calls}=await nativeProbe({session:{...nativeSession(),error:"private-refresh-error"}});
  assert.equal(value.reason,"bearer_account_confirmed");assert.equal(value.auth_mode,"bearer");assert.equal(calls.length,2);
  assert.equal(calls[1].route,"/backend-api/me");assert.equal(calls[1].options.headers.Authorization,"Bearer private-token");assert.equal(calls[0].options.signal,calls[1].options.signal);
  assert.doesNotMatch(JSON.stringify(value),/private|accessToken|user/);
});
test("native account check does not reinterpret session errors without independent proof",async()=>{
  for(const [params,reason] of [[{account:{id:"other-private"}},"account_principal_mismatch"],[{account:{}},"unrecognized_account_response"],[{status:401},"access_token_rejected"],[{status:403},"provider_or_network_blocked"],[{changeAfter:"json"},"tab_changed"]]){
    const {value}=await nativeProbe({session:{...nativeSession(),error:"private"},...params});assert.equal(value.authentication,"unknown");assert.equal(value.reason,reason);assert.doesNotMatch(JSON.stringify(value),/private/);
  }
  for(const extra of [{expires:"2000-01-01"},{accessToken:""},{user:{}},{accessToken:"a."+btoa(JSON.stringify({exp:1}))+".c"}]){
    const {value,calls}=await nativeProbe({session:{...nativeSession(),error:"private",...extra}});assert.equal(value.reason,"session_reported_error");assert.equal(calls.length,1);
  }
  const good=await nativeProbe({session:nativeSession()});assert.equal(good.value.auth_mode,"session");assert.equal(good.calls.length,1);
});

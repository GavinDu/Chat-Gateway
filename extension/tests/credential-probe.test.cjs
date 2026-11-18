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

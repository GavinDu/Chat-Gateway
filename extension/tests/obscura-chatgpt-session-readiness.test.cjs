// Offline owner-free fixtures for the exact JavaScript injected into an
// imported Obscura ChatGPT page. Never contact a real provider or log secrets.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

const script = fs.readFileSync("src/providers/chatgpt/session_readiness.js", "utf8")
  .replaceAll("__ATTEMPT__", JSON.stringify("synthetic-attempt"))
  .replaceAll("__ORIGIN__", JSON.stringify("https://chatgpt.com"))
  .replaceAll("__TIMEOUT_MS__", "2500");

const validSession = () => ({
  user: {email: "private@example.test"},
  accessToken: "synthetic-bearer-private-token",
  expires: new Date(Date.now() + 60_000).toISOString()
});

async function run({origin="https://chatgpt.com", status=200,
                    session=validSession(), changeAt=null,
                    navigation="same_origin", rejected=false,
                    redirected=false, responseUrl=null, jsonRejected=false} = {}) {
  const location = {
    origin, href: origin + "/c/original-private-thread"
  };
  const calls = [];
  const changePage = () => {
    if (navigation === "off_origin") {
      location.origin = "https://different.private.invalid";
      location.href = location.origin + "/c/private-account";
    } else {
      location.href = origin + "/c/different-private-account";
    }
  };
  const sandbox = {
    location, AbortController, setTimeout, clearTimeout, Date, atob,
    fetch: async (route, options) => {
      calls.push({route,options});
      if (rejected) throw new Error("private upstream redirect or bearer");
      if (changeAt === "fetch") changePage();
      return {
        status, ok: status >= 200 && status < 300,
        redirected, url: responseUrl === null ? origin + "/api/auth/session" : responseUrl,
        json: async () => {
          if (changeAt === "json") changePage();
          if (jsonRejected) throw new Error("private malformed response body");
          return session;
        }
      };
    }
  };
  vm.createContext(sandbox);
  const initial = vm.runInContext(script, sandbox);
  await new Promise(resolve => setImmediate(resolve));
  const report = sandbox.__chatGatewayChatgptSessionReadiness
    ? vm.runInContext("({state:globalThis.__chatGatewayChatgptSessionReadiness.state, http:globalThis.__chatGatewayChatgptSessionReadiness.http, reason:globalThis.__chatGatewayChatgptSessionReadiness.reason})", sandbox)
    : initial;
  return {report, calls, sandbox};
}

test("verified first-party snapshot is source-scoped, no account data crosses result", async () => {
  const {report,calls} = await run();
  assert.equal(report.state, "verified");
  assert.equal(report.http, 200);
  assert.equal(calls.length,1);
  assert.equal(calls[0].route,"/api/auth/session");
  assert.equal(calls[0].options.credentials,"include");
  assert.equal(calls[0].options.cache,"no-store");
  // Pinned Obscura only exposes real redirect metadata with "follow";
  // the verifier must fail closed on Response.redirected and final URL.
  assert.equal(calls[0].options.redirect,"follow");
  assert(calls[0].options.signal);
  assert.deepEqual(Object.keys(report).sort(),["http","reason","state"]);
  const json=JSON.stringify(report);
  for (const secret of ["private@example.test", "bearer-private", "original-private-thread"]) {
    assert(!json.includes(secret));
  }
});

test("explicit logout, challenge, malformed and redirected responses stay distinct", async () => {
  assert.equal((await run({status:401})).report.state,"needs_login");
  assert.equal((await run({status:403})).report.state,"unavailable");
  assert.equal((await run({status:503})).report.state,"unavailable");
  assert.equal((await run({session:{}})).report.state,"needs_login");
  assert.equal((await run({session:[]})).report.state,"unavailable");
  assert.equal((await run({rejected:true})).report.state,"unavailable");
  assert.equal((await run({redirected:true})).report.state,"unavailable");
  assert.equal((await run({responseUrl:"https://foreign.private.invalid/auth/session"})).report.state,"unavailable");
  assert.equal((await run({responseUrl:"https://chatgpt.com/c/not-session"})).report.state,"unavailable");
  assert.equal((await run({responseUrl:""})).report.state,"unavailable");
  assert.equal((await run({status:401,redirected:true})).report.state,"unavailable");
  const foreign=await run({origin:"https://different.private.invalid"});
  assert.equal(foreign.report.state,"off_origin");
  assert.equal(foreign.calls.length,0);
});

test("Obscura never treats whitespace or missing accessToken as a first-party verified account", async () => {
  const expires = new Date(Date.now() + 60_000).toISOString();
  for (const accessToken of ["", " ", "  \t\n  ", null, undefined]) {
    const {report,calls} = await run({
      session: {user:{fixture:true},accessToken,expires}
    });
    assert.equal(calls.length, 1);
    assert.equal(report.state, "unavailable", JSON.stringify(accessToken));
    assert.equal(report.http, 200);
    const output=JSON.stringify(report);
    assert(!output.includes("accessToken"));
    assert(!output.includes("fixture"));
  }
  // The corresponding Chrome active-tab verifier already requires trim().
  // Neither context may declare the other one's saved-session snapshot valid.
  assert.equal((await run()).report.state,"verified");
});

test("changed first-party SPA route during fetch or JSON parse cannot certify previous account", async () => {
  for (const changeAt of ["fetch","json"]) {
    for (const navigation of ["same_origin","off_origin"]) {
      const {report,calls}=await run({changeAt,navigation});
      assert.equal(report.state,"unavailable",changeAt+"/"+navigation);
      assert.equal(report.http,200);
      assert.equal(calls.length,1);
      const serialized=JSON.stringify(report);
      assert(!serialized.includes("private-account"));
      assert(!serialized.includes("private-thread"));
      assert(!serialized.includes("synthetic-bearer"));
    }
  }
});

test("a moved page cannot turn an earlier 401 into a false current logout", async () => {
  const {report}=await run({status:401,changeAt:"fetch"});
  assert.equal(report.state,"unavailable");
  assert.equal(report.http,401);
});

test("fixed reasons distinguish HTTP 200 failures without returning session data", async () => {
  const jwt = payload => "e30." + Buffer.from(JSON.stringify(payload)).toString("base64url") + ".synthetic-signature";
  const cases = [
    [{}, "verified", "verified"],
    [{status:401}, "needs_login", "explicit_unauthenticated"],
    [{session:{}}, "needs_login", "explicit_unauthenticated"],
    [{status:403}, "unavailable", "http_unavailable"],
    [{redirected:true}, "unavailable", "response_redirected"],
    [{responseUrl:""}, "unavailable", "response_url_missing"],
    [{responseUrl:"https://private.invalid/secret"}, "unavailable", "response_endpoint_mismatch"],
    [{jsonRejected:true}, "unavailable", "invalid_json_or_body"],
    [{session:[]}, "unavailable", "invalid_session_schema"],
    [{session:{...validSession(),error:"private-refresh-error"}}, "unavailable", "session_reported_error"],
    [{session:{...validSession(),user:{}}}, "unavailable", "user_missing"],
    [{session:{...validSession(),accessToken:" "}}, "unavailable", "access_token_missing"],
    [{session:{...validSession(),accessToken:"a.invalid.signature"}}, "unavailable", "access_token_invalid"],
    [{session:{...validSession(),accessToken:jwt({exp:1})}}, "unavailable", "access_token_expired"],
    [{session:{...validSession(),accessToken:jwt({exp:"private"})}}, "unavailable", "access_token_invalid"],
    [{session:{...validSession(),accessToken:jwt({exp:Date.now()/1000+60})}}, "verified", "verified"],
    [{session:{...validSession(),accessToken:jwt({})}}, "verified", "verified"],
    [{session:{...validSession(),expires:"private-invalid-expiry"}}, "unavailable", "session_expiry_invalid"],
    [{session:{...validSession(),expires:"2000-01-01T00:00:00Z"}}, "unavailable", "session_expiry_expired"],
    [{rejected:true}, "unavailable", "request_failed"],
    [{changeAt:"json"}, "unavailable", "page_changed"],
    [{origin:"https://private.invalid"}, "off_origin", "off_origin"],
  ];
  for (const [options,state,reason] of cases) {
    const {report} = await run(options);
    assert.equal(report.state,state,reason);
    assert.equal(report.reason,reason);
    const serialized=JSON.stringify(report);
    for (const secret of ["private", "synthetic-signature", "accessToken", "user", "expires"]) {
      // Reason names are constant; the user_missing category is deliberately allowed.
      if (secret === "user") continue;
      assert(!serialized.includes(secret), reason + ": " + secret);
    }
  }
});

test("deadline reports only a fixed reason and aborts its pending request", async () => {
  let signal;
  const sandbox = {
    location: {origin:"https://chatgpt.com",href:"https://chatgpt.com/c/private-thread"},
    AbortController, setTimeout, clearTimeout, Date,
    fetch: (_, options) => { signal=options.signal; return new Promise(() => {}); }
  };
  vm.createContext(sandbox);
  vm.runInContext(script.replace("2500", "10"),sandbox);
  await new Promise(resolve => setTimeout(resolve,30));
  const p=sandbox.__chatGatewayChatgptSessionReadiness;
  assert.equal(p.state,"timeout");
  assert.equal(p.reason,"deadline");
  assert.equal(signal.aborted,true);
});

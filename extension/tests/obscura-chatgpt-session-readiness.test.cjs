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
                    redirected=false, responseUrl=null} = {}) {
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
    location, AbortController, setTimeout, clearTimeout, Date,
    fetch: async (route, options) => {
      calls.push({route,options});
      if (rejected) throw new Error("private upstream redirect or bearer");
      if (changeAt === "fetch") changePage();
      return {
        status, ok: status >= 200 && status < 300,
        redirected, url: responseUrl === null ? origin + "/api/auth/session" : responseUrl,
        json: async () => {
          if (changeAt === "json") changePage();
          return session;
        }
      };
    }
  };
  vm.createContext(sandbox);
  const initial = vm.runInContext(script, sandbox);
  await new Promise(resolve => setImmediate(resolve));
  const report = sandbox.__chatGatewayChatgptSessionReadiness
    ? vm.runInContext("({state:globalThis.__chatGatewayChatgptSessionReadiness.state, http:globalThis.__chatGatewayChatgptSessionReadiness.http})", sandbox)
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
  assert.deepEqual(Object.keys(report).sort(),["http","state"]);
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

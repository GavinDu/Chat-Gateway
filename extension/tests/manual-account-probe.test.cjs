const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const script = fs.readFileSync(
  path.join(__dirname, "..", "..", "scripts", "provider_credential_probe.js"),
  "utf8"
);
const firstParty = "https://chatgpt.com/c/original?private=secret";
function fixture(fetchAction, locationOverride = {}) {
  const location = {
    protocol: "https:", hostname: "chatgpt.com", port: "",
    href: firstParty, ...locationOverride
  };
  let calls = 0;
  const ctx = {
    location, Date, Promise, AbortController, setTimeout, clearTimeout,
    fetch: async (url, options) => {
      ++calls;
      assert.equal(url, "/api/auth/session");
      assert.equal(options.credentials, "include");
      assert.equal(options.redirect, "error");
      return fetchAction(location);
    }
  };
  return { run: () => vm.runInNewContext(script, ctx), location, calls: () => calls };
}
function signedIn() {
  return {
    user: { fixture: true }, accessToken: "private-test-bearer",
    expires: new Date(Date.now() + 60_000).toISOString()
  };
}
function assertRedacted(result) {
  const output = JSON.stringify(result);
  for(const secret of ["private-test-bearer", "private=secret", firstParty]) {
    assert(!output.includes(secret));
  }
  assert.equal(result.exported_snapshot_verified, false);
}

test("manual account probe keeps genuine same-page source evidence distinct", async () => {
  for(const [status, payload, authentication, reason] of [
    [200, signedIn(), "verified", "first_party_session_confirmed"],
    [200, {}, "unauthenticated", "provider_reports_no_session"],
    [401, signedIn(), "unauthenticated", "provider_rejected_session"],
    [403, signedIn(), "unknown", "provider_or_network_blocked"],
  ]) {
    const probe = fixture(() => ({ status, json: async () => payload }));
    const result = await probe.run();
    assert.equal(result.authentication, authentication);
    assert.equal(result.reason, reason);
    assert.equal(probe.calls(), 1);
    assertRedacted(result);
  }
});

test("same-origin SPA navigation while awaiting fetch invalidates 200 and 401", async () => {
  for(const status of [200, 401]) {
    let parses = 0;
    const probe = fixture(location => {
      location.href = "https://chatgpt.com/c/new-route?token=private";
      return {status, json: async () => { ++parses; return signedIn(); }};
    });
    const result = await probe.run();
    assert.equal(result.authentication, "unknown");
    assert.equal(result.reason, "tab_changed");
    assert.equal(parses, 0);
    assertRedacted(result);
  }
});

test("same-origin navigation while parsing the body never verifies prior account", async () => {
  const probe = fixture(location => ({
    status: 200, json: async () => {
      location.href = "https://chatgpt.com/c/changed-after-fetch";
      return signedIn();
    }
  }));
  const result = await probe.run();
  assert.equal(result.authentication, "unknown");
  assert.equal(result.reason, "tab_changed");
  assertRedacted(result);
});

test("off-provider navigation and source errors remain unknown without URL disclosure", async () => {
  const probe = fixture(location => {
    location.hostname = "malicious.example";
    location.href = "https://malicious.example/?cookie=private";
    return {status: 401, json: async()=>({})};
  });
  const result = await probe.run();
  assert.equal(result.reason, "tab_changed");
  assert.equal(result.authentication, "unknown");
  assertRedacted(result);
  let contacted = 0;
  const wrong = fixture(() => { contacted++; return {status:200,json:async()=>signedIn()}; },
    {hostname:"malicious.example",href:"https://malicious.example/"});
  const unknown = await wrong.run();
  assert.equal(unknown.authentication, "unknown");
  assert.equal(unknown.reason, "unsupported_origin");
  assert.equal(contacted, 0);
});

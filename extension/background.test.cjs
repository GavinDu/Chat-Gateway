const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function loadBackground(linkedProviders, failingProviders = []) {
  const calls = [];
  const timers = new Map();
  const clearedTimers = new Set();
  let nextTimerId = 0;
  const listeners = {
    installed: [],
    startup: [],
    cookieChanged: [],
    message: [],
  };
  const sandbox = {
    Map,
    Set,
    Promise,
    Object,
    setTimeout(callback, delay) {
      const id = ++nextTimerId;
      timers.set(id, { callback, delay });
      return id;
    },
    clearTimeout(id) {
      clearedTimers.add(id);
    },
    importScripts() {},
    PROVIDERS: { chatgpt: ["chatgpt.com"], claude: ["claude.ai"] },
    syncProvider: async provider => {
      calls.push(provider);
      if (failingProviders.includes(provider)) {
        throw new Error("simulated refresh failure");
      }
    },
    chrome: {
      cookies: {
        onChanged: { addListener: callback => listeners.cookieChanged.push(callback) },
      },
      runtime: {
        onInstalled: { addListener: callback => listeners.installed.push(callback) },
        onStartup: { addListener: callback => listeners.startup.push(callback) },
        onMessage: { addListener: callback => listeners.message.push(callback) },
      },
      storage: {
        local: {
          async get() {
            return { linkedProviders };
          },
        },
      },
    },
  };
  const source = fs.readFileSync(path.join(__dirname, "background.js"), "utf8");
  vm.runInNewContext(source, sandbox, { filename: "background.js" });
  return { calls, listeners, timers, clearedTimers };
}

function flushAsyncWork() {
  return new Promise(resolve => setImmediate(resolve));
}

test("service-worker install and startup restore each unique linked provider", async () => {
  const { calls, listeners } = loadBackground([
    "chatgpt",
    "chatgpt",
    "unsupported-provider",
    "claude",
  ]);

  assert.equal(listeners.installed.length, 1);
  assert.equal(listeners.startup.length, 1);

  listeners.installed[0]();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);

  calls.length = 0;
  listeners.startup[0]();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("one provider refresh failure does not prevent other linked providers", async () => {
  const { calls, listeners } = loadBackground(["chatgpt", "claude"], ["chatgpt"]);

  listeners.startup[0]();
  await flushAsyncWork();

  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("cookie rotation is debounced and rejects lookalike domains", async () => {
  const { calls, listeners, timers, clearedTimers } = loadBackground([]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".evilchatgpt.com" } });
  assert.equal(timers.size, 0, "lookalike domains must not trigger a provider sync");

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: "chatgpt.com" } });
  assert.equal(timers.size, 2);
  assert.ok(clearedTimers.has(1), "the previous debounce timer should be cancelled");
  assert.equal(timers.get(2).delay, 750);

  timers.get(1).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, [], "a stale debounce callback must not sync");

  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt"]);
});

test("two linked providers rotate independently without cross-provider sync", async () => {
  const { calls, listeners, timers } = loadBackground(["chatgpt", "claude"]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: ".claude.ai" } });
  assert.equal(timers.size, 2, "each provider should own an independent debounce timer");

  timers.get(1).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt"]);

  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["chatgpt", "claude"]);
});

test("rapid cookie rotations coalesce per provider while preserving the other provider", async () => {
  const { calls, listeners, timers, clearedTimers } = loadBackground(["chatgpt", "claude"]);
  const onCookieChanged = listeners.cookieChanged[0];

  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });
  onCookieChanged({ cookie: { domain: ".claude.ai" } });
  onCookieChanged({ cookie: { domain: ".chatgpt.com" } });

  assert.equal(timers.size, 3);
  assert.ok(clearedTimers.has(1), "only the previous ChatGPT timer should be cancelled");

  timers.get(1).callback();
  timers.get(2).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["claude"], "Claude's independent timer should still fire");

  timers.get(3).callback();
  await flushAsyncWork();
  assert.deepEqual(calls, ["claude", "chatgpt"]);
});

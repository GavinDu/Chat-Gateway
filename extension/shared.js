const PROVIDERS = {
  qwen: ["qwen.ai", "aliyun.com"],
  metaai: ["meta.ai", "facebook.com", "fbcdn.net"],
  claude: ["claude.ai", "anthropic.com"],
  chatgpt: ["chatgpt.com", "openai.com"],
  gemini: ["gemini.google.com", "google.com", "googleusercontent.com"],
  grok: ["grok.com", "x.com", "twitter.com", "x.ai"],
  mistral: ["mistral.ai"],
  perplexity: ["perplexity.ai"],
  manus: ["manus.im"]
};

const PROVIDER_LOGIN_URLS = Object.freeze({
  chatgpt: "https://chatgpt.com/",
  claude: "https://claude.ai/",
  gemini: "https://gemini.google.com/",
  qwen: "https://chat.qwen.ai/",
  metaai: "https://www.meta.ai/",
  grok: "https://grok.com/",
  mistral: "https://chat.mistral.ai/",
  perplexity: "https://www.perplexity.ai/",
  manus: "https://manus.im/app"
});

function normalizeGateway(value) {
  const url = new URL(value);
  if (url.username || url.password || url.search || url.hash || (url.pathname !== "/" && url.pathname !== "")) {
    throw new Error("Gateway URL must be an origin without credentials, path, query, or fragment.");
  }
  if (url.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)) {
    return url.origin;
  }
  if (url.protocol === "https:" && url.hostname && !url.hostname.endsWith(".localhost")) {
    return url.origin;
  }
  throw new Error("Gateway URL must be local HTTP or an explicitly configured HTTPS remote origin.");
}

function randomHex(bytes = 16) {
  const value = crypto.getRandomValues(new Uint8Array(bytes));
  return Array.from(value, byte => byte.toString(16).padStart(2, "0")).join("");
}

async function relayFetch(path, options = {}, expectedConnection = null) {
  const {gateway, token} = await chrome.storage.local.get(["gateway", "token"]);
  // Cookie snapshots must never switch destinations after they were collected.
  // This also protects a caller that re-pairs while a browser cookie read is
  // pending: a new install token is not consent to upload the old snapshot.
  if (expectedConnection && (
    gateway !== expectedConnection.gateway || token !== expectedConnection.token
  )) {
    throw new Error("Gateway connection changed during provider sync; retry linking.");
  }
  if (!gateway) throw new Error("Set the local gateway URL first.");
  // Treat browser storage as untrusted: a stale or tampered gateway value must
  // never redirect provider cookies or the relay token to a remote origin.
  const localGateway = normalizeGateway(gateway);
  const hasGatewayPermission = await chrome.permissions.contains({origins:[localGateway + "/*"]});
  if (!hasGatewayPermission) {
    throw new Error("Gateway host permission is missing. Re-pair and grant access only to the configured gateway origin.");
  }
  if (expectedConnection) {
    const latest = await chrome.storage.local.get(["gateway", "token", "linkedProviders"]);
    if (latest.gateway !== gateway || latest.token !== token ||
        (expectedConnection.provider && !latest.linkedProviders?.includes(expectedConnection.provider))) {
      throw new Error("Provider consent or gateway connection changed during sync.");
    }
  }
  const headers = new Headers(options.headers || {});
  headers.set("X-Relay-Timestamp", String(Math.floor(Date.now() / 1000)));
  headers.set("X-Relay-Nonce", randomHex());
  if (token) headers.set("Authorization", "Bearer " + token);
  if (options.body) headers.set("Content-Type", "application/json");
  // Fail closed on HTTP redirects: a 307/308 could replay the provider
  // cookie snapshot body to a different origin before CORS responds.
  return fetch(localGateway + path, {...options, headers, cache:"no-store", redirect:"error"});
}

async function providerCookieSnapshot(provider) {
  const domains = PROVIDERS[provider];
  if (!domains) throw new Error("Unsupported provider.");
  // A partial permission grant must never overwrite a valid gateway session
  // with an incomplete cookie set. Check every declared root and subdomain
  // pattern before reading or transmitting any session material.
  const requiredOrigins = domains.flatMap(domain => [
    "https://" + domain + "/*",
    "https://*." + domain + "/*"
  ]);
  const hasAllPermissions = await chrome.permissions.contains({origins:requiredOrigins});
  if (!hasAllPermissions) {
    throw new Error("Missing provider host permissions. Refusing to send a partial cookie snapshot; re-link the provider and grant all requested domains.");
  }
  const all = [];
  for (const domain of domains) {
    const cookies = await chrome.cookies.getAll({domain});
    for (const cookie of cookies) {
      const normalized = cookie.domain.replace(/^\./, "").toLowerCase();
      if (domains.some(root => normalized === root || normalized.endsWith("." + root))) {
        all.push({
          name: cookie.name, value: cookie.value, domain: cookie.domain,
          path: cookie.path || "/", secure: Boolean(cookie.secure), http_only: Boolean(cookie.httpOnly)
        });
      }
    }
  }
  // Chrome permissions may be revoked while asynchronous cookie reads are
  // pending. Never return collected session material after consent changes.
  if (!await chrome.permissions.contains({origins:requiredOrigins})) {
    throw new Error("Provider host permissions were revoked during cookie collection.");
  }
  const deduped = new Map(all.map(cookie => [cookie.domain + "\n" + cookie.path + "\n" + cookie.name, cookie]));
  return {cookies:Array.from(deduped.values()), local_storage:{}, ts:new Date().toISOString()};
}

async function syncProvider(provider) {
  const {gateway, token, linkedProviders = []} = await chrome.storage.local.get(["gateway", "token", "linkedProviders"]);
  if (!gateway || !token || !linkedProviders.includes(provider)) return;
  const snapshot = await providerCookieSnapshot(provider);
  // A permission/cookie read can await long enough for unpair, re-pair or
  // explicit provider unlink. Re-check ownership and consent immediately
  // before preparing the upload, and bind the relay fetch to that snapshot.
  const current = await chrome.storage.local.get(["gateway", "token", "linkedProviders"]);
  if (current.gateway !== gateway || current.token !== token ||
      !current.linkedProviders?.includes(provider)) return;
  // Missing permissions or an expired browser session must not overwrite a
  // previously valid gateway session with an empty snapshot.
  if (snapshot.cookies.length === 0) return;
  // Storage reads also await; recheck host consent at the final upload
  // boundary rather than relying only on the snapshot-time checks.
  const domains = PROVIDERS[provider];
  const requiredOrigins = domains.flatMap(domain => [
    "https://" + domain + "/*", "https://*." + domain + "/*"
  ]);
  if (!await chrome.permissions.contains({origins:requiredOrigins})) {
    throw new Error("Provider host permissions were revoked before session sync.");
  }
  const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {
    method:"POST", body:JSON.stringify(snapshot)
  }, {gateway, token, provider});
  if (!response.ok) throw new Error("Gateway rejected session update (" + response.status + "). Check vault mode and provider permissions.");
}

const PROVIDERS = {
  qwen: ["qwen.ai", "aliyun.com"],
  metaai: ["meta.ai", "facebook.com", "fbcdn.net"],
  claude: ["claude.ai", "anthropic.com"],
  chatgpt: ["chatgpt.com", "openai.com"],
  gemini: ["gemini.google.com", "google.com", "googleusercontent.com"],
  grok: ["grok.com", "x.com", "twitter.com", "x.ai"],
  mistral: ["mistral.ai"],
  perplexity: ["perplexity.ai"]
};

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

async function relayFetch(path, options = {}) {
  const {gateway, token} = await chrome.storage.local.get(["gateway", "token"]);
  if (!gateway) throw new Error("Set the local gateway URL first.");
  // Treat browser storage as untrusted: a stale or tampered gateway value must
  // never redirect provider cookies or the relay token to a remote origin.
  const localGateway = normalizeGateway(gateway);
  const headers = new Headers(options.headers || {});
  headers.set("X-Relay-Timestamp", String(Math.floor(Date.now() / 1000)));
  headers.set("X-Relay-Nonce", randomHex());
  if (token) headers.set("Authorization", "Bearer " + token);
  if (options.body) headers.set("Content-Type", "application/json");
  return fetch(localGateway + path, {...options, headers, cache:"no-store"});
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
  const deduped = new Map(all.map(cookie => [cookie.domain + "\n" + cookie.path + "\n" + cookie.name, cookie]));
  return {cookies:Array.from(deduped.values()), local_storage:{}, ts:new Date().toISOString()};
}

async function syncProvider(provider) {
  const {gateway, token, linkedProviders = []} = await chrome.storage.local.get(["gateway", "token", "linkedProviders"]);
  if (!gateway || !token || !linkedProviders.includes(provider)) return;
  const snapshot = await providerCookieSnapshot(provider);
  // Missing permissions or an expired browser session must not overwrite a
  // previously valid gateway session with an empty snapshot.
  if (snapshot.cookies.length === 0) return;
  const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {
    method:"POST", body:JSON.stringify(snapshot)
  });
  if (!response.ok) throw new Error("Gateway rejected session update (" + response.status + "). Check vault mode and provider permissions.");
}

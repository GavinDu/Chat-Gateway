const GATEWAY = "http://127.0.0.1:8080";
const PROVIDERS = {
  qwen: { origins: ["https://chat.qwen.ai"], localStorageKeys: [] },
  metaai: { origins: ["https://www.meta.ai"], localStorageKeys: [] },
  claude: { origins: ["https://claude.ai"], localStorageKeys: [] },
  chatgpt: { origins: ["https://chatgpt.com"], localStorageKeys: [] },
  gemini: { origins: ["https://gemini.google.com"], localStorageKeys: [] },
  grok: { origins: ["https://grok.com"], localStorageKeys: [] },
  mistral: { origins: ["https://chat.mistral.ai"], localStorageKeys: [] },
  perplexity: { origins: ["https://www.perplexity.ai"], localStorageKeys: [] }
};
const changedTimers = new Map();

async function api(path, options = {}, authenticated = true) {
  const { token } = await chrome.storage.local.get("token");
  const headers = { "Content-Type": "application/json", ...(options.headers || {}) };
  if (authenticated) {
    if (!token) throw new Error("Pair this extension with the local gateway first.");
    headers.Authorization = `Bearer ${token}`;
  }
  const response = await fetch(GATEWAY + path, { ...options, headers, cache: "no-store" });
  if (!response.ok) {
    throw new Error(response.status === 401 ? "Pairing expired or revoked. Pair again." :
      response.status === 403 ? "Gateway rejected this extension origin." :
      `Gateway request failed (HTTP ${response.status}).`);
  }
  return response.status === 204 ? null : response.json();
}
async function pair(code) {
  if (!/^\d{6,10}$/.test(code)) throw new Error("Enter the short-lived pairing code shown by the local gateway.");
  const result = await api("/relay/v1/pair", { method: "POST", body: JSON.stringify({ code }) }, false);
  if (!result || typeof result.token !== "string" || result.token.length < 32) throw new Error("Gateway returned an invalid pairing response.");
  await chrome.storage.local.set({ token: result.token });
  return { paired: true };
}
async function currentProvider(tabId) {
  const tab = await chrome.tabs.get(tabId);
  if (!tab.url) throw new Error("Open an approved provider tab first.");
  const url = new URL(tab.url);
  const entry = Object.entries(PROVIDERS).find(([, p]) => p.origins.includes(url.origin));
  if (!entry) throw new Error("This tab is not an approved provider origin.");
  return { provider: entry[0], origin: url.origin, tab };
}
async function exportSession(tabId) {
  const { provider, origin } = await currentProvider(tabId);
  const consentKey = `consent:${provider}`;
  const { [consentKey]: consent } = await chrome.storage.local.get(consentKey);
  if (!consent) {
    const { pendingConsent } = await chrome.storage.local.get("pendingConsent");
    if (pendingConsent !== provider) throw new Error("Confirm provider linking in the extension popup first.");
    await chrome.storage.local.set({ [consentKey]: true });
    await chrome.storage.local.remove("pendingConsent");
  }
  const cookies = await chrome.cookies.getAll({ url: origin });
  const storage = await chrome.tabs.sendMessage(tabId, { type: "CG_READ_LOCAL_STORAGE", keys: PROVIDERS[provider].localStorageKeys });
  const session = {
    provider, origin,
    cookies: cookies.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, secure: c.secure, http_only: c.httpOnly })),
    local_storage: storage && storage.values ? storage.values : {}
  };
  await api("/relay/v1/session", { method: "POST", body: JSON.stringify(session) });
  return { provider, cookieCount: session.cookies.length };
}
async function status() { return api("/relay/v1/status"); }
async function unlink(provider) {
  if (!Object.hasOwn(PROVIDERS, provider)) throw new Error("Unknown provider.");
  await api(`/relay/v1/session/${encodeURIComponent(provider)}`, { method: "DELETE" });
  await chrome.storage.local.remove(`consent:${provider}`);
  return { unlinked: provider };
}
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  (async () => {
    try {
      switch (message && message.type) {
        case "CG_PAIR": sendResponse({ ok: true, result: await pair(message.code) }); break;
        case "CG_PREPARE_LINK":
          if (!Object.hasOwn(PROVIDERS, message.provider)) throw new Error("Unknown provider.");
          await chrome.storage.local.set({ pendingConsent: message.provider });
          sendResponse({ ok: true }); break;
        case "CG_LINK_TAB": sendResponse({ ok: true, result: await exportSession(message.tabId) }); break;
        case "CG_STATUS": sendResponse({ ok: true, result: await status() }); break;
        case "CG_UNLINK": sendResponse({ ok: true, result: await unlink(message.provider) }); break;
        default: throw new Error("Unsupported extension action.");
      }
    } catch (error) {
      sendResponse({ ok: false, error: error instanceof Error ? error.message : "Relay action failed." });
    }
  })();
  return true;
});
chrome.cookies.onChanged.addListener(change => {
  if (change.removed || !change.cookie || !change.cookie.domain) return;
  const domain = change.cookie.domain.replace(/^\./, "");
  const entry = Object.entries(PROVIDERS).find(([, p]) => p.origins.some(origin => new URL(origin).hostname === domain));
  if (!entry) return;
  const [provider] = entry;
  clearTimeout(changedTimers.get(provider));
  changedTimers.set(provider, setTimeout(async () => {
    try {
      const { [`consent:${provider}`]: consent, token } = await chrome.storage.local.get([`consent:${provider}`, "token"]);
      if (!consent || !token) return;
      const origin = PROVIDERS[provider].origins[0];
      const cookies = await chrome.cookies.getAll({ url: origin });
      await api("/relay/v1/session", { method: "POST", body: JSON.stringify({
        provider, origin,
        cookies: cookies.map(c => ({ name: c.name, value: c.value, domain: c.domain, path: c.path, secure: c.secure, http_only: c.httpOnly })),
        local_storage: {}
      }) });
    } catch (_) { /* Never log session data or authentication errors. */ }
  }, 500));
});

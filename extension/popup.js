const $ = (id) => document.getElementById(id);
const providers = {
  chatgpt: ["chatgpt.com", "openai.com"],
  claude: ["claude.ai", "anthropic.com"],
  gemini: ["gemini.google.com", "google.com", "googleusercontent.com"],
  metaai: ["meta.ai", "facebook.com", "fbcdn.net"],
  grok: ["grok.com", "x.com", "twitter.com", "x.ai"],
  qwen: ["qwen.ai", "aliyun.com"],
  mistral: ["mistral.ai"],
  perplexity: ["perplexity.ai"]
};
const gateway = () => {
  const port = Number($("port").value);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("Enter a valid gateway port.");
  return `http://127.0.0.1:${port}`;
};
const say = (message) => { $("status").textContent = message; };
const token = async () => (await chrome.storage.session.get("relayToken")).relayToken;
async function api(path, options = {}) {
  const base = gateway();
  const auth = await token();
  const headers = { "Content-Type": "application/json", ...(options.headers || {}) };
  if (auth) headers.Authorization = `Bearer ${auth}`;
  const response = await fetch(base + path, { ...options, headers, cache: "no-store" });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(body?.error?.message || body?.error || `Gateway returned HTTP ${response.status}`);
  return body;
}
$("pair").addEventListener("click", async () => {
  try {
    const code = $("code").value.trim();
    if (!/^\d{6}$/.test(code)) throw new Error("Enter the six-digit code printed by the gateway.");
    await chrome.permissions.request({ origins: ["http://127.0.0.1/*"] });
    const result = await api("/relay/v1/pair", { method: "POST", body: JSON.stringify({ code }) });
    await chrome.storage.session.set({ relayToken: result.token });
    $("code").value = "";
    say("Paired. The token stays in this browser session and is not persisted across browser restarts.");
  } catch (error) { say(error.message); }
});
$("capture").addEventListener("click", async () => {
  try {
    const auth = await token();
    if (!auth) throw new Error("Pair with the gateway first.");
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (!tab?.url) throw new Error("No active tab was found.");
    const url = new URL(tab.url);
    if (url.protocol !== "https:") throw new Error("Session capture is allowed only on HTTPS provider pages.");
    const provider = $("provider").value;
    const bases = providers[provider];
    const base = bases.find((domain) => url.hostname === domain || url.hostname.endsWith("." + domain));
    if (!base) throw new Error("The active tab is not on an approved domain for the selected provider.");
    const originPattern = `https://${url.hostname}/*`;
    const granted = await chrome.permissions.request({ origins: [originPattern] });
    if (!granted) throw new Error("Provider-site permission was not granted.");
    const cookies = await chrome.cookies.getAll({ url: url.origin });
    const filtered = cookies.filter((cookie) => {
      const domain = cookie.domain.replace(/^\./, "").toLowerCase();
      return domain === base || domain.endsWith("." + base);
    }).map((cookie) => ({
      name: cookie.name, value: cookie.value, domain: cookie.domain, path: cookie.path,
      secure: cookie.secure, http_only: cookie.httpOnly
    }));
    if (filtered.length === 0) throw new Error("No provider cookies were found for this tab.");
    await api(`/relay/v1/sessions/${encodeURIComponent(provider)}`, {
      method: "POST",
      body: JSON.stringify({ cookies: filtered, local_storage: {}, ts: new Date().toISOString() })
    });
    say(`Imported ${filtered.length} cookies for ${provider}. The gateway requires encrypted vault mode.`);
  } catch (error) { say(error.message); }
});
$("refresh").addEventListener("click", async () => {
  try {
    const result = await api("/relay/v1/status");
    say("Paired. " + result.providers.map((p) => `${p.provider}: ${p.session_present ? "linked" : "not linked"}`).join("\n"));
  } catch (error) { say(error.message); }
});
$("unlink").addEventListener("click", async () => {
  try {
    if (await token()) await api("/relay/v1/unlink", { method: "POST", body: "{}" });
    await chrome.storage.session.remove("relayToken");
    say("Unpaired. Restart the gateway to generate a new one-time pairing code.");
  } catch (error) { say(error.message); }
});

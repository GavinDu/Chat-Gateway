const $ = id => document.getElementById(id);
function message(text) { $("message").textContent = text; }

async function refreshStatus() {
  const response = await relayFetch("/relay/v1/status");
  if (!response.ok) throw new Error("Status request failed (" + response.status + ").");
  const data = await response.json();
  $("status").replaceChildren(...data.providers.map(item => {
    const li = document.createElement("li");
    li.textContent = item.provider + ": " + (item.linked ? "linked" : "not linked");
    return li;
  }));
  if (!data.vault_mode) message("Vault mode is off. Set CHAT_GATEWAY_SESSION_STORE=vault and CHAT_GATEWAY_VAULT_KEY, then restart the gateway.");
}

async function initialize() {
  const stored = await chrome.storage.local.get(["gateway","token"]);
  if (stored.gateway) $("gateway").value = stored.gateway;
  $("pairing").hidden = Boolean(stored.token);
  $("linked").hidden = !stored.token;
  if (stored.token) {
    try { await refreshStatus(); } catch (error) { message(error.message); }
  }
}

$("pair").addEventListener("click", async () => {
  try {
    const gateway = normalizeGateway($("gateway").value);
    const code = $("code").value.trim();
    if (!/^\d{8}$/.test(code)) throw new Error("Enter the 8-digit code printed by the gateway.");
    const response = await fetch(gateway + "/relay/v1/pair", {
      method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify({code}), cache:"no-store"
    });
    if (!response.ok) throw new Error("Pairing rejected (" + response.status + "). Check extension ID configuration and code expiry.");
    const data = await response.json();
    await chrome.storage.local.set({gateway,token:data.token,linkedProviders:[]});
    $("pairing").hidden = true; $("linked").hidden = false;
    message("Paired. The token is stored in extension-local storage; protect this browser profile.");
    await refreshStatus();
  } catch (error) { message(error.message); }
});

$("link").addEventListener("click", async () => {
  try {
    const provider = $("provider").value;
    const patterns = PROVIDERS[provider].flatMap(domain => ["https://" + domain + "/*", "https://*." + domain + "/*"]);
    const granted = await chrome.permissions.request({origins:patterns});
    if (!granted) throw new Error("Provider permissions were not granted.");
    const snapshot = await providerCookieSnapshot(provider);
    if (snapshot.cookies.length === 0) throw new Error("No cookies found for this provider. Sign in normally in Chrome first.");
    const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {method:"POST",body:JSON.stringify(snapshot)});
    if (!response.ok) throw new Error("Gateway rejected session import (" + response.status + "). Ensure vault mode is enabled and provider domains are allowed.");
    const stored = await chrome.storage.local.get(["linkedProviders"]);
    const linkedProviders = Array.from(new Set([...(stored.linkedProviders || []),provider]));
    await chrome.storage.local.set({linkedProviders});
    message("Linked " + provider + ". The gateway activated this session in its embedded browser; no restart is required, and cookie changes sync automatically.");
    await refreshStatus();
  } catch (error) { message(error.message); }
});

$("refresh").addEventListener("click", async () => {
  try { await refreshStatus(); message("Status refreshed."); } catch (error) { message(error.message); }
});


$("unlink").addEventListener("click", async () => {
  try {
    const provider = $("provider").value;
    const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {method:"DELETE",body:"{}"});
    if (!response.ok) throw new Error("Could not unlink " + provider + ".");
    const stored = await chrome.storage.local.get(["linkedProviders"]);
    await chrome.storage.local.set({linkedProviders:(stored.linkedProviders || []).filter(value => value !== provider)});
    message("Unlinked " + provider + ". The gateway cleared the persisted session, embedded-browser cookies, and origin localStorage.");
    await refreshStatus();
  } catch (error) { message(error.message); }
});

$("unpair").addEventListener("click", async () => {
  try {
    const response = await relayFetch("/relay/v1/unpair",{method:"POST",body:"{}"});
    if (!response.ok) throw new Error("Could not revoke the install token.");
    await chrome.storage.local.remove(["token","linkedProviders"]);
    $("pairing").hidden = false; $("linked").hidden = true;
    message("Extension unpaired. Provider vault entries remain; unlink them individually before unpairing if you want them deleted.");
  } catch (error) { message(error.message); }
});

initialize();

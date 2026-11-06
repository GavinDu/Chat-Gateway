const $ = id => document.getElementById(id);
function message(text) { $("message").textContent = text; }

async function refreshStatus() {
  const response = await relayFetch("/relay/v1/status");
  if (!response.ok) throw new Error("Status request failed (" + response.status + ").");
  const data = await response.json();
  const items = await Promise.all(data.providers.map(async item => {
    const li = document.createElement("li");
    const heading = document.createElement("strong");
    heading.textContent = item.provider;
    li.append(heading);

    const status = document.createElement("p");
    status.className = "provider-detail";
    const enabled = item.enabled === false ? "Disabled in gateway config" : "Enabled in gateway";
    const linked = item.linked ? "Session saved" : "Not linked";
    let browser = "Browser permission not granted";
    const domains = PROVIDERS[item.provider] || [];
    const origins = domains.flatMap(domain => ["https://" + domain + "/*", "https://*." + domain + "/*"]);
    if (origins.length && await chrome.permissions.contains({origins})) {
      const present = await Promise.all(domains.map(domain => chrome.cookies.getAll({domain})));
      browser = present.some(cookies => cookies.length > 0) ? "Browser cookies present" : "No browser cookies";
    }
    const auth = item.authentication === "verified" ? "Login verified"
      : item.authentication === "needs_login" ? "Login needed"
      : "Login unverified";
    status.textContent = [enabled, linked, browser, auth].join(" · ");
    li.append(status);

    const openButton = document.createElement("button");
    openButton.type = "button";
    openButton.className = "secondary provider-action";
    openButton.textContent = "Open login";
    openButton.addEventListener("click", () => openProviderLogin(item.provider).catch(error => message(error.message)));
    li.append(openButton);

    const linkButton = document.createElement("button");
    linkButton.type = "button";
    linkButton.className = "provider-action";
    linkButton.textContent = item.linked ? "Sync session" : "Link account";
    linkButton.disabled = item.enabled === false;
    linkButton.addEventListener("click", () => {
      $("provider").value = item.provider;
      $("link").click();
    });
    li.append(linkButton);

    if (item.linked) {
      const unlinkButton = document.createElement("button");
      unlinkButton.type = "button";
      unlinkButton.className = "danger provider-action";
      unlinkButton.textContent = "Unlink";
      unlinkButton.addEventListener("click", () => {
        $("provider").value = item.provider;
        $("unlink").click();
      });
      li.append(unlinkButton);
    }
    return li;
  }));
  $("status").replaceChildren(...items);
  if (!data.vault_mode) message("Vault mode is off. Set CHAT_GATEWAY_SESSION_STORE=vault and CHAT_GATEWAY_VAULT_KEY (or CHAT_GATEWAY_VAULT_KEY_FILE), then restart the gateway.");
}

async function openProviderLogin(provider) {
  const url = PROVIDER_LOGIN_URLS[provider];
  if (!url) throw new Error("This provider is not yet supported by Chat-Gateway.");
  await chrome.tabs.create({url});
  message("Sign in on the provider website. Then return here and choose Link account. Credentials and MFA stay on the provider website.");
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
    const granted = await chrome.permissions.request({origins:[gateway + "/*"]});
    if (!granted) throw new Error("Gateway host permission was not granted.");
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

$("open-login").addEventListener("click", async () => {
  try { await openProviderLogin($("provider").value); } catch (error) { message(error.message); }
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
    if (!confirm("Remove the saved " + provider + " session from Chat-Gateway?")) return;
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

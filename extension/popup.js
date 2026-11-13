const $ = id => document.getElementById(id);
function message(text) { $("message").textContent = text; }

// Some extension runtimes do not expose the tabs API in popups. In that case,
// keep a clickable link available instead of leaving a pending approval that
// the operator cannot open. The URL comes only from validated gateway/provider
// origins; links never include credentials or polling secrets.
function messageWithLink(text, url, label) {
  const target = $("message");
  target.textContent = text + " ";
  const link = document.createElement("a");
  link.href = url;
  link.target = "_blank";
  link.rel = "noopener noreferrer";
  link.textContent = label;
  target.append(link);
}

function pendingApprovalUrl(pending) {
  if (!/^[a-f0-9]{64}$/.test(pending.request_id)) throw new Error("Invalid pending approval request.");
  return normalizeGateway(pending.gateway) + "/relay/approve/" + pending.request_id;
}

function showPendingApproval(pending) {
  messageWithLink("Waiting for local approval.", pendingApprovalUrl(pending), "Open approval page");
}

async function openExtensionTab(url) {
  try {
    if (typeof chrome.tabs?.create === "function") {
      await chrome.tabs.create({url});
      return true;
    }
    // Some extension popup hosts omit the tabs API entirely.
    // A direct user gesture may still allow opening a new browser tab.
    if (typeof window.open === "function") {
      const opened = window.open(url, "_blank", "noopener,noreferrer");
      if (opened) return true;
    }
  } catch {
    // Preserve a clickable link when automatic navigation is blocked.
  }
  return false;
}

function showGatewayConnection(connected) {
  $("pairing").hidden = connected;
  $("linked").hidden = !connected;
  $("gateway").disabled = connected;
}

async function refreshStatus(force = false) {
  // The service-worker API exposes gateway status together with its own
  // redacted connectivity/sync failures. Fall back to direct status for older
  // worker versions or when the worker is unavailable.
  const diagnostics = await chrome.runtime.sendMessage({type:"relay-diagnostics", force}).catch(() => null);
  const response = diagnostics?.ok && diagnostics.gateway
    ? {ok:true, status:200, json:async () => diagnostics.gateway}
    : await relayFetch("/relay/v1/status", {method:"POST",body:"{}"});
  if (response.status === 401 || response.status === 403) {
    const state = await chrome.storage.local.get(["gateway", "connectionMode"]);
    if (state.connectionMode === "local" && isLocalGateway(state.gateway)) {
      throw new Error("Local relay access rejected. Check the gateway mode and optional extension ID restriction.");
    }
    await chrome.storage.local.remove("token");
    showGatewayConnection(false);
    throw new Error("Gateway connection expired or was revoked. Choose Connect to gateway to approve it again.");
  }
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
    // Chrome can revoke a provider's permission or fail a cookie read while
    // another provider is healthy. Keep the dashboard usable and fail closed
    // for only the affected provider; do not lose all login/logout controls.
    try {
      if (origins.length && await chrome.permissions.contains({origins})) {
        const present = await Promise.all(domains.map(domain => chrome.cookies.getAll({domain})));
        browser = present.some(cookies => cookies.length > 0) ? "Browser cookies present" : "No browser cookies";
      }
    } catch {
      browser = "Browser cookie check unavailable";
    }
    const auth = item.authentication === "verified" ? "Login verified"
      : item.authentication === "needs_login" ? "Login needed"
      : "Login unverified";
    const blocker = item.manual_verification_required ? "Manual provider verification required; open website"
      : item.availability === "degraded" ? "Gateway check unavailable; retry" : null;
    const runtimeIssue = item.issue && item.issue !== "authentication_unverified"
      ? "Gateway issue: " + item.issue.replaceAll("_", " ") : null;
    const extensionIssue = diagnostics?.extension?.provider_sync?.[item.provider]?.issue;
    status.textContent = [enabled, linked, browser, auth,
      ...(blocker ? [blocker] : []),
      ...(runtimeIssue ? [runtimeIssue] : []),
      ...(extensionIssue ? ["Extension sync: " + extensionIssue.replaceAll("_", " ")] : [])
    ].join(" · ");
    li.append(status);

    const actions = document.createElement("div");
    actions.className = "provider-actions";

    const linkButton = document.createElement("button");
    linkButton.type = "button";
    linkButton.className = "provider-action";
    linkButton.textContent = !item.linked ? "Log in / Connect"
      : item.authentication === "needs_login" ? "Re-login / Sync" : "Sync session";
    linkButton.disabled = item.enabled === false;
    linkButton.addEventListener("click", () => connectProvider(
      item.provider,
      !item.linked || item.authentication === "needs_login"
    ).catch(error => message(error.message)));
    actions.append(linkButton);

    const openButton = document.createElement("button");
    openButton.type = "button";
    openButton.className = "secondary provider-action";
    openButton.textContent = "Open website";
    openButton.addEventListener("click", () => openProviderLogin(item.provider).catch(error => message(error.message)));
    actions.append(openButton);

    if (item.linked) {
      const logoutButton = document.createElement("button");
      logoutButton.type = "button";
      logoutButton.className = "quiet-danger provider-action";
      logoutButton.textContent = "Log out";
      logoutButton.title = "Remove the gateway session and stop cookie resync. Does not sign out from the official website.";
      logoutButton.addEventListener("click", () => logoutProvider(item.provider).catch(error => message(error.message)));
      actions.append(logoutButton);
    }
    li.append(actions);
    return li;
  }));
  $("status").replaceChildren(...items);
  if (!data.vault_mode) message("Vault mode is off. Set CHAT_GATEWAY_SESSION_STORE=vault and CHAT_GATEWAY_VAULT_KEY (or CHAT_GATEWAY_VAULT_KEY_FILE), then restart the gateway.");
}

async function openProviderLogin(provider) {
  const url = PROVIDER_LOGIN_URLS[provider];
  if (!url) throw new Error("This provider is not yet supported by Chat-Gateway.");
  if (await openExtensionTab(url)) {
    message("Use your existing provider session or sign in on the provider website. Connected accounts sync automatically; credentials and MFA stay there.");
  } else {
    messageWithLink("Your browser cannot open a tab automatically.", url, "Open provider website");
  }
}

// Detect local-only relay mode before asking for a pairing code. A plain
// read-only mode probe contains no token and never collects provider cookies.
async function connectLocalGateway(gateway, requestPermission = false) {
  if (!isLocalGateway(gateway)) return false;
  const origins = [gateway + "/*"];
  const allowed = requestPermission
    ? await chrome.permissions.request({origins})
    : await chrome.permissions.contains({origins});
  if (!allowed) return false;
  const response = await fetch(gateway + "/relay/v1/mode", {
    method:"GET", cache:"no-store", redirect:"error"
  });
  if (!response.ok || (await response.json()).no_pairing !== true) return false;
  const previous = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
  // Opening the popup must not invalidate the fingerprint of successfully
  // imported sessions, or the next background repair would restart browsers.
  if (previous.gateway === gateway && previous.connectionMode === "local" && !previous.token) {
    showGatewayConnection(true);
    await refreshStatus();
    return true;
  }
  const linkedProviders = previous.gateway === gateway ? (previous.linkedProviders || []) : [];
  await chrome.storage.local.set({
    gateway, connectionMode:"local", linkedProviders, localDisconnected:false
  });
  await chrome.storage.local.remove(["token", "pendingConnection", "relaySnapshotVersions"]);
  await chrome.alarms?.clear?.("relay-connection");
  showGatewayConnection(true);
  await refreshStatus(true);
  return true;
}

function showConnectionMode() {
  const remote = $("gateway").value.trim().startsWith("https://");
  $("remote-pairing").hidden = !remote;
  $("connection-help").textContent = remote
    ? "Private remote gateways can use the legacy one-time pairing code."
    : "Local gateway connects directly, without a pairing code or install token.";
}
$("gateway").addEventListener("input", showConnectionMode);

async function initialize() {
  const stored = await chrome.storage.local.get(["gateway","token","connectionMode","localDisconnected"]);
  if (stored.gateway) $("gateway").value = stored.gateway;
  showConnectionMode();
  if (!stored.localDisconnected && isLocalGateway($("gateway").value)) {
    try {
      if (await connectLocalGateway(normalizeGateway($("gateway").value))) return;
    } catch (error) {
      message("Local gateway unavailable: " + error.message);
    }
  }
  showGatewayConnection(hasRelayConnection(stored));
  if (hasRelayConnection(stored)) {
    try { await refreshStatus(); } catch (error) { message(error.message); }
  }
}

$("pair").addEventListener("click", async () => {
  try {
    const gateway = normalizeGateway($("gateway").value);
    if (await connectLocalGateway(gateway, true)) return;
    const remote = gateway.startsWith("https://");
    const code = remote ? $("code").value.trim() : "";
    if (remote && !/^[0-9]{8}$/.test(code)) throw new Error("Enter the fresh 8-digit code from your private remote gateway.");
    const granted = await chrome.permissions.request({origins:[gateway + "/*"]});
    if (!granted) throw new Error("Gateway permission was not granted.");
    // A popup closure must not strand a valid approval. Reopen the same page
    // instead of repeatedly creating and invalidating requests.
    if (!remote) {
      let {pendingConnection} = await chrome.storage.local.get(["pendingConnection"]);
      if (pendingConnection?.gateway === gateway && Date.now() < pendingConnection.expires) {
        const checked = await updateConnection();
        const current = await chrome.storage.local.get(["token", "pendingConnection"]);
        if (current.token) return;
        pendingConnection = current.pendingConnection;
        if (pendingConnection?.gateway === gateway && Date.now() < pendingConnection.expires) {
          if (!checked) return;
          if (!await openExtensionTab(pendingApprovalUrl(pendingConnection))) showPendingApproval(pendingConnection);
          else message("Approve the pending connection in the local tab, then return here.");
          return;
        }
      }
    }
    const response = await fetch(gateway + (remote ? "/relay/v1/pair" : "/relay/v1/connect/request"), {
      method:"POST", headers:{"Content-Type":"application/json"},
      body:JSON.stringify(remote ? {code} : {}), cache:"no-store", redirect:"error"
    });
    if (!response.ok) throw new Error(remote ? "Remote pairing failed. Check the gateway and request a fresh pairing code." : response.status === 409
      ? "An approval is already pending. Finish it in the local approval tab or retry in five minutes."
      : "Could not request approval. Check the gateway URL and configured extension ID.");
    const data = await response.json();
    if (remote) {
      if (!/^[a-f0-9]{64}$/.test(data.token)) throw new Error("Invalid connection response.");
      await chrome.storage.local.set({gateway,token:data.token,linkedProviders:[]});
      await chrome.storage.local.remove("pendingConnection");
      await chrome.alarms?.clear?.("relay-connection");
      $("code").value = "";
      await updateConnection();
      return;
    }
    if (!/^[a-f0-9]{64}$/.test(data.request_id) || !/^[a-f0-9]{64}$/.test(data.poll_secret)) throw new Error("Invalid connection response.");
    await chrome.storage.local.set({pendingConnection:{gateway, ...data, expires:Date.now()+300000}});
    // The popup itself polls every five seconds. Alarms are only a background
    // convenience, not a prerequisite for local approval.
    await chrome.alarms?.create?.("relay-connection", {periodInMinutes:1});
    const pending = {gateway, ...data};
    if (await openExtensionTab(pendingApprovalUrl(pending))) {
      message("Approve the connection in the local tab, then return here.");
    } else {
      showPendingApproval(pending);
    }
  } catch (error) { message(error.message); }
});

// Linking consents to automatic updates from this provider. The browser only
// sends cookies for domains the user has explicitly granted permission to.
async function connectProvider(provider, openLogin = false) {
  const domains = PROVIDERS[provider];
  if (!domains) throw new Error("Unsupported provider.");
  // Pairing may be revoked or replaced while Chrome's permission prompt or
  // browser cookie read is pending. Consent must belong to the exact install.
  const connection = await chrome.storage.local.get(["gateway", "token", "connectionMode"]);
  if (!hasRelayConnection(connection)) {
    throw new Error("Connect to gateway before linking a provider.");
  }
  const patterns = domains.flatMap(domain => ["https://" + domain + "/*", "https://*." + domain + "/*"]);
  if (!await chrome.permissions.request({origins:patterns})) {
    throw new Error("Provider permissions were not granted.");
  }
  const snapshot = await providerCookieSnapshot(provider);
  const stored = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
  if (stored.gateway !== connection.gateway || stored.token !== connection.token || stored.connectionMode !== connection.connectionMode) {
    throw new Error("Gateway connection changed while linking the provider. Reconnect and retry.");
  }
  const prior = stored.linkedProviders || [];
  await chrome.storage.local.set({linkedProviders:Array.from(new Set([...prior,provider]))});
  if (snapshot.cookies.length === 0) {
    await openProviderLogin(provider);
    // openProviderLogin already provides a manual clickable link when a tab
    // cannot open; do not overwrite that link with a second status message.
    return;
  }
  try {
    // The service worker serializes imports and logout per provider. Never
    // send a manual import in parallel with a logout request.
    const result = await chrome.runtime.sendMessage({type:"sync-provider",provider});
    if (!result?.ok) throw new Error(result?.error || ("Gateway could not sync " + provider));
    // An empty or superseded browser-cookie capture is a skipped upload,
    // never evidence that the provider session was saved. The service worker
    // still skips empty *automatic* refreshes to preserve older credentials.
    if (result.syncStatus === "no_cookies") {
      throw new Error("Provider cookies disappeared before sync. Sign in to the provider and retry linking.");
    }
    if (result.syncStatus === "saved_activation_pending") {
      await refreshStatus();
      message("Session saved. The provider browser is still unavailable; check gateway diagnostics before retrying.");
      if (openLogin) await openProviderLogin(provider);
      return;
    }
    if (result.syncStatus && result.syncStatus !== "uploaded") {
      throw new Error("Provider session was not uploaded; check the gateway connection and retry.");
    }
  } catch (error) {
    // A failed first connection must not opt this provider into background
    // uploads. Leave prior consent unchanged on an existing connection.
    const latest = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
    // A delayed old-install failure must not revoke consent granted by a
    // newly paired gateway. Roll back only the install that initiated sync.
    if (latest.gateway === connection.gateway && latest.token === connection.token && latest.connectionMode === connection.connectionMode) {
      await chrome.storage.local.set({
        linkedProviders:(latest.linkedProviders || []).filter(value => value !== provider || prior.includes(provider))
      });
    }
    throw error;
  }
  await refreshStatus();
  // Log in / Connect and Re-login must open the official provider page even
  // when old cookies exist. Their presence does not prove authentication.
  // An explicit Sync session on an already-linked account remains tab-free.
  if (openLogin) {
    await openProviderLogin(provider);
  } else {
    message("Saved " + provider + " to the gateway. Verify the login status; saved cookies alone do not confirm authentication.");
  }
}

async function logoutProvider(provider) {
  if (!PROVIDERS[provider]) throw new Error("Unsupported provider.");
  if (!confirm("Log out " + provider + " from Chat Gateway and stop automatic sync? This does not sign you out on the provider website. You can reconnect later.")) return;
  // The service worker queues logout behind in-flight cookie syncs, then
  // revokes the persisted gateway snapshot. No website cookies are deleted.
  const result = await chrome.runtime.sendMessage({type:"logout-provider",provider});
  if (!result?.ok) throw new Error(result?.error || "Provider logout could not complete. Retry before reconnecting.");
  await refreshStatus();
  message("Logged out " + provider + " from Chat Gateway. Your website login is unchanged. Choose Log in / Connect to reconnect.");
}

$("refresh").addEventListener("click", async () => {
  try { await refreshStatus(true); message("Status refreshed."); } catch (error) { message(error.message); }
});


$("unpair").addEventListener("click", async () => {
  try {
    const connection = await chrome.storage.local.get(["gateway", "connectionMode"]);
    if (connection.connectionMode === "local" && isLocalGateway(connection.gateway)) {
      await chrome.storage.local.remove(["connectionMode", "token", "linkedProviders", "relaySnapshotVersions", "pendingConnection"]);
      await chrome.storage.local.set({localDisconnected:true});
      await chrome.alarms?.clear?.("relay-connection");
      showGatewayConnection(false);
      message("Local extension disconnected; provider sessions remain in the encrypted gateway vault.");
      return;
    }
    const response = await relayFetch("/relay/v1/unpair",{method:"POST",body:"{}"});
    if (!response.ok) throw new Error("Could not revoke the install token.");
    // A stale approval should not be able to silently reconnect a gateway
    // after the user explicitly revoked this install.
    await chrome.storage.local.remove(["token","linkedProviders","pendingConnection","relaySnapshotVersions"]);
    await chrome.alarms?.clear?.("relay-connection");
    showGatewayConnection(false);
    message("Gateway disconnected. Saved provider sessions remain in the gateway vault. Reconnect to manage them.");
  } catch (error) { message(error.message); }
});

let checkingConnection;
async function updateConnection() {
  if (checkingConnection) return checkingConnection;
  checkingConnection = checkConnection();
  try { return await checkingConnection; }
  finally { checkingConnection = undefined; }
}
async function checkConnection() {
  try {
    const state = await chrome.storage.local.get(["gateway", "token", "connectionMode", "pendingConnection"]);
    if (state.connectionMode === "local" && isLocalGateway(state.gateway)) {
      showGatewayConnection(true);
      await refreshStatus();
      return true;
    }
    const result = await chrome.runtime.sendMessage({type:"poll-connection"});
    const {token, pendingConnection} = await chrome.storage.local.get(["token","pendingConnection"]);
    showGatewayConnection(Boolean(token));
    if (token) await refreshStatus();
    else if (result?.ok !== true) {
      // Do not hide a failed exchange behind an endless waiting message.
      // A stale/missing service worker can also return no acknowledgement.
      message(result?.error || "The connection worker did not respond. Reload Chat Gateway Browser Relay at chrome://extensions, then reconnect.");
      return false;
    } else if (pendingConnection) showPendingApproval(pendingConnection);
    return true;
  } catch (error) { message(error.message); return false; }
}
initialize().then(updateConnection);
// Connected gateway dashboards use a bounded ten-second check for revocation without five-second polling.
// Preserve prompt five-second approval completion only while pairing is pending.
setInterval(updateConnection, 10_000);
setInterval(async () => {
  const {pendingConnection} = await chrome.storage.local.get("pendingConnection");
  if (pendingConnection) await updateConnection();
}, 5_000);
window.addEventListener("focus", updateConnection);

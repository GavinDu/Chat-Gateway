importScripts("shared.js");

const debounceTimers = new Map();
// Internal API snapshots contain only fixed health categories and timestamps.
// Never retain or return provider cookies, install credentials or raw errors.
const providerDiagnostics = new Map();
// Keep cache shorter than the popup check interval so token revocation is
// never masked by a still-valid dashboard snapshot on the next poll.
const CONNECTION_DIAGNOSTICS_TTL_MS = 5_000;
let lastGatewayCheck = null;
let lastGatewayCheckAt = 0;
let lastGatewayIdentity = null;
function classifyRelayFailure(error) {
  const message = String(error?.message || "");
  if (message.includes("timed out") || message.includes("cancelled")) return "gateway_timeout";
  if (message.includes("permission") || message.includes("consent")) return "permission_or_consent";
  if (message.includes("(504)")) return "activation_timeout";
  if (message.includes("(403)") || message.includes("(401)")) return "gateway_unauthorized";
  return "relay_unavailable";
}
function recordProviderDiagnostic(provider, state, errorCode = null) {
  providerDiagnostics.set(provider, {
    state, issue: errorCode, updated_at: new Date().toISOString()
  });
}
async function relayDiagnostics(force = false) {
  const current = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
  const identity = hasRelayConnection(current) ? current.gateway + "|" + relayIdentity(current) : null;
  if (identity !== lastGatewayIdentity) {
    lastGatewayCheck = null;
    lastGatewayIdentity = identity;
    providerDiagnostics.clear();
  }
  const extension = {
    connected: hasRelayConnection(current),
    linked_providers: [...(current.linkedProviders || [])].filter(provider => Boolean(PROVIDERS[provider])),
    provider_sync: Object.fromEntries(providerDiagnostics),
  };
  if (!extension.connected) return {ok:false, extension, gateway_issue:"not_connected"};
  if (!force && lastGatewayCheck && Date.now() - lastGatewayCheckAt < CONNECTION_DIAGNOSTICS_TTL_MS) {
    return {ok:true, extension, gateway:lastGatewayCheck};
  }
  try {
    const response = await relayFetch("/relay/v1/diagnostics", {method:"POST",body:"{}"});
    if (!response.ok) {
      return {ok:false, extension, gateway_issue:
        response.status === 401 || response.status === 403 ? "gateway_unauthorized" : "gateway_request_failed"};
    }
    const gateway = await response.json();
    lastGatewayCheck = gateway;
    lastGatewayCheckAt = Date.now();
    return {ok:true, extension, gateway};
  } catch (error) {
    return {ok:false, extension, gateway_issue:classifyRelayFailure(error)};
  }
}

// Serialize each provider's automatic imports with logout so a delayed cookie
// refresh cannot recreate a vault session after the user disconnects it.
const providerTasks = new Map();
function queueProviderTask(provider, work) {
  const prior = providerTasks.get(provider) || Promise.resolve();
  const pending = prior.catch(() => {}).then(work);
  providerTasks.set(provider, pending);
  pending.then(
    () => { if (providerTasks.get(provider) === pending) providerTasks.delete(provider); },
    () => { if (providerTasks.get(provider) === pending) providerTasks.delete(provider); }
  );
  return pending;
}
// Keep one trailing refresh per provider. It reads the latest cookie snapshot
// when it starts, rather than replaying every rotation behind a slow import.
const queuedProviderSyncs = new Map();
const queuedProviderForces = new Set();
function queueProviderSync(provider, force = false) {
  const queued = queuedProviderSyncs.get(provider);
  if (queued) {
    // Upgrade one pending automatic refresh to manual without queueing
    // duplicate calls that would repeatedly restart the provider page.
    if (force) queuedProviderForces.add(provider);
    return queued;
  }
  const pending = queueProviderTask(provider, () => {
    if (queuedProviderSyncs.get(provider) === pending) queuedProviderSyncs.delete(provider);
    const shouldForce = force || queuedProviderForces.has(provider);
    queuedProviderForces.delete(provider);
    return syncProvider(provider, {force:shouldForce}).then(
      result => {
        recordProviderDiagnostic(provider, result || "skipped");
        if (result === "uploaded") lastGatewayCheck = null;
        return result;
      },
      error => {
        recordProviderDiagnostic(provider, "failed", classifyRelayFailure(error));
        throw error;
      }
    );
  });
  queuedProviderSyncs.set(provider, pending);
  return pending;
}
function logoutProviderFromRelay(provider) {
  // Later sync requests belong after this logout barrier.
  queuedProviderSyncs.delete(provider);
  queuedProviderForces.delete(provider);
  // Bind logout to the install present when the user clicked. An earlier
  // provider sync may still be queued while a fresh gateway is paired.
  const connectionAtClick = chrome.storage.local.get(["gateway", "token", "connectionMode"]);
  return queueProviderTask(provider, async () => {
    const expected = await connectionAtClick;
    if (!hasRelayConnection(expected)) {
      throw new Error("Gateway connection missing; reconnect before provider logout.");
    }
    const current = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
    if (current.gateway !== expected.gateway || current.token !== expected.token || current.connectionMode !== expected.connectionMode) {
      throw new Error("Gateway connection changed before provider logout. Retry on the new connection.");
    }
    // Opt out before revocation; even if gateway deletion fails, do not
    // upload more cookies until the user explicitly reconnects this provider.
    const linkedProviders = current.linkedProviders || [];
    await chrome.storage.local.set({linkedProviders:linkedProviders.filter(value => value !== provider)});
    // A re-pair during the awaited storage write must never redirect the
    // DELETE request to a different gateway or a newly issued install token.
    const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {
      method:"DELETE",body:"{}"
    }, {gateway:expected.gateway,token:expected.token,connectionMode:expected.connectionMode});
    if (!response.ok) throw new Error("Gateway logout failed (" + response.status + "). Auto sync is paused; retry logout.");
    providerDiagnostics.delete(provider);
    lastGatewayCheck = null;
    const latest = await chrome.storage.local.get(["gateway", "token", "connectionMode", "relaySnapshotVersions"]);
    if (latest.gateway === expected.gateway && latest.token === expected.token && latest.connectionMode === expected.connectionMode) {
      const versions = {...(latest.relaySnapshotVersions || {})};
      delete versions[provider];
      await chrome.storage.local.set({relaySnapshotVersions:versions});
    }
  });
}

chrome.cookies.onChanged.addListener(({cookie}) => {
  const domains = Object.entries(PROVIDERS);
  const host = cookie.domain.replace(/^\./, "").toLowerCase();
  for (const [provider, roots] of domains) {
    if (!roots.some(root => host === root || host.endsWith("." + root))) continue;
    clearTimeout(debounceTimers.get(provider));
    const timer = setTimeout(() => {
      if (debounceTimers.get(provider) !== timer) return;
      debounceTimers.delete(provider);
      queueProviderSync(provider).catch(() => {
        // Never log cookie material or relay credentials. The popup can surface status on demand.
      });
    }, 10_000);
    debounceTimers.set(provider, timer);
  }
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "relay-diagnostics") {
    relayDiagnostics(message.force === true).then(sendResponse).catch(() => sendResponse({ok:false, gateway_issue:"diagnostics_unavailable"}));
    return true;
  }
  if (message?.type === "poll-connection") {
    pollConnection().then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
  if (message?.type === "sync-provider" && PROVIDERS[message.provider]) {
    queueProviderSync(message.provider, true).then(syncStatus => sendResponse({ok:true,syncStatus:syncStatus || "skipped"})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
  if (message?.type === "logout-provider" && PROVIDERS[message.provider]) {
    logoutProviderFromRelay(message.provider).then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
});


async function resyncLinkedProviders() {
  const {linkedProviders = []} = await chrome.storage.local.get(["linkedProviders"]);
  // Startup restoration is best-effort: providers remain independently usable
  // if one domain is unavailable, and no session material is written to logs.
  // Repair an unexpectedly missing vault snapshot even when Chrome cookie
  // contents are unchanged (for example after a gateway volume replacement).
  // Do not force heavy browser reloads merely because authentication is unknown.
  const status = await relayDiagnostics(true);
  const missing = new Set(status.ok
    ? (status.gateway.providers || []).filter(item => !item.linked).map(item => item.provider)
    : []);
  await Promise.allSettled(
    [...new Set(linkedProviders)].filter(provider => PROVIDERS[provider])
      .map(provider => queueProviderSync(provider, missing.has(provider)))
  );
}

function scheduleLinkedProviderResync() {
  chrome.alarms?.create?.("relay-session-refresh", {periodInMinutes:15});
  pollConnection().catch(() => {});
  resyncLinkedProviders().catch(() => {
    // Do not log provider session material or relay credentials.
  });
}

// MV3 service workers can be recreated independently of a browser startup.
// Restore linked sessions after installation/update as well as browser startup.
chrome.runtime.onInstalled.addListener(scheduleLinkedProviderResync);
chrome.runtime.onStartup.addListener(scheduleLinkedProviderResync);


// Pending approval survives popup closure and MV3 worker suspension. Alarms wake
// the worker; opening/focusing the popup also polls immediately.
let connectionPoll;
async function pollConnection() {
  if (connectionPoll) return connectionPoll;
  connectionPoll = completePendingConnection();
  try { await connectionPoll; } finally { connectionPoll = undefined; }
}
// A request can be replaced from the popup while this worker is awaiting the
// gateway. Never let an older poll clear a newer approval request or its alarm.
async function clearPendingApprovalIfCurrent(pending) {
  const {pendingConnection: current} = await chrome.storage.local.get(["pendingConnection"]);
  if (current?.request_id !== pending.request_id ||
      current?.gateway !== pending.gateway ||
      current?.poll_secret !== pending.poll_secret ||
      current?.expires !== pending.expires) return false;
  await chrome.storage.local.remove("pendingConnection");
  await chrome.alarms?.clear?.("relay-connection");
  return true;
}
async function completePendingConnection() {
  const {pendingConnection: pending} = await chrome.storage.local.get(["pendingConnection"]);
  if (!pending) return;
  if (Date.now() >= pending.expires) {
    await clearPendingApprovalIfCurrent(pending);
    throw new Error("Connection approval expired. Choose Connect to gateway again.");
  }
  const gateway = normalizeGateway(pending.gateway);
  if (!await chrome.permissions.contains({origins:[gateway + "/*"]})) throw new Error("Gateway permission is missing.");
  const response = await fetch(gateway + "/relay/v1/connect/poll", {
    method:"POST", headers:{"Content-Type":"application/json"},
    body:JSON.stringify({request_id:pending.request_id,poll_secret:pending.poll_secret}),
    cache:"no-store", redirect:"error"
  });
  if (response.status === 202) return;
  if (!response.ok) {
    if (response.status === 410) await clearPendingApprovalIfCurrent(pending);
    throw new Error("Connection approval unavailable. Connect again from the extension.");
  }
  const data = await response.json();
  if (!/^[a-f0-9]{64}$/.test(data.token)) throw new Error("Invalid connection token.");
  const latest = await chrome.storage.local.get(["pendingConnection","gateway","linkedProviders"]);
  const current = latest.pendingConnection;
  // A prior approval response is not valid for a renewed request, even if a
  // request ID is reused. Fence the successful exchange to the full secret
  // and expiry identity rather than just its public ID and origin.
  if (current?.request_id !== pending.request_id || current?.gateway !== gateway ||
      current?.poll_secret !== pending.poll_secret || current?.expires !== pending.expires) return;
  // A slow network response cannot extend the five-minute approval window.
  if (Date.now() >= current.expires) {
    await clearPendingApprovalIfCurrent(current);
    throw new Error("Connection approval expired. Choose Connect to gateway again.");
  }
  const linkedProviders = latest.gateway === gateway ? (latest.linkedProviders || []) : [];
  await chrome.storage.local.set({gateway, token:data.token, linkedProviders});
  await chrome.storage.local.remove("pendingConnection");
  await chrome.alarms?.clear?.("relay-connection");
  lastGatewayCheck = null;
  resyncLinkedProviders().catch(() => {});
}
chrome.alarms?.onAlarm?.addListener?.(alarm => {
  if (alarm.name === "relay-connection") pollConnection().catch(() => {});
  if (alarm.name === "relay-session-refresh") scheduleLinkedProviderResync();
});

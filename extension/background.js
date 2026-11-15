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
// Only the newest read-only gateway health request for the current identity
// can update the cached status or cookie-event outage backoff.
let gatewayProbeSerial = 0;
// Only throttle automatic cookie-event resync after a gateway health probe
// has actually failed; a manual Sync or scheduled liveness probe is never
// suppressed. A new gateway identity cannot inherit the old outage.
const COOKIE_OUTAGE_BACKOFF_MS = 60_000;
let automaticCookieSyncSuppressedUntil = 0;
function classifyRelayFailure(error) {
  const message = String(error?.message || "");
  if (message.includes("timed out") || message.includes("cancelled")) return "gateway_timeout";
  if (message.includes("permission") || message.includes("consent")) return "permission_or_consent";
  if (message.includes("(504)")) return "activation_timeout";
  if (message.includes("(403)") || message.includes("(401)")) return "gateway_unauthorized";
  return "relay_unavailable";
}
// Permission state is independent of relay connectivity or saved sessions.
// Return fixed booleans (or null when this browser cannot inspect permission),
// never cookie values, credential-bearing URLs or raw browser errors.
async function hasOriginPermissions(origins) {
  if (!chrome.permissions?.contains) return null;
  try { return Boolean(await chrome.permissions.contains({origins})); }
  catch { return false; }
}
function recordProviderDiagnostic(provider, state, errorCode = null) {
  // A provider sync attempt is NOT authenticated-login verification.
  // Store only stable local result codes, never raw upstream responses.
  const safeState = ["uploaded", "unchanged", "no_cookies", "skipped", "failed"].includes(state)
    ? state : "unknown";
  providerDiagnostics.set(provider, {
    state: safeState, issue: errorCode, updated_at: new Date().toISOString()
  });
}
// Every diagnostics response has independent per-provider attempt freshness.
// Failed gateway status makes old local sync snapshots stale, but never
// deletes the last attempt time or falsely changes a backend login state.
function finalizeRelayDiagnostics(result) {
  const extension = result.extension;
  if (!extension) return result;
  extension.provider_sync = Object.fromEntries(
    extension.linked_providers.map(provider => {
      const previous = providerDiagnostics.get(provider);
      return [provider, {
        state: previous?.state || "not_attempted",
        issue: previous?.issue || null,
        updated_at: previous?.updated_at || null,
        stale: !result.ok,
      }];
    })
  );
  return result;
}
async function relayDiagnostics(force = false) {
  const current = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
  const identity = hasRelayConnection(current) ? current.gateway + "|" + relayIdentity(current) : null;
  if (identity !== lastGatewayIdentity) {
    lastGatewayCheck = null;
    lastGatewayIdentity = identity;
    automaticCookieSyncSuppressedUntil = 0;
    providerDiagnostics.clear();
  }
  const extension = {
    connected: hasRelayConnection(current),
    connection_mode: !hasRelayConnection(current) ? "disconnected" : current.connectionMode === "local" ? "local" : "paired",
    linked_providers: [...new Set(current.linkedProviders || [])].filter(provider => Boolean(PROVIDERS[provider])),
    provider_sync: Object.fromEntries(providerDiagnostics),
    permissions: {gateway:null, providers:{}},
  };
  extension.permissions.providers = Object.fromEntries(await Promise.all(
    extension.linked_providers.map(async provider => {
      const origins = PROVIDERS[provider].flatMap(domain => [
        "https://" + domain + "/*", "https://*." + domain + "/*"
      ]);
      return [provider, await hasOriginPermissions(origins)];
    })
  ));
  if (!extension.connected) return finalizeRelayDiagnostics({ok:false, extension, gateway_issue:"not_connected"});
  extension.permissions.gateway = await hasOriginPermissions([current.gateway + "/*"]);
  if (!force && lastGatewayCheck && Date.now() - lastGatewayCheckAt < CONNECTION_DIAGNOSTICS_TTL_MS) {
    extension.gateway_checked_at = new Date(lastGatewayCheckAt).toISOString();
    return finalizeRelayDiagnostics({ok:true, extension, gateway:lastGatewayCheck});
  }
  const probeSerial = ++gatewayProbeSerial;
  const isCurrentProbe = async () => {
    try {
      const latest = await chrome.storage.local.get(["gateway", "token", "connectionMode"]);
      return probeSerial === gatewayProbeSerial && identity === lastGatewayIdentity &&
        latest.gateway === current.gateway && latest.token === current.token &&
        latest.connectionMode === current.connectionMode;
    } catch (_) {
      return false; // Storage failure cannot authorize a stale cache write.
    }
  };
  const changed = () => finalizeRelayDiagnostics({
    ok:false, extension, gateway_issue:"gateway_connection_changed"
  });
  try {
    // Check the identity before dispatching the status request, as well as
    // after it resolves. A reconnect must not reuse an old target/token.
    const response = await relayFetch(
      "/relay/v1/diagnostics", {method:"POST",body:"{}"}, current
    );
    if (!await isCurrentProbe()) return changed();
    if (!response.ok) {
      lastGatewayCheck = null;
      lastGatewayCheckAt = 0;
      automaticCookieSyncSuppressedUntil = Date.now() + COOKIE_OUTAGE_BACKOFF_MS;
      return finalizeRelayDiagnostics({ok:false, extension, gateway_issue:
        response.status === 401 || response.status === 403 ? "gateway_unauthorized" : "gateway_request_failed"});
    }
    const gateway = await response.json();
    if (!await isCurrentProbe()) return changed();
    lastGatewayCheck = gateway;
    lastGatewayCheckAt = Date.now();
    automaticCookieSyncSuppressedUntil = 0;
    extension.gateway_checked_at = new Date(lastGatewayCheckAt).toISOString();
    return finalizeRelayDiagnostics({ok:true, extension, gateway});
  } catch (error) {
    // In-flight failure from a prior connection must not mark the new one
    // offline or revoke its healthy cache/automatic cookie sync.
    if (!await isCurrentProbe()) return changed();
    lastGatewayCheck = null;
    lastGatewayCheckAt = 0;
    automaticCookieSyncSuppressedUntil = Date.now() + COOKIE_OUTAGE_BACKOFF_MS;
    return finalizeRelayDiagnostics({ok:false, extension, gateway_issue:classifyRelayFailure(error)});
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
// An in-flight cookie import can finish after an owner switches gateways.
// Scope only its local redacted diagnostics/cache writes to the connection
// captured when the task actually began. Sync/cookie transport already owns
// its separate permission and destination checks.
async function providerDiagnosticScopeCurrent(provider, attempt) {
  try {
    const latest = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs"]);
    return attempt.gateway === latest.gateway &&
      attempt.token === latest.token &&
      attempt.connectionMode === latest.connectionMode &&
      latest.linkedProviders?.includes(provider) &&
      // Legacy linked entries without epochs are allowed only until their
      // first explicit link/logout transition; that transition always writes
      // a fresh epoch, so an old in-flight result cannot pass this check.
      attempt.providerLinkEpochs?.[provider] === latest.providerLinkEpochs?.[provider];
  } catch (_) {
    return false; // Failed storage read cannot authorize a status mutation.
  }
}
function queueProviderSync(provider, force = false) {
  const queued = queuedProviderSyncs.get(provider);
  if (queued) {
    // Upgrade one pending automatic refresh to manual without queueing
    // duplicate calls that would repeatedly restart the provider page.
    if (force) queuedProviderForces.add(provider);
    return queued;
  }
  const pending = queueProviderTask(provider, async () => {
    if (queuedProviderSyncs.get(provider) === pending) queuedProviderSyncs.delete(provider);
    const shouldForce = force || queuedProviderForces.has(provider);
    queuedProviderForces.delete(provider);
    const attempt = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs"]);
    try {
      const result = await syncProvider(provider, {force:shouldForce});
      if (await providerDiagnosticScopeCurrent(provider, attempt)) {
        recordProviderDiagnostic(provider, result || "skipped");
        if (result === "uploaded") lastGatewayCheck = null;
      }
      return result;
    } catch (error) {
      if (await providerDiagnosticScopeCurrent(provider, attempt)) {
        recordProviderDiagnostic(provider, "failed", classifyRelayFailure(error));
      }
      throw error;
    }
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
    const current = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs"]);
    if (current.gateway !== expected.gateway || current.token !== expected.token || current.connectionMode !== expected.connectionMode) {
      throw new Error("Gateway connection changed before provider logout. Retry on the new connection.");
    }
    // Opt out before revocation; even if gateway deletion fails, do not
    // upload more cookies until the user explicitly reconnects this provider.
    const linkedProviders = current.linkedProviders || [];
    await chrome.storage.local.set({
      linkedProviders:linkedProviders.filter(value => value !== provider),
      providerLinkEpochs:{...(current.providerLinkEpochs || {}), [provider]:randomHex(16)}
    });
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
    const timer = setTimeout(async () => {
      if (debounceTimers.get(provider) !== timer) return;
      // A recent failed gateway health probe already establishes an outage.
      // Do not convert every cookie rotation into a doomed automatic import;
      // retain the next scheduled health check and explicit manual override.
      if (Date.now() < automaticCookieSyncSuppressedUntil) {
        debounceTimers.delete(provider);
        return;
      }
      // The cookie domain may belong to a provider the user explicitly
      // unlinked, or the gateway may have been disconnected during debounce.
      // Read current consent before scheduling a browser/vault mutation.
      let state;
      try {
        state = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
      } catch (_) {
        // Storage failures do not authorize a background session import.
        if (debounceTimers.get(provider) === timer) debounceTimers.delete(provider);
        return;
      }
      // Recheck after the await: another cookie event may have replaced this
      // generation while the storage request was pending.
      if (debounceTimers.get(provider) !== timer) return;
      debounceTimers.delete(provider);
      if (!hasRelayConnection(state) || !state.linkedProviders?.includes(provider) ||
          Date.now() < automaticCookieSyncSuppressedUntil) return;
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
  // If an installed gateway is currently unreachable or rejects the status
  // check, one periodic heartbeat is enough. Do not launch a burst of
  // automatic cookie reads/imports doomed to fail against the same endpoint.
  // Keep linked-provider consent intact so the next successful check repairs
  // missing snapshots normally. Manual provider Sync remains independent.
  // No configured gateway is not permission to enqueue every remembered
  // provider. Preserve linked consent for a later successful reconnect, but
  // never report synthetic skipped imports while disconnected.
  if (!status.ok) return;
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

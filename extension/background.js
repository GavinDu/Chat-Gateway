importScripts("shared.js");

const debounceTimers = new Map();
// Internal API snapshots contain only fixed health categories and timestamps.
// Never retain or return provider cookies, install credentials or raw errors.
const providerDiagnostics = new Map();
// Only worker-internal consent ownership; never expose epochs in diagnostics.
const providerDiagnosticEpochs = new Map();
const IMPORT_PHASES = ["permission", "cookie_read", "digest", "upload"];
const IMPORT_MAX_DURATION_MS = 120_000;
// Only fixed keys, finite bounded milliseconds and an allowlisted active phase
// may cross the worker-to-popup diagnostics boundary.
function redactedImportTimings(value) {
  const timings = {};
  for (const phase of IMPORT_PHASES) {
    const n = value?.[phase];
    if (typeof n === "number" && Number.isFinite(n)) {
      timings[phase] = Math.max(0, Math.min(IMPORT_MAX_DURATION_MS, Math.floor(n)));
    }
  }
  return timings;
}
let diagnosticIdentityGeneration = 0;
// Keep cache shorter than the popup check interval so token revocation is
// never masked by a still-valid dashboard snapshot on the next poll.
const CONNECTION_DIAGNOSTICS_TTL_MS = 5_000;
let lastGatewayCheck = null;
let lastGatewayCheckAt = 0;
let lastGatewayIdentity = null;
// Same-identity status reads share one in-flight request, including forced
// popup/automatic probes. Real connection changes always start a new probe.
let inFlightGatewayStatus = null;
// Only the newest read-only gateway health request for the current identity
// can update the cached status or cookie-event outage backoff.
let gatewayProbeSerial = 0;
// A provider sync may begin before the first popup diagnostics read. Give
// both paths the same gateway identity boundary, so that a first status
// read cannot erase an in-flight result. Never expose this identity.
function ensureGatewayDiagnosticIdentity(current) {
  const identity = hasRelayConnection(current) ? current.gateway + "|" + relayIdentity(current) : null;
  if (identity !== lastGatewayIdentity) {
    // Invalidate any response from an older install generation, including
    // A -> B -> A where the gateway URL returns to its previous value.
    diagnosticIdentityGeneration += 1;
    gatewayProbeSerial += 1;
    lastGatewayCheck = null;
    lastGatewayIdentity = identity;
    automaticCookieSyncSuppressedUntil = 0;
    providerDiagnostics.clear();
    providerDiagnosticEpochs.clear();
  }
  return identity;
}
// Only throttle automatic cookie-event resync after a gateway health probe
// has actually failed; a manual Sync or scheduled liveness probe is never
// suppressed. A new gateway identity cannot inherit the old outage.
const COOKIE_OUTAGE_BACKOFF_MS = 60_000;
let automaticCookieSyncSuppressedUntil = 0;
function classifyRelayFailure(error) {
  const message = String(error?.message || "");
  // Only the relay's own redacted cancellation category can claim an
  // owner-initiated abort. An unrelated upstream string is not evidence.
  if (message.startsWith("Gateway request cancelled;")) return "gateway_cancelled";
  if (message.includes("timed out")) return "gateway_timeout";
  if (message.includes("permission") || message.includes("consent")) return "permission_or_consent";
  if (message.includes("(504)")) return "activation_timeout";
  if (message.includes("(403)") || message.includes("(401)")) return "gateway_unauthorized";
  return "relay_unavailable";
}
// A manual sync result crosses the MV3 worker-to-popup trust boundary.
// Never forward raw upstream exceptions (which may include credential URLs,
// cookie names/values or gateway response bodies) into the UI message.
const MANUAL_SYNC_ERRORS = Object.freeze({
  gateway_cancelled: "Gateway sync was cancelled. Retry when ready.",
  gateway_timeout: "Gateway sync timed out. Check the connection and retry.",
  permission_or_consent: "Provider permission or consent is missing. Re-link and retry.",
  activation_timeout: "Provider activation timed out. Retry from the extension.",
  gateway_unauthorized: "Gateway connection was rejected. Reconnect and retry.",
  relay_unavailable: "Provider sync failed. Check the gateway connection and retry."
});
function redactedManualSyncFailure(error) {
  const errorCode = classifyRelayFailure(error);
  return {ok:false, errorCode, error:MANUAL_SYNC_ERRORS[errorCode]};
}
// Provider logout is not a website sign-out and may fail after local
// consent was already revoked. Never reflect raw worker/upstream exceptions
// (including response URLs, session identifiers or cookies) into popup text.
const MANUAL_LOGOUT_ERRORS = Object.freeze({
  connection_changed: "Gateway connection changed before logout. Reconnect and retry.",
  gateway_cancelled: "Gateway logout was cancelled. Check status and retry.",
  gateway_timeout: "Gateway logout timed out. Check status and retry.",
  permission_or_consent: "Gateway or provider permission changed. Reconnect and retry.",
  activation_timeout: "Gateway did not finish logout in time. Check status and retry.",
  gateway_unauthorized: "Gateway rejected logout. Reconnect and retry.",
  relay_unavailable: "Gateway logout failed. Check the connection and retry."
});
function redactedManualLogoutFailure(error) {
  // The specific source-authored connection-change failure is distinguishable
  // from generic networking failures without forwarding any exception text.
  const sourceMessage = String(error?.message || "");
  const errorCode = sourceMessage.startsWith("Gateway connection changed before provider logout.")
    ? "connection_changed" : classifyRelayFailure(error);
  return {ok:false, errorCode, error:MANUAL_LOGOUT_ERRORS[errorCode]};
}
// Permission state is independent of relay connectivity or saved sessions.
// Return fixed booleans (or null when this browser cannot inspect permission),
// never cookie values, credential-bearing URLs or raw browser errors.
async function hasOriginPermissions(origins) {
  if (!chrome.permissions?.contains) return null;
  try { return Boolean(await chrome.permissions.contains({origins})); }
  catch { return false; }
}
function recordProviderDiagnostic(provider, state, errorCode = null, attempt = null, telemetry = null) {
  // A provider sync attempt is NOT authenticated-login verification.
  // Store only stable local result codes, never raw upstream responses.
  const safeState = ["syncing", "uploaded", "saved_activation_pending", "unchanged", "no_cookies", "skipped", "failed"].includes(state)
    ? state : "unknown";
  const entry = {
    state: safeState, issue: errorCode, updated_at: new Date().toISOString(),
    phase: safeState === "syncing" && IMPORT_PHASES.includes(telemetry?.phase)
      ? telemetry.phase : null,
    durations_ms: redactedImportTimings(telemetry?.durations_ms)
  };
  providerDiagnostics.set(provider, entry);
  if (attempt) providerDiagnosticEpochs.set(provider, attempt.providerLinkEpochs?.[provider]);
  return entry;
}
// Every diagnostics response has independent per-provider attempt freshness.
// Failed gateway status makes old local sync snapshots stale, but never
// deletes the last attempt time or falsely changes a backend login state.
function finalizeRelayDiagnostics(result, requestedGeneration, requestedEpochs) {
  const extension = result.extension;
  if (!extension) return result;
  extension.provider_sync = Object.fromEntries(
    extension.linked_providers.map(provider => {
      // A delayed popup query must not borrow another gateway's or newly
      // relinked provider's live progress. The status record's epoch is private.
      const previous = requestedGeneration === diagnosticIdentityGeneration &&
        providerDiagnosticEpochs.get(provider) === requestedEpochs?.[provider]
        ? providerDiagnostics.get(provider) : null;
      return [provider, {
        state: previous?.state || "not_attempted",
        issue: previous?.issue || null,
        updated_at: previous?.updated_at || null,
        phase: previous?.phase || null,
        durations_ms: redactedImportTimings(previous?.durations_ms),
        stale: !result.ok,
      }];
    })
  );
  return result;
}
async function relayDiagnostics(force = false) {
  const current = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs"]);
  const identity = ensureGatewayDiagnosticIdentity(current);
  const requestedGeneration = diagnosticIdentityGeneration;
  // A same-gateway provider unlink/relink changes consent even when the
  // connection identity itself has not changed.
  for (const provider of providerDiagnostics.keys()) {
    if (providerDiagnosticEpochs.get(provider) !== current.providerLinkEpochs?.[provider]) {
      providerDiagnostics.delete(provider);
      providerDiagnosticEpochs.delete(provider);
    }
  }
  const extension = {
    connected: hasRelayConnection(current),
    connection_mode: !hasRelayConnection(current) ? "disconnected" : current.connectionMode === "local" ? "local" : "paired",
    linked_providers: [...new Set(Array.isArray(current.linkedProviders) ? current.linkedProviders : [])].filter(
      provider => typeof provider === "string" && Object.hasOwn(PROVIDERS, provider)
    ),
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
  if (!extension.connected) return finalizeRelayDiagnostics({ok:false, extension, gateway_issue:"not_connected"}, requestedGeneration, current.providerLinkEpochs);
  extension.permissions.gateway = await hasOriginPermissions([current.gateway + "/*"]);
  // The health probe is read-only. Its network result may be shared for the
  // same identity, but each caller must retain its own linked-provider and
  // permission snapshot, never reuse another popup's consent metadata.
  if (inFlightGatewayStatus?.identity === identity) {
    const shared = await inFlightGatewayStatus.promise;
    if (shared.extension?.gateway_checked_at) {
      extension.gateway_checked_at = shared.extension.gateway_checked_at;
    }
    return finalizeRelayDiagnostics({
      ok: shared.ok, extension, gateway: shared.gateway,
      gateway_issue: shared.gateway_issue
    }, requestedGeneration, current.providerLinkEpochs);
  }
  if (!force && lastGatewayCheck && Date.now() - lastGatewayCheckAt < CONNECTION_DIAGNOSTICS_TTL_MS) {
    extension.gateway_checked_at = new Date(lastGatewayCheckAt).toISOString();
    return finalizeRelayDiagnostics({ok:true, extension, gateway:lastGatewayCheck}, requestedGeneration, current.providerLinkEpochs);
  }
  const probe = (async () => {
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
  }, requestedGeneration, current.providerLinkEpochs);
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
        response.status === 401 || response.status === 403 ? "gateway_unauthorized" : "gateway_request_failed"}, requestedGeneration, current.providerLinkEpochs);
    }
    const gateway = await response.json();
    if (!await isCurrentProbe()) return changed();
    lastGatewayCheck = gateway;
    lastGatewayCheckAt = Date.now();
    automaticCookieSyncSuppressedUntil = 0;
    extension.gateway_checked_at = new Date(lastGatewayCheckAt).toISOString();
    return finalizeRelayDiagnostics({ok:true, extension, gateway}, requestedGeneration, current.providerLinkEpochs);
  } catch (error) {
    // In-flight failure from a prior connection must not mark the new one
    // offline or revoke its healthy cache/automatic cookie sync.
    if (!await isCurrentProbe()) return changed();
    lastGatewayCheck = null;
    lastGatewayCheckAt = 0;
    automaticCookieSyncSuppressedUntil = Date.now() + COOKIE_OUTAGE_BACKOFF_MS;
    return finalizeRelayDiagnostics({ok:false, extension, gateway_issue:classifyRelayFailure(error)}, requestedGeneration, current.providerLinkEpochs);
  }
  })();
  inFlightGatewayStatus = {identity, promise:probe};
  try { return await probe; }
  finally {
    // A stale completion must never release a newer identity's lease.
    if (inFlightGatewayStatus?.promise === probe) inFlightGatewayStatus = null;
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
// Chrome storage.local has no compare-and-swap. All explicit provider
// link/unlink/rollback writes to the shared linkedProviders array and epoch
// map are serialized through this single worker-owned queue. The per-provider
// task queue separately orders logout after a pending cookie upload.
let lastConsentMutation = Promise.resolve();
function queueConsentMutation(work) {
  const next = lastConsentMutation.catch(() => {}).then(work);
  lastConsentMutation = next.catch(() => {});
  return next;
}
function sameRelayIdentity(expected, actual) {
  return expected?.gateway === actual.gateway &&
    expected?.token === actual.token &&
    expected?.connectionMode === actual.connectionMode;
}
// A popup can close or fail after creating a first-time consent. Persist a
// provisional owner until its upload commits or that exact attempt rolls back.
// A second explicit link adopts a fresh epoch, so an old failed popup can
// never revoke a later successful opt-in, even on the same gateway.
function validLinkAttempt(attemptId) {
  return typeof attemptId === "string" && attemptId.length >= 8 && attemptId.length <= 128;
}
function linkProviderConsent(provider, expected, attemptId) {
  return queueProviderTask(provider, () => queueConsentMutation(async () => {
    if (!validLinkAttempt(attemptId)) throw new Error("Invalid provider link attempt.");
    const current = await chrome.storage.local.get(
      ["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs", "providerLinkPendingAttempts"]);
    if (!hasRelayConnection(current) || !sameRelayIdentity(expected, current)) {
      throw new Error("Gateway connection changed before provider consent was saved.");
    }
    // Malformed persisted storage must not count as linked consent.
    // An explicit new link is still a fresh owner action, never implicit.
    const linked = Array.isArray(current.linkedProviders) ? current.linkedProviders : [];
    const epochs = current.providerLinkEpochs || {};
    const pending = current.providerLinkPendingAttempts || {};
    const alreadyLinked = linked.includes(provider);
    const sameAttempt = pending[provider] === attemptId;
    const tentative = !alreadyLinked || Boolean(pending[provider]);
    const newGeneration = !alreadyLinked || (Boolean(pending[provider]) && !sameAttempt);
    const epoch = newGeneration || !epochs[provider] ? randomHex(16) : epochs[provider];
    await chrome.storage.local.set({
      linkedProviders:Array.from(new Set([...linked, provider])),
      providerLinkEpochs:{...epochs, [provider]:epoch},
      providerLinkPendingAttempts: tentative
        ? {...pending, [provider]:attemptId} : pending
    });
    return {alreadyLinked:alreadyLinked && !tentative, tentative};
  }));
}
function finishProviderLink(provider, expected, attemptId, rollback) {
  return queueProviderTask(provider, () => queueConsentMutation(async () => {
    if (!validLinkAttempt(attemptId)) return false;
    const current = await chrome.storage.local.get(
      ["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs", "providerLinkPendingAttempts"]);
    if (!sameRelayIdentity(expected, current) ||
        current.providerLinkPendingAttempts?.[provider] !== attemptId ||
        (!Array.isArray(current.linkedProviders) || !current.linkedProviders.includes(provider))) return false;
    const remaining = {...current.providerLinkPendingAttempts};
    delete remaining[provider];
    if (rollback) {
      await chrome.storage.local.set({
        linkedProviders:current.linkedProviders.filter(p => p !== provider),
        providerLinkEpochs:{...(current.providerLinkEpochs || {}), [provider]:randomHex(16)},
        providerLinkPendingAttempts:remaining
      });
    } else {
      await chrome.storage.local.set({providerLinkPendingAttempts:remaining});
    }
    return true;
  }));
}
// The local approval request crosses a popup HTTP await. Reserve its one-time
// generation in the MV3 worker before network I/O, then commit only if no
// owner-controlled connection transition occurred while the popup was away.
// A persisted reservation also survives worker suspension; it expires after
// two minutes if the popup closes before receiving the gateway response.
function reservePendingApproval(gateway) {
  return queueConsentMutation(async () => {
    if (!isLocalGateway(gateway) || normalizeGateway(gateway) !== gateway) {
      throw new Error("Only a normalized loopback gateway supports local approval.");
    }
    if (!await chrome.permissions.contains({origins:[gateway + "/*"]})) {
      throw new Error("Gateway host permission is missing.");
    }
    const current = await chrome.storage.local.get([
      "gateway", "token", "connectionMode", "relayConnectionEpoch",
      "localDisconnected", "pendingConnection", "pendingApprovalRequest"
    ]);
    if (hasRelayConnection(current)) throw new Error("Gateway is already connected.");
    if (current.pendingConnection && Date.now() < current.pendingConnection.expires) {
      throw new Error("Finish the existing approval before requesting another.");
    }
    if (current.pendingApprovalRequest && Date.now() < current.pendingApprovalRequest.expires) {
      throw new Error("Another local approval request is already in progress.");
    }
    const generation = randomHex(16);
    await chrome.storage.local.set({pendingApprovalRequest:{
      gateway, generation, expires:Date.now() + 120_000,
      installedGateway:current.gateway || null,
      connectionEpoch:current.relayConnectionEpoch || null,
      connectionMode:current.connectionMode || null,
      localDisconnected:Boolean(current.localDisconnected)
    }});
    return generation;
  });
}
function samePendingApprovalReservation(reservation, current, gateway, generation) {
  return reservation?.gateway === gateway && reservation.generation === generation &&
    Date.now() < reservation.expires &&
    (current.gateway || null) === reservation.installedGateway &&
    (current.relayConnectionEpoch || null) === reservation.connectionEpoch &&
    (current.connectionMode || null) === reservation.connectionMode &&
    Boolean(current.localDisconnected) === reservation.localDisconnected &&
    !hasRelayConnection(current);
}
function installPendingApproval(gateway, generation, requestId, pollSecret) {
  return queueConsentMutation(async () => {
    if (!/^[a-f0-9]{64}$/.test(requestId) || !/^[a-f0-9]{64}$/.test(pollSecret) ||
        typeof generation !== "string" || !/^[a-f0-9]{32}$/.test(generation)) {
      throw new Error("Invalid local approval response.");
    }
    const current = await chrome.storage.local.get([
      "gateway", "token", "connectionMode", "relayConnectionEpoch",
      "localDisconnected", "pendingApprovalRequest", "pendingConnection"
    ]);
    const reservation = current.pendingApprovalRequest;
    if (!samePendingApprovalReservation(reservation, current, gateway, generation) ||
        (current.pendingConnection && Date.now() < current.pendingConnection.expires)) {
      throw new Error("Local approval identity changed while requesting approval.");
    }
    const pending = {
      gateway, request_id:requestId, poll_secret:pollSecret,
      expires:Date.now() + 300_000,
      approvalGeneration:generation, connectionEpoch:reservation.connectionEpoch
    };
    await chrome.storage.local.set({pendingConnection:pending, pendingApprovalRequest:null});
    return {gateway, request_id:requestId, expires:pending.expires};
  });
}
function releasePendingApproval(generation) {
  return queueConsentMutation(async () => {
    const {pendingApprovalRequest:reservation} = await chrome.storage.local.get(["pendingApprovalRequest"]);
    if (!reservation || reservation.generation !== generation) return false;
    await chrome.storage.local.set({pendingApprovalRequest:null});
    return true;
  });
}
// Local loopback connection transitions must use the same writer as consent
// updates. Otherwise a delayed link's storage.set can resurrect consent after
// the owner disconnects, or stale auto-connect can undo explicit opt-out.
function connectLocalRelay(gateway, manual) {
  return queueConsentMutation(async () => {
    if (!isLocalGateway(gateway)) {
      throw new Error("Only a loopback gateway supports local no-pairing mode.");
    }
    const current = await chrome.storage.local.get(
      ["gateway", "token", "connectionMode", "linkedProviders", "localDisconnected"]);
    if (current.localDisconnected && !manual) {
      throw new Error("Local gateway was explicitly disconnected.");
    }
    if (current.gateway === gateway && current.connectionMode === "local" && !current.token) {
      return {unchanged:true};
    }
    const linkedProviders = current.gateway === gateway && Array.isArray(current.linkedProviders)
      ? current.linkedProviders : [];
    // Write the complete disconnected/connected view at once, then remove
    // compatibility keys. No intervening provider consent mutation can run.
    await chrome.storage.local.set({
      gateway, connectionMode:"local", token:null, pendingConnection:null,
      pendingApprovalRequest:null, relayConnectionEpoch:randomHex(16),
      relaySnapshotVersions:{}, linkedProviders, localDisconnected:false
    });
    await chrome.storage.local.remove(["token", "pendingConnection", "relaySnapshotVersions"]);
    lastGatewayCheck = null;
    return {unchanged:false};
  });
}
function disconnectLocalRelay(expected) {
  return queueConsentMutation(async () => {
    const current = await chrome.storage.local.get(
      ["gateway", "token", "connectionMode"]);
    if (current.connectionMode !== "local" || !isLocalGateway(current.gateway) ||
        !sameRelayIdentity(expected, current)) {
      throw new Error("Local gateway connection changed before disconnect.");
    }
    await chrome.storage.local.set({
      connectionMode:null, token:null, linkedProviders:[],
      relayConnectionEpoch:randomHex(16), pendingApprovalRequest:null,
      relaySnapshotVersions:{}, pendingConnection:null, localDisconnected:true
    });
    await chrome.storage.local.remove(
      ["connectionMode", "token", "linkedProviders", "relaySnapshotVersions", "pendingConnection"]);
    lastGatewayCheck = null;
    return true;
  });
}
// Legacy HTTPS Pair results are committed by the same owner-controlled
// writer as consent, pending approval and Unpair. An older network callback
// must not resurrect a revoked install after the user disconnects.
function connectPairedRelay(gateway, token, expected) {
  return queueConsentMutation(async () => {
    if (typeof gateway !== "string" || !gateway.startsWith("https://") ||
        normalizeGateway(gateway) !== gateway || !/^[a-f0-9]{64}$/.test(token)) {
      throw new Error("Invalid paired gateway target or token.");
    }
    if (!await chrome.permissions.contains({origins:[gateway + "/*"]})) {
      throw new Error("Paired gateway host permission missing.");
    }
    const current = await chrome.storage.local.get([
      "gateway", "token", "connectionMode", "relayConnectionEpoch",
      "localDisconnected", "linkedProviders", "providerLinkEpochs"
    ]);
    if (!expected || !sameRelayIdentity(expected, current) ||
        (expected.relayConnectionEpoch || null) !== (current.relayConnectionEpoch || null) ||
        Boolean(expected.localDisconnected) !== Boolean(current.localDisconnected)) {
      throw new Error("Paired connection changed while pairing.");
    }
    const epochs = {...(current.providerLinkEpochs || {})};
    for (const provider of new Set([
      ...(Array.isArray(current.linkedProviders) ? current.linkedProviders : []), ...Object.keys(epochs)
    ])) {
      if (Object.hasOwn(PROVIDERS, provider)) epochs[provider] = randomHex(16);
    }
    // One storage write makes a complete new connection snapshot. This
    // invalidates both old provider consents and any older pending approval.
    await chrome.storage.local.set({
      gateway, token, connectionMode:"paired",
      relayConnectionEpoch:randomHex(16),
      linkedProviders:[], providerLinkEpochs:epochs,
      providerLinkPendingAttempts:{}, relaySnapshotVersions:{},
      pendingApprovalRequest:null, pendingConnection:null, localDisconnected:false
    });
    lastGatewayCheck = null;
    providerDiagnostics.clear();
    return true;
  });
}

// Explicit remote Unpair is a connection transition, not a popup-side
// storage cleanup. Its old HTTPS response must not clear a new connection
// or race a worker-owned provider link/approval commit.
function disconnectPairedRelay(expected) {
  return queueConsentMutation(async () => {
    const current = await chrome.storage.local.get([
      "gateway", "token", "connectionMode", "linkedProviders",
      "providerLinkEpochs", "providerLinkPendingAttempts", "pendingConnection"
    ]);
    const gateway = expected?.gateway;
    if (typeof gateway !== "string" || !gateway.startsWith("https://") ||
        normalizeGateway(gateway) !== gateway || !current.token ||
        current.connectionMode === "local" || !sameRelayIdentity(expected, current)) {
      throw new Error("Paired gateway connection changed before disconnect.");
    }
    const epochs = {...(current.providerLinkEpochs || {})};
    for (const provider of new Set([
      ...(Array.isArray(current.linkedProviders) ? current.linkedProviders : []), ...Object.keys(epochs)
    ])) {
      if (Object.hasOwn(PROVIDERS, provider)) epochs[provider] = randomHex(16);
    }
    // A newer pending approval belongs to a different owner attempt. Clear
    // only the exact request visible when the user initiated Unpair.
    const pending = current.pendingConnection;
    const oldPending = expected.pendingRequest;
    const clearPending = Boolean(pending && oldPending &&
      pending.gateway === gateway && pending.request_id === oldPending.request_id &&
      pending.expires === oldPending.expires);
    const patch = {
      connectionMode:null, token:null, linkedProviders:[],
      relayConnectionEpoch:randomHex(16),
      providerLinkEpochs:epochs, providerLinkPendingAttempts:{},
      pendingApprovalRequest:null, relaySnapshotVersions:{}
    };
    if (clearPending) patch.pendingConnection = null;
    // A single write publishes a fully disconnected view atomically.
    // Null tokens and pending approvals are treated as absent by callers.
    await chrome.storage.local.set(patch);
    lastGatewayCheck = null;
    providerDiagnostics.clear();
    return {clearedPending:clearPending};
  });
}

// A 401/403 from the *gateway diagnostics route* invalidates only the
// install identity that was actually probed. A delayed popup status response
// must never revoke a newer pairing, nor mutate local no-pairing consent.
function invalidatePairedRelay(expected) {
  return queueConsentMutation(async () => {
    const current = await chrome.storage.local.get([
      "gateway", "token", "connectionMode", "relayConnectionEpoch",
      "linkedProviders", "providerLinkEpochs", "providerLinkPendingAttempts"
    ]);
    // Pre-no-pairing installations may still hold an owner-approved token
    // for an HTTP *loopback* gateway. Revoke that legacy paired identity too,
    // but never confuse tokenless local mode with a paired installation.
    const legacyLoopbackPair = current.gateway &&
      isLocalGateway(current.gateway) && Boolean(current.token) &&
      current.connectionMode !== "local";
    const remotePair = typeof current.gateway === "string" &&
      current.gateway.startsWith("https://") && Boolean(current.token);
    let validOrigin = false;
    try { validOrigin = normalizeGateway(current.gateway) === current.gateway; }
    catch (_) {}
    if (!expected || !validOrigin || (!remotePair && !legacyLoopbackPair) ||
        current.connectionMode === "local" ||
        !sameRelayIdentity(expected, current) ||
        (expected.relayConnectionEpoch || null) !== (current.relayConnectionEpoch || null)) {
      return false;
    }
    const epochs = {...(current.providerLinkEpochs || {})};
    for (const provider of new Set([
      ...(Array.isArray(current.linkedProviders) ? current.linkedProviders : []), ...Object.keys(epochs)
    ])) {
      if (Object.hasOwn(PROVIDERS, provider)) epochs[provider] = randomHex(16);
    }
    // Preserve newer pending approval. A gateway status probe was not an
    // explicit cancellation of a separate pending owner authorization.
    await chrome.storage.local.set({
      token:null, connectionMode:null, relayConnectionEpoch:randomHex(16),
      linkedProviders:[], providerLinkEpochs:epochs,
      providerLinkPendingAttempts:{}, pendingApprovalRequest:null,
      relaySnapshotVersions:{}
    });
    lastGatewayCheck = null;
    providerDiagnostics.clear();
    return true;
  });
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
      Array.isArray(latest.linkedProviders) && latest.linkedProviders.includes(provider) &&
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
    // A queued sync is not active until its serialized provider task starts.
    // Expose only a fixed redacted category for the *current* owner; never
    // mistake this for authenticated login or leak cookie/progress payloads.
    let inProgress = null;
    if (await providerDiagnosticScopeCurrent(provider, attempt)) {
      ensureGatewayDiagnosticIdentity(attempt);
      inProgress = recordProviderDiagnostic(provider, "syncing", null, attempt);
    }
    // A delayed result can follow unlink/relink or an owner change. Remove
    // only this attempt's in-progress record; never erase a newer attempt.
    const clearStaleProgress = () => {
      if (inProgress && providerDiagnostics.get(provider) === inProgress) {
        providerDiagnostics.delete(provider);
        providerDiagnosticEpochs.delete(provider);
      }
    };
    try {
      const onProgress = sample => {
        if (!inProgress || providerDiagnostics.get(provider) !== inProgress ||
            providerDiagnosticEpochs.get(provider) !== attempt.providerLinkEpochs?.[provider]) return;
        // The observer only accepts fixed stage categories and bounded
        // durations. Never forward snapshots, secrets or URLs to diagnostics.
        inProgress.phase = IMPORT_PHASES.includes(sample?.phase) ? sample.phase : null;
        inProgress.durations_ms = redactedImportTimings(sample?.durations_ms);
      };
      const result = await syncProvider(provider, {
        force:shouldForce, onProgress,
        snapshotVersionMutation:queueConsentMutation
      });
      if (await providerDiagnosticScopeCurrent(provider, attempt)) {
        recordProviderDiagnostic(provider, result || "skipped",
          result === "saved_activation_pending" ? "activation_pending" : null,
          attempt, inProgress);
        if (result === "uploaded" || result === "saved_activation_pending") lastGatewayCheck = null;
      } else {
        clearStaleProgress();
      }
      return result;
    } catch (error) {
      if (await providerDiagnosticScopeCurrent(provider, attempt)) {
        recordProviderDiagnostic(provider, "failed", classifyRelayFailure(error), attempt, inProgress);
      } else {
        clearStaleProgress();
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
    // Take the provider off the consent list BEFORE the network DELETE.
    // Other provider links may proceed while network revocation awaits.
    // Their array/epoch writes must not race the logout mutation.
    let revokedEpoch;
    await queueConsentMutation(async () => {
      const current = await chrome.storage.local.get(
        ["gateway", "token", "connectionMode", "linkedProviders", "providerLinkEpochs", "providerLinkPendingAttempts"]);
      if (!sameRelayIdentity(expected, current)) {
        throw new Error("Gateway connection changed before provider logout. Retry on the new connection.");
      }
      revokedEpoch = randomHex(16);
      await chrome.storage.local.set({
        linkedProviders:(Array.isArray(current.linkedProviders) ? current.linkedProviders : [])
          .filter(value => value !== provider),
        providerLinkEpochs:{...(current.providerLinkEpochs || {}), [provider]:revokedEpoch},
        providerLinkPendingAttempts: Object.fromEntries(
          Object.entries(current.providerLinkPendingAttempts || {}).filter(([name]) => name !== provider)
        )
      });
    });
    // A re-pair during the awaited storage write must never redirect the
    // DELETE request to a different gateway or a newly issued install token.
    const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {
      method:"DELETE",body:"{}"
    }, {gateway:expected.gateway,token:expected.token,connectionMode:expected.connectionMode});
    if (!response.ok) throw new Error("Gateway logout failed (" + response.status + "). Auto sync is paused; retry logout.");
    // A concurrent provider upload also writes this shared map. Serialize
    // the cleanup with every link/logout/snapshot writer, and do not delete a
    // newly relinked provider's digest or its fresh diagnostic record.
    await queueConsentMutation(async () => {
      const latest = await chrome.storage.local.get([
        "gateway", "token", "connectionMode", "linkedProviders",
        "providerLinkEpochs", "relaySnapshotVersions"
      ]);
      if (!sameRelayIdentity(expected, latest) ||
          !Array.isArray(latest.linkedProviders) || latest.linkedProviders.includes(provider) ||
          latest.providerLinkEpochs?.[provider] !== revokedEpoch) return;
      const versions = {...(latest.relaySnapshotVersions || {})};
      delete versions[provider];
      await chrome.storage.local.set({relaySnapshotVersions:versions});
      providerDiagnostics.delete(provider);
      lastGatewayCheck = null;
    });
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
      if (!hasRelayConnection(state) || !Array.isArray(state.linkedProviders) || !state.linkedProviders.includes(provider) ||
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
  if (message?.type === "reserve-pending-approval") {
    reservePendingApproval(message.gateway)
      .then(generation => sendResponse({ok:true, generation}))
      .catch(() => sendResponse({ok:false,error:"Local approval reservation unavailable."}));
    return true;
  }
  if (message?.type === "install-pending-approval") {
    installPendingApproval(message.gateway, message.generation, message.request_id, message.poll_secret)
      .then(pending => sendResponse({ok:true,pending}))
      .catch(() => sendResponse({ok:false,error:"Local approval changed or expired before it was saved."}));
    return true;
  }
  if (message?.type === "release-pending-approval") {
    releasePendingApproval(message.generation)
      .then(released => sendResponse({ok:true,released}))
      .catch(() => sendResponse({ok:false,error:"Local approval cleanup unavailable."}));
    return true;
  }
  if (message?.type === "poll-connection") {
    pollConnection()
      .then(() => sendResponse({ok:true}))
      // A pending approval can involve browser storage and network exceptions.
      // Do not reflect credential-bearing raw errors into the popup.
      .catch(() => sendResponse({
        ok:false, errorCode:"connection_poll_unavailable",
        error:"Connection approval check unavailable. Retry or reconnect."
      }));
    return true;
  }
  if (message?.type === "connect-local-relay") {
    connectLocalRelay(message.gateway, message.manual === true)
      .then(({unchanged}) => sendResponse({ok:true, unchanged}))
      .catch(() => sendResponse({ok:false, error:"Local gateway connection changed or was disconnected."}));
    return true;
  }
  if (message?.type === "disconnect-local-relay") {
    disconnectLocalRelay(message.connection)
      .then(() => sendResponse({ok:true}))
      .catch(() => sendResponse({ok:false, error:"Local gateway connection changed before disconnect."}));
    return true;
  }
  if (message?.type === "invalidate-paired-relay") {
    invalidatePairedRelay(message.connection)
      .then(invalidated => sendResponse({ok:true, invalidated}))
      .catch(() => sendResponse({ok:false, error:"Paired gateway status invalidation unavailable."}));
    return true;
  }
  if (message?.type === "connect-paired-relay") {
    connectPairedRelay(message.gateway, message.token, message.expected)
      .then(() => sendResponse({ok:true}))
      .catch(() => sendResponse({ok:false, error:"Paired gateway connection changed or was rejected."}));
    return true;
  }
  if (message?.type === "disconnect-paired-relay") {
    disconnectPairedRelay(message.connection)
      .then(() => sendResponse({ok:true}))
      .catch(() => sendResponse({ok:false, error:"Paired gateway connection changed before disconnect."}));
    return true;
  }
  if (message?.type === "link-provider" && typeof message.provider === "string" && Object.hasOwn(PROVIDERS, message.provider)) {
    linkProviderConsent(message.provider, message.connection, message.attemptId)
      .then(({alreadyLinked, tentative}) => sendResponse({ok:true, alreadyLinked, tentative}))
      .catch(() => sendResponse({ok:false, error:"Gateway connection or link attempt invalid."}));
    return true;
  }
  if (message?.type === "commit-provider-link" && typeof message.provider === "string" && Object.hasOwn(PROVIDERS, message.provider)) {
    finishProviderLink(message.provider, message.connection, message.attemptId, false)
      .then(committed => sendResponse({ok:true, committed}))
      .catch(() => sendResponse({ok:false, error:"Provider link commit unavailable."}));
    return true;
  }
  if (message?.type === "rollback-provider-link" && typeof message.provider === "string" && Object.hasOwn(PROVIDERS, message.provider)) {
    finishProviderLink(message.provider, message.connection, message.attemptId, true)
      .then(rolledBack => sendResponse({ok:true, rolledBack}))
      .catch(() => sendResponse({ok:false, error:"Provider link rollback unavailable."}));
    return true;
  }
  if (message?.type === "sync-provider" && typeof message.provider === "string" && Object.hasOwn(PROVIDERS, message.provider)) {
    queueProviderSync(message.provider, true)
      .then(syncStatus => sendResponse({ok:true,syncStatus:syncStatus || "skipped"}))
      .catch(error => sendResponse(redactedManualSyncFailure(error)));
    return true;
  }
  if (message?.type === "logout-provider" && typeof message.provider === "string" && Object.hasOwn(PROVIDERS, message.provider)) {
    logoutProviderFromRelay(message.provider)
      .then(() => sendResponse({ok:true}))
      .catch(error => sendResponse(redactedManualLogoutFailure(error)));
    return true;
  }
});


async function resyncLinkedProviders() {
  // An owner can revoke consent or replace the gateway while an awaited
  // diagnostics request is in flight. Do not schedule imports from the
  // linked-provider list captured before that await.
  const initial = await chrome.storage.local.get(["gateway", "token", "connectionMode"]);
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
  let latest;
  try {
    latest = await chrome.storage.local.get(["gateway", "token", "connectionMode", "linkedProviders"]);
  } catch (_) {
    // A failed storage read cannot authorize automatic cookie collection.
    return;
  }
  if (!hasRelayConnection(latest) || !sameRelayIdentity(initial, latest)) return;
  const missing = new Set((status.gateway.providers || [])
    .filter(item => !item.linked).map(item => item.provider));
  await Promise.allSettled(
    [...new Set(Array.isArray(latest.linkedProviders) ? latest.linkedProviders : [])]
      .filter(provider => typeof provider === "string" && Object.hasOwn(PROVIDERS, provider))
      .map(provider => queueProviderSync(provider, missing.has(provider)))
  );
}

// Startup, installation and periodic alarms may overlap while a gateway
// diagnostics request is still in flight. Coalesce only these automatic
// refreshes. Explicit manual Sync and post-approval resync remain independent
// so a changed connection cannot be hidden behind an old scheduled request.
let scheduledProviderRefresh = null;
function scheduleLinkedProviderResync() {
  // Register a repeating alarm at install/startup, not on every alarm
  // firing. Chrome already reschedules alarms with periodInMinutes.
  chrome.alarms?.create?.("relay-session-refresh", {periodInMinutes:15});
  refreshLinkedProviders();
}
function refreshLinkedProviders() {
  pollConnection().catch(() => {});
  if (scheduledProviderRefresh) return;
  const run = resyncLinkedProviders();
  scheduledProviderRefresh = run;
  // Both success and failure release the lease. Attaching a rejection handler
  // also prevents an unhandled promise if the gateway/storage is unavailable.
  const release = () => {
    if (scheduledProviderRefresh === run) scheduledProviderRefresh = null;
  };
  void run.then(release, release);
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
// Read/compare/remove is one worker-owned consent mutation. Call the
// inner version only when already holding the queue; nesting would deadlock.
function samePendingApproval(current, expected) {
  return Boolean(current && expected &&
    current.request_id === expected.request_id &&
    current.gateway === expected.gateway &&
    current.poll_secret === expected.poll_secret &&
    current.expires === expected.expires &&
    current.approvalGeneration === expected.approvalGeneration &&
    (current.connectionEpoch || null) === (expected.connectionEpoch || null));
}
function pendingApprovalConnectionValid(pending, current) {
  // Legacy pending records can complete until their original expiry only if
  // the connection has not acquired a replacement durable generation.
  if (!pending?.approvalGeneration) return !current.relayConnectionEpoch;
  return (pending.connectionEpoch || null) === (current.relayConnectionEpoch || null);
}
async function clearPendingApprovalIfCurrentLocked(pending) {
  const {pendingConnection:current} = await chrome.storage.local.get(["pendingConnection"]);
  if (!samePendingApproval(current, pending)) return false;
  await chrome.storage.local.remove("pendingConnection");
  await chrome.alarms?.clear?.("relay-connection");
  return true;
}
function clearPendingApprovalIfCurrent(pending) {
  return queueConsentMutation(() => clearPendingApprovalIfCurrentLocked(pending));
}
// A stalled fetch OR successful response with a stuck body parser must not
// pin the popup/alarm approval singleton forever. Keep the five-minute
// approval expiry authoritative: a timeout does not discard pending consent.
const APPROVAL_POLL_DEADLINE_MS = 12_000;
async function boundedApprovalPoll(gateway, pending) {
  const controller = new AbortController();
  let deadline;
  try {
    return await Promise.race([
      (async () => {
        const response = await fetch(gateway + "/relay/v1/connect/poll", {
          method:"POST", headers:{"Content-Type":"application/json"},
          body:JSON.stringify({request_id:pending.request_id,poll_secret:pending.poll_secret}),
          cache:"no-store", redirect:"error", signal:controller.signal
        });
        // HTTP 202 is a normal pending result; do not parse its empty body.
        if (response.status === 202 || !response.ok) {
          return {status:response.status, data:null};
        }
        return {status:response.status, data:await response.json()};
      })(),
      new Promise((_, reject) => {
        deadline = setTimeout(() => {
          reject(new Error("Connection approval poll timed out. Retry or reconnect."));
          controller.abort();
        }, APPROVAL_POLL_DEADLINE_MS);
      })
    ]);
  } finally {
    clearTimeout(deadline);
  }
}
async function completePendingConnection() {
  const initial = await chrome.storage.local.get(["pendingConnection", "relayConnectionEpoch"]);
  const pending = initial.pendingConnection;
  if (!pending) return;
  if (!pendingApprovalConnectionValid(pending, initial)) {
    await clearPendingApprovalIfCurrent(pending);
    throw new Error("Local approval connection changed. Connect again.");
  }
  if (Date.now() >= pending.expires) {
    await clearPendingApprovalIfCurrent(pending);
    throw new Error("Connection approval expired. Choose Connect to gateway again.");
  }
  const gateway = normalizeGateway(pending.gateway);
  if (!await chrome.permissions.contains({origins:[gateway + "/*"]})) throw new Error("Gateway permission is missing.");
  const {status, data} = await boundedApprovalPoll(gateway, pending);
  if (status === 202) return;
  if (data === null) {
    if (status === 410) await clearPendingApprovalIfCurrent(pending);
    throw new Error("Connection approval unavailable. Connect again from the extension.");
  }
  if (!/^[a-f0-9]{64}$/.test(data?.token)) throw new Error("Invalid connection token.");
  const latest = await chrome.storage.local.get(["pendingConnection","gateway","linkedProviders","relayConnectionEpoch"]);
  const current = latest.pendingConnection;
  // A prior approval response is not valid for a renewed request, even if a
  // request ID is reused. Fence the successful exchange to the full secret
  // and expiry identity rather than just its public ID and origin.
  if (!samePendingApproval(current, pending) ||
      !pendingApprovalConnectionValid(current, latest)) return;
  // A slow network response cannot extend the five-minute approval window.
  if (Date.now() >= current.expires) {
    await clearPendingApprovalIfCurrent(current);
    throw new Error("Connection approval expired. Choose Connect to gateway again.");
  }
  // A successful external approval is not permission to overwrite newer
  // provider consent or a replacement pending request. Finalize it through
  // the SAME writer as link/logout and revalidate every approval field after
  // awaiting that queue. The network response may be older than a popup
  // disconnect, re-pair or a linked-provider mutation.
  const committed = await queueConsentMutation(async () => {
    const now = await chrome.storage.local.get(
      ["pendingConnection", "gateway", "linkedProviders", "relayConnectionEpoch"]);
    const active = now.pendingConnection;
    if (!samePendingApproval(active, pending) ||
        !pendingApprovalConnectionValid(active, now)) {
      return false;
    }
    if (Date.now() >= active.expires) {
      await clearPendingApprovalIfCurrentLocked(active);
      throw new Error("Connection approval expired. Choose Connect to gateway again.");
    }
    const linkedProviders = now.gateway === gateway && Array.isArray(now.linkedProviders)
      ? now.linkedProviders : [];
    await chrome.storage.local.set({
      gateway, token:data.token, connectionMode:"paired", linkedProviders,
      relayConnectionEpoch:randomHex(16), pendingApprovalRequest:null
    });
    await chrome.storage.local.remove("pendingConnection");
    lastGatewayCheck = null;
    return true;
  });
  if (!committed) return;
  await chrome.alarms?.clear?.("relay-connection");
  resyncLinkedProviders().catch(() => {});
}
chrome.alarms?.onAlarm?.addListener?.(alarm => {
  if (alarm.name === "relay-connection") pollConnection().catch(() => {});
  if (alarm.name === "relay-session-refresh") refreshLinkedProviders();
});

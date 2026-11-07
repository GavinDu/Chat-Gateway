importScripts("shared.js");

const debounceTimers = new Map();

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
function queueProviderSync(provider) {
  return queueProviderTask(provider, () => syncProvider(provider));
}
function logoutProviderFromRelay(provider) {
  return queueProviderTask(provider, async () => {
    // Opt out before revocation; even if the gateway request fails, do not
    // upload more cookies until the user explicitly reconnects this provider.
    const {linkedProviders = []} = await chrome.storage.local.get(["linkedProviders"]);
    await chrome.storage.local.set({linkedProviders:linkedProviders.filter(value => value !== provider)});
    const response = await relayFetch("/relay/v1/session/" + encodeURIComponent(provider), {
      method:"DELETE",body:"{}"
    });
    if (!response.ok) throw new Error("Gateway logout failed (" + response.status + "). Auto sync is paused; retry logout.");
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
    }, 750);
    debounceTimers.set(provider, timer);
  }
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "poll-connection") {
    pollConnection().then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
  if (message?.type === "sync-provider" && PROVIDERS[message.provider]) {
    queueProviderSync(message.provider).then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
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
  await Promise.allSettled(
    [...new Set(linkedProviders)].filter(provider => PROVIDERS[provider]).map(queueProviderSync)
  );
}

function scheduleLinkedProviderResync() {
  chrome.alarms?.create?.("relay-session-refresh", {periodInMinutes:5});
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
      current?.poll_secret !== pending.poll_secret) return false;
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
  if (latest.pendingConnection?.request_id !== pending.request_id || latest.pendingConnection?.gateway !== gateway) return;
  const linkedProviders = latest.gateway === gateway ? (latest.linkedProviders || []) : [];
  await chrome.storage.local.set({gateway, token:data.token, linkedProviders});
  await chrome.storage.local.remove("pendingConnection");
  await chrome.alarms?.clear?.("relay-connection");
  resyncLinkedProviders().catch(() => {});
}
chrome.alarms?.onAlarm?.addListener?.(alarm => {
  if (alarm.name === "relay-connection") pollConnection().catch(() => {});
  if (alarm.name === "relay-session-refresh") scheduleLinkedProviderResync();
});

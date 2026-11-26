/* Local development control plane. No provider session material crosses this port. */
(() => {
  const HOST = "com.chat_gateway.reload_bridge";
  const ALARM = "native-bridge-reconnect";
  const permission = {permissions:["nativeMessaging"]};
  const instance = randomHex(16);
  const validId = id => typeof id === "string" && /^[a-f0-9]{32}$/.test(id);
  const pick = (value, allowed) => allowed.includes(value) ? value : "unknown";
  const states = ["syncing","uploaded","saved_activation_pending","unchanged","no_cookies","skipped","failed","not_attempted"];
  const issues = ["partition_scope_unsupported","gateway_cancelled","gateway_timeout","permission_or_consent","activation_timeout","gateway_unauthorized","relay_unavailable","activation_pending","not_connected","gateway_connection_changed","gateway_request_failed"];
  const flag = value => typeof value === "boolean" ? value : null;
  function project(snapshot) {
    const e = snapshot?.extension;
    const rows = Array.isArray(snapshot?.gateway?.providers) ? snapshot.gateway.providers : [];
    return {
      gateway_ok: snapshot?.ok === true,
      gateway_issue: snapshot?.ok === true ? null : pick(snapshot?.gateway_issue, issues),
      connected: e?.connected === true,
      connection_mode: pick(e?.connection_mode, ["local","paired","disconnected"]),
      providers: Object.keys(PROVIDERS).map(provider => {
        const matches = rows.filter(row => row?.provider === provider);
        const row = matches.length === 1 ? matches[0] : null;
        const sync = e?.provider_sync?.[provider];
        return {
          provider, consent: Array.isArray(e?.linked_providers) && e.linked_providers.includes(provider),
          permission: flag(e?.permissions?.providers?.[provider]),
          enabled: flag(row?.enabled), saved_session: flag(row?.linked),
          authentication: pick(row?.authentication, ["verified","needs_login","unknown"]),
          availability: pick(row?.availability, ["ready","degraded","unavailable","unknown"]),
          activation: pick(row?.activation, ["pending","ready","failed","cancelled","not_attempted"]),
          successful_chat_observed: flag(row?.successful_chat_observed),
          sync_state: pick(sync?.state, states), sync_issue: sync?.issue == null ? null : pick(sync.issue, issues),
          stale: sync?.stale !== false
        };
      })
    };
  }
  let port = null, opening = false, generation = 0, working = false, prepared = null;
  const busy = () => providerTasks.size > 0 || pendingConsentMutations > 0 || Boolean(scheduledProviderRefresh) || Boolean(connectionPoll);
  function stop() {
    generation++;
    prepared = null;
    const old = port;
    port = null;
    if (old) old.disconnect();
  }
  async function open() {
    if (opening || port) return;
    opening = true;
    const epoch = generation;
    try {
      if (!await chrome.permissions.contains(permission) || epoch !== generation) return;
      // Native ports keep the worker alive; the alarm only reconnects after a failure.
      await chrome.alarms.create(ALARM, {periodInMinutes:1});
      if (epoch !== generation || !await chrome.permissions.contains(permission) || epoch !== generation) {
        await chrome.alarms.clear(ALARM);
        return;
      }
      const connected = chrome.runtime.connectNative(HOST);
      port = connected;
      connected.onDisconnect.addListener(() => {
        void chrome.runtime.lastError; // consume, never forward raw host errors
        if (port === connected) { port = null; prepared = null; }
      });
      connected.onMessage.addListener(message => { void receive(connected, message); });
    } catch (_) { /* Host absent or permission revoked: retry on next alarm. */ }
    finally { opening = false; if (epoch !== generation) void open(); }
  }
  async function receive(source, request) {
    if (source !== port || !request || request.v !== 1 || !validId(request.id) ||
        Object.keys(request).sort().join(",") !== "command,id,v") return;
    const reply = (ok, result) => {
      if (source !== port) return false;
      try { source.postMessage({v:1,id:request.id,ok,...(ok ? {result} : {error:result})}); return true; }
      catch (_) { return false; }
    };
    if (working) { reply(false,"busy"); return; }
    working = true;
    try {
      if (!await chrome.permissions.contains(permission) || source !== port) return;
      switch (request.command) {
        case "ping": {
          const {nativeBridgeReloadReceipt: receipt} = await chrome.storage.local.get("nativeBridgeReloadReceipt");
          reply(true,{instance,version:chrome.runtime.getManifest().version,
            reload_receipt: validId(receipt) ? receipt : null});
          break;
        }
        case "diagnostics":
          reply(true,project(await relayDiagnostics(false)));
          break;
        case "reload_extension":
          if (busy() || (prepared && prepared.until > Date.now())) { reply(false,"busy"); break; }
          prepared = {id:request.id,until:Date.now()+10000};
          if (!reply(true,{stage:"accepted",instance})) prepared = null;
          break;
        case "reload_commit":
          if (!prepared || prepared.id !== request.id || prepared.until < Date.now() || busy()) {
            prepared = null; reply(false,"reload_not_prepared"); break;
          }
          prepared = null;
          await chrome.storage.local.set({nativeBridgeReloadReceipt:request.id});
          if (source !== port || busy() || !await chrome.permissions.contains(permission) || source !== port || busy()) {
            await chrome.storage.local.remove("nativeBridgeReloadReceipt");
            reply(false,"busy"); break;
          }
          try { chrome.runtime.reload(); }
          catch (_) { await chrome.storage.local.remove("nativeBridgeReloadReceipt"); throw _; }
          break;
        default: reply(false,"unsupported_command");
      }
    } catch (_) { reply(false,"unavailable"); }
    finally { working = false; }
  }
  chrome.permissions.onAdded.addListener(change => {
    if (change.permissions?.includes("nativeMessaging")) void open();
  });
  chrome.permissions.onRemoved.addListener(change => {
    if (change.permissions?.includes("nativeMessaging")) {
      stop(); void chrome.alarms.clear(ALARM);
    }
  });
  chrome.alarms.onAlarm.addListener(alarm => { if (alarm.name === ALARM) void open(); });
  chrome.runtime.onStartup.addListener(() => { void open(); });
  chrome.runtime.onInstalled.addListener(() => { void open(); });
  void open();
})();

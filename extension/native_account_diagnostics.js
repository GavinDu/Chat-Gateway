/* Fixed read-only account diagnosis. Credentials never leave the first-party page. */
async function nativeChatgptAccountDiagnostic(lease = async () => true) {
  const unknown = reason => ({provider:"chatgpt",context:"chrome_native_diagnostic",
    authentication:"unknown",reason,http_status:null,auth_mode:"unknown",
    checked_at:new Date().toISOString(),exported_snapshot_verified:false});
  const keys = ["gateway","token","connectionMode","linkedProviders","providerLinkEpochs"];
  let active = true, timer, created = null;
  const owned = (before, after) => hasRelayConnection(after) &&
    before.gateway === after.gateway && before.token === after.token &&
    before.connectionMode === after.connectionMode &&
    Array.isArray(after.linkedProviders) && after.linkedProviders.includes("chatgpt") &&
    before.providerLinkEpochs?.chatgpt === after.providerLinkEpochs?.chatgpt;
  const firstParty = value => {
    try { const u = new URL(value); return u.protocol === "https:" &&
      ["chatgpt.com","www.chatgpt.com"].includes(u.hostname) && !u.port && !u.username && !u.password; }
    catch (_) { return false; }
  };
  const closeOwnedTab = async id => {
    let cleanupTimer;
    try {
      await Promise.race([(async()=>{
        const tab=await chrome.tabs.get(id);
        if (tab && tab.active === false) await chrome.tabs.remove(id);
      })(),new Promise(resolve=>{cleanupTimer=setTimeout(resolve,1000);})]);
    } catch (_) { /* Never expose browser errors. */ }
    finally { clearTimeout(cleanupTimer); }
  };
  const work = async () => {
    const before = await chrome.storage.local.get(keys);
    if (!active) return unknown("verification_timeout");
    if (!hasRelayConnection(before) || !Array.isArray(before.linkedProviders) ||
        !before.linkedProviders.includes("chatgpt")) return unknown("not_linked");
    const origins = requiredProviderOrigins("chatgpt");
    if (!await chrome.permissions.contains({origins})) return unknown("permission_missing");
    if (!active) return unknown("verification_timeout");
    const snapshot = await relayDiagnostics(false);
    if (!active) return unknown("verification_timeout");
    const provider = snapshot?.gateway?.providers?.filter(row => row?.provider === "chatgpt");
    if (!snapshot?.ok || provider?.length !== 1) return unknown("verification_unavailable");
    if (provider[0].enabled !== true) return unknown("provider_disabled");
    if (!chrome.tabs?.query || !chrome.tabs?.get || !chrome.scripting?.executeScript) return unknown("tab_unavailable");
    const tabs = await chrome.tabs.query({url:["https://chatgpt.com/*","https://www.chatgpt.com/*"]});
    if (!active) return unknown("verification_timeout");
    let tab = (Array.isArray(tabs) ? tabs : []).filter(t => Number.isSafeInteger(t?.id) && firstParty(t.url))
      .sort((a,b) => Number(b.active) - Number(a.active) || (b.lastAccessed || 0) - (a.lastAccessed || 0))[0];
    if (!tab) {
      if (!active || !await lease()) return unknown("source_changed");
      if (!chrome.tabs.create) return unknown("tab_unavailable");
      tab = await chrome.tabs.create({url:"https://chatgpt.com/",active:false});
      if (Number.isSafeInteger(tab?.id)) created = tab.id;
      if (!active) { if (created !== null) await closeOwnedTab(created); return unknown("verification_timeout"); }
      const deadline = Date.now() + 15000;
      while (tab?.status !== "complete" && active && Date.now() < deadline) {
        await new Promise(resolve => setTimeout(resolve,100));
        if (!active) return unknown("verification_timeout");
        tab = await chrome.tabs.get(created);
      }
    }
    if (!active) return unknown("verification_timeout");
    if (!Number.isSafeInteger(tab?.id) || !firstParty(tab.url) || tab.status !== "complete") return unknown("tab_unavailable");
    const tabId = tab.id, initialUrl = tab.url;
    const midway = await chrome.storage.local.get(keys);
    if (!active || !owned(before,midway)) return unknown("source_changed");
    if (!await chrome.permissions.contains({origins}) || !active) return unknown("permission_missing");
    if (!active || !await lease()) return unknown("source_changed");
    const runs = await chrome.scripting.executeScript({target:{tabId,allFrames:false},world:"MAIN",
      func:probeActiveChatgptAccount,args:["chrome_native_diagnostic"]});
    if (!active) return unknown("verification_timeout");
    const afterTab = await chrome.tabs.get(tabId);
    const afterOwner = await chrome.storage.local.get(keys);
    if (!active || !owned(before,afterOwner) || afterTab?.id !== tabId ||
        afterTab?.url !== initialUrl || !firstParty(afterTab.url)) return unknown("source_changed");
    if (!await chrome.permissions.contains({origins}) || !active) return unknown("permission_missing");
    return Array.isArray(runs) && runs.length === 1 && runs[0]?.frameId === 0
      ? runs[0].result : unknown("verification_unavailable");
  };
  try {
    return await Promise.race([work(), new Promise(resolve => {
      timer = setTimeout(() => { active = false; resolve(unknown("verification_timeout")); },35000);
    })]);
  } catch (_) { return unknown("verification_unavailable"); }
  finally {
    active = false; clearTimeout(timer);
    if (created !== null && chrome.tabs?.remove) {
      // Close only a tab created by this operation which the user has not
      // activated. Existing or user-adopted tabs are never closed.
      await closeOwnedTab(created);
    }
  }
}

function projectNativeChatgptAccount(r, now = Date.now()) {
  var F=function(){return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'unknown',reason:'verification_unavailable',http_status:null,auth_mode:'unknown',exported_snapshot_verified:false};};
  if(typeof r!=='object'||r===null||Array.isArray(r))return F();
  if(r.provider!=='chatgpt'||r.context!=='chrome_native_diagnostic'||r.exported_snapshot_verified!==false)return F();
  if(typeof r.checked_at!=='string')return F();
  var d=new Date(r.checked_at);
  if(Number.isNaN(d.getTime()))return F();
  if(d.toISOString()!==r.checked_at)return F();
  if(typeof now!=='number'||!Number.isFinite(now))return F();
  if(Math.abs(d.getTime()-now)>30000)return F();
  var h=r.http_status;
  if(!(h===null||(Number.isInteger(h)&&h>=100&&599>=h)))return F();
  var a=r.authentication;
  var re=r.reason;
  var m=r.auth_mode;
  if(a==='verified'){if(re==='first_party_session_confirmed'&&m==='session'&&h===200)return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'verified',reason:'first_party_session_confirmed',http_status:200,auth_mode:'session',exported_snapshot_verified:false};
  if(re==='bearer_account_confirmed'&&m==='bearer'&&h===200)return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'verified',reason:'bearer_account_confirmed',http_status:200,auth_mode:'bearer',exported_snapshot_verified:false};
  return F();}
  if(a==='unauthenticated'){if(re==='provider_rejected_session'&&m==='unknown'&&h===401)return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'unauthenticated',reason:'provider_rejected_session',http_status:401,auth_mode:'unknown',exported_snapshot_verified:false};
  if(re==='provider_reports_no_session'&&m==='unknown'&&h===200)return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'unauthenticated',reason:'provider_reports_no_session',http_status:200,auth_mode:'unknown',exported_snapshot_verified:false};
  return F();}
  if(a==='unknown'){if(m!=='unknown')return F();
  var L=['unsupported_origin','tab_changed','provider_or_network_blocked','unrecognized_session_response','incomplete_or_expired_session_evidence','verification_timeout','verification_unavailable','session_reported_error','access_token_rejected','account_principal_mismatch','unrecognized_account_response','tab_unavailable','source_changed','permission_missing','not_linked','provider_disabled','busy'];
  if(L.indexOf(re)===-1)return F();
  return {provider:'chatgpt',context:'chrome_native_diagnostic',authentication:'unknown',reason:re,http_status:h,auth_mode:'unknown',exported_snapshot_verified:false};}return F();}

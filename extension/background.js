importScripts("shared.js");

const debounceTimers = new Map();
chrome.cookies.onChanged.addListener(({cookie}) => {
  const domains = Object.entries(PROVIDERS);
  const host = cookie.domain.replace(/^\./, "").toLowerCase();
  for (const [provider, roots] of domains) {
    if (!roots.some(root => host === root || host.endsWith("." + root))) continue;
    clearTimeout(debounceTimers.get(provider));
    const timer = setTimeout(() => {
      if (debounceTimers.get(provider) !== timer) return;
      debounceTimers.delete(provider);
      syncProvider(provider).catch(() => {
        // Never log cookie material or relay credentials. The popup can surface status on demand.
      });
    }, 750);
    debounceTimers.set(provider, timer);
  }
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "sync-provider" && PROVIDERS[message.provider]) {
    syncProvider(message.provider).then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
});


async function resyncLinkedProviders() {
  const {linkedProviders = []} = await chrome.storage.local.get(["linkedProviders"]);
  // Startup restoration is best-effort: providers remain independently usable
  // if one domain is unavailable, and no session material is written to logs.
  await Promise.allSettled(
    [...new Set(linkedProviders)].filter(provider => PROVIDERS[provider]).map(syncProvider)
  );
}

chrome.runtime.onStartup.addListener(() => {
  resyncLinkedProviders().catch(() => {
    // Do not log provider session material or relay credentials.
  });
});

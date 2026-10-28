importScripts("shared.js");

const debounceTimers = new Map();
chrome.cookies.onChanged.addListener(({cookie}) => {
  const domains = Object.entries(PROVIDERS);
  const host = cookie.domain.replace(/^\./, "").toLowerCase();
  for (const [provider, roots] of domains) {
    if (!roots.some(root => host === root || host.endsWith("." + root))) continue;
    clearTimeout(debounceTimers.get(provider));
    debounceTimers.set(provider, setTimeout(() => {
      syncProvider(provider).catch(() => {
        // Never log cookie material or relay credentials. The popup can surface status on demand.
      });
    }, 750));
  }
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === "sync-provider" && PROVIDERS[message.provider]) {
    syncProvider(message.provider).then(() => sendResponse({ok:true})).catch(error => sendResponse({ok:false,error:error.message}));
    return true;
  }
});

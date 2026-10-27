const ALLOWED_STORAGE_KEYS = new Set([]); // Empty until a provider-specific key is justified and reviewed.
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || message.type !== "CG_READ_LOCAL_STORAGE") return;
  const requested = Array.isArray(message.keys) ? message.keys : [];
  const values = {};
  for (const key of requested) {
    if (typeof key === "string" && ALLOWED_STORAGE_KEYS.has(key)) {
      try {
        const value = localStorage.getItem(key);
        if (value !== null) values[key] = value;
      } catch (_) { /* Fail closed. */ }
    }
  }
  sendResponse({ values });
});

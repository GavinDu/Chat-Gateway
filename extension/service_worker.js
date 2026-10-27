// Intentionally no background polling or automatic cookie export.
// Session capture is initiated explicitly from popup.js by the user.
chrome.runtime.onInstalled.addListener(() => {
  console.info("Chat Gateway Browser Relay installed; no provider data is collected automatically.");
});

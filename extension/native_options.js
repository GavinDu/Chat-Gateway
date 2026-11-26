const DISABLED = "nativeBridgeDisabled";
const status = document.getElementById("status");
const enable = document.getElementById("enable");
const disable = document.getElementById("disable");
async function refreshNativeSetting() {
  const state = await chrome.storage.local.get(DISABLED);
  const granted = await chrome.permissions.contains({permissions:["nativeMessaging"]});
  const paused = state[DISABLED] === true;
  enable.disabled = !granted || !paused;
  disable.disabled = paused;
  status.textContent = !granted ? "Chrome native messaging permission is unavailable. Check extension installation." :
    paused ? "Local bridge disabled. Your provider connections are unchanged." :
    "Local bridge enabled automatically. Use the CLI to check host connectivity.";
}
async function setDisabled(value) {
  enable.disabled = disable.disabled = true;
  try { await chrome.storage.local.set({[DISABLED]:value}); await refreshNativeSetting(); }
  catch (_) { status.textContent = "Bridge setting unavailable. Reopen options and retry."; }
}
enable.addEventListener("click", () => { void setDisabled(false); });
disable.addEventListener("click", () => { void setDisabled(true); });
refreshNativeSetting().catch(() => { status.textContent = "Bridge status unavailable."; });

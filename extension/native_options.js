const permission = {permissions:["nativeMessaging"]};
const status = document.getElementById("status");
async function refreshNativePermission() {
  const enabled = await chrome.permissions.contains(permission);
  status.textContent = enabled ? "Permission granted. Use the CLI to check host connectivity." : "Native messaging disabled.";
}
document.getElementById("enable").addEventListener("click", async () => {
  try { await chrome.permissions.request(permission); await refreshNativePermission(); }
  catch (_) { status.textContent = "Permission request unavailable."; }
});
document.getElementById("disable").addEventListener("click", async () => {
  try { await chrome.permissions.remove(permission); await refreshNativePermission(); }
  catch (_) { status.textContent = "Permission revocation unavailable."; }
});
refreshNativePermission().catch(() => { status.textContent = "Permission status unavailable."; });

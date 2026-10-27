const $ = id => document.getElementById(id);
async function send(message) {
  const response = await chrome.runtime.sendMessage(message);
  if (!response || !response.ok) throw new Error(response && response.error ? response.error : "Action failed.");
  return response.result;
}
function show(text) { $("status").textContent = text; }
$("pair").addEventListener("click", async () => {
  try { await send({ type: "CG_PAIR", code: $("code").value.trim() }); $("code").value = ""; show("Paired with local gateway."); }
  catch (e) { show(e.message); }
});
$("link").addEventListener("click", async () => {
  const provider = $("provider").value;
  const labels = {qwen:"Qwen",metaai:"Meta AI",claude:"Claude",chatgpt:"ChatGPT",gemini:"Gemini",grok:"Grok",mistral:"Mistral",perplexity:"Perplexity"};
  if (!confirm(`Allow Chat Gateway to transfer session cookies for ${labels[provider]} from this approved tab to your local gateway? Passwords and unrelated origins are not collected.`)) return;
  try {
    await send({ type: "CG_PREPARE_LINK", provider });
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    const result = await send({ type: "CG_LINK_TAB", tabId: tab.id });
    show(`Linked ${result.provider}; transferred ${result.cookieCount} cookies.`);
  } catch (e) { show(e.message); }
});
$("refresh").addEventListener("click", async () => {
  try { show(JSON.stringify(await send({ type: "CG_STATUS" }), null, 2)); } catch (e) { show(e.message); }
});
$("unlink").addEventListener("click", async () => {
  if (!confirm("Unlink this provider and delete its gateway vault entry?")) return;
  try { const r = await send({ type: "CG_UNLINK", provider: $("provider").value }); show(`Unlinked ${r.unlinked}.`); } catch (e) { show(e.message); }
});

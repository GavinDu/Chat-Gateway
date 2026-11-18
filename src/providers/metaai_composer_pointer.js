(() => {
  const metaEditor = __ELEMENT__;
  if (!['https://www.meta.ai', 'https://meta.ai'].includes(location.origin)) return false;
  const selector = 'div[contenteditable="true"][role="textbox"]';
  const eligible = node => {
    try {
      return node.matches(selector) && node.getClientRects().length > 0 &&
        getComputedStyle(node).visibility !== 'hidden' &&
        getComputedStyle(node).pointerEvents === 'auto' &&
        !node.closest('[hidden],[inert],[aria-hidden="true"],[aria-disabled="true"],[aria-readonly="true"],dialog,[role="dialog"],[role="search"]');
    } catch (_) { return false; }
  };
  if (!eligible(metaEditor)) return false;
  if (Array.from(document.querySelectorAll('[aria-modal="true"],dialog[open]'))
      .some(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden')) return false;
  const editors = Array.from(document.querySelectorAll(selector)).filter(eligible);
  if (editors.length !== 1 || editors[0] !== metaEditor) return false;
  if(document.activeElement === metaEditor && metaEditor.getAttribute('data-lexical-editor') === 'true') return true;
  try {
    const box = metaEditor.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return !!hit && (hit === metaEditor || metaEditor.contains(hit));
  } catch (_) { return false; }
})()

(editor => {
  // Obscura innerText collapses significant whitespace, even in pre-wrap.
  // Read only visible editor text nodes and explicit line breaks instead.
  if (!editor) return null;
  const visible = node => {
    if (node.closest('[hidden],[inert],[aria-hidden="true"],script,style')) return false;
    for (let current = node; current; current = current.parentElement) {
      const style = getComputedStyle(current);
      if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) return false;
    }
    return true;
  };
  const read = node => {
    if (node.nodeType === 3) return visible(node.parentElement) ? node.textContent : '';
    if (node.nodeType !== 1 || !visible(node)) return '';
    if (node.tagName === 'BR') return '\n';
    if (/^(IMG|SVG|VIDEO|AUDIO|IFRAME|INPUT|TEXTAREA|CANVAS)$/.test(node.tagName)) return null;
    // Do not concatenate distinct block paragraphs into a different prompt.
    const children = Array.from(node.childNodes).filter(child => child.nodeType === 3 ? child.textContent !== '' : child.nodeType === 1 && visible(child));
    const blocks = children.filter(child => child.nodeType === 1 && /^(P|DIV|PRE|LI|UL|OL|BLOCKQUOTE)$/.test(child.tagName));
    if (blocks.length && children.length > 1) return null;
    let result = '';
    for (const child of children) {
      const text = read(child);
      if (text === null) return null;
      result += text;
    }
    return result;
  };
  return visible(editor) ? read(editor) : null;
})

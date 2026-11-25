(() => {
  if(!['https://www.meta.ai','https://meta.ai'].includes(location.origin))return false;
  if(Array.from(document.querySelectorAll('[aria-modal="true"],dialog[open]')).some(node=>node.getClientRects().length>0 && getComputedStyle(node).visibility!=='hidden'))return false;
  const nontransparent=node=>{for(let parent=node;parent;parent=parent.parentElement){if(Number(getComputedStyle(parent).opacity)===0)return false;}return true;};
  const selector=__SELECTOR__;
  const eligible=el=>nontransparent(el) && el.getClientRects().length>0 && getComputedStyle(el).visibility!=='hidden' && !el.closest('[hidden],[inert],[aria-hidden="true"],[aria-disabled="true"],[aria-readonly="true"],dialog,[role="dialog"],[role="search"]');
  const lexical=el=>el.getAttribute('data-lexical-editor')==='true' && el.getAttribute('contenteditable')==='true';
  const editors=()=>Array.from(document.querySelectorAll(selector)).filter(eligible);
  const matches=editors();
  if(matches.length!==1 || !lexical(matches[0]))return false;
  const el=matches[0];
  // Navigation need not autofocus Lexical. Focus only the validated editor,
  // then recheck: a focus handler may replace/disable it or open a dialog.
  try{el.focus();}catch(_){return false;}
  if(!['https://www.meta.ai','https://meta.ai'].includes(location.origin))return false;
  if(Array.from(document.querySelectorAll('[aria-modal="true"],dialog[open]')).some(node=>node.getClientRects().length>0 && getComputedStyle(node).visibility!=='hidden'))return false;
  const current=editors();
  if(current.length!==1 || current[0]!==el || !lexical(el) || document.activeElement!==el)return false;
  const prompt=__PROMPT__,clipboard={types:['text/plain'],files:[],items:[],getData(kind){return kind==='text/plain'||kind==='Text'?prompt:'';}};
  try{const event=new ClipboardEvent('paste',{bubbles:true,cancelable:true,composed:true});Object.defineProperty(event,'clipboardData',{value:clipboard});return !el.dispatchEvent(event);}catch(_){return false;}
})()

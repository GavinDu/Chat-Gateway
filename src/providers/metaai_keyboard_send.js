(() => {
  if(!['https://www.meta.ai','https://meta.ai'].includes(location.origin))return 'off_origin';
  const expected=__PROMPT__;
  const nontransparent=node=>{for(let n=node;n;n=n.parentElement){if(Number(getComputedStyle(n).opacity)===0)return false;}return true;};
  const visible=node=>nontransparent(node) && node.getClientRects().length>0 && getComputedStyle(node).visibility!=='hidden' && !node.closest('[hidden],[inert],[aria-hidden="true"],[aria-disabled="true"],[aria-readonly="true"],dialog,[role="dialog"],[role="search"]');
  if(Array.from(document.querySelectorAll('[aria-modal="true"],dialog[open]')).some(node=>node.getClientRects().length>0 && getComputedStyle(node).visibility!=='hidden' && !node.closest('[hidden],[inert],[aria-hidden="true"]')))return 'modal';
  const editors=Array.from(document.querySelectorAll('div[contenteditable="true"][role="textbox"][data-lexical-editor="true"]')).filter(visible);
  if(editors.length!==1 || document.activeElement!==editors[0])return 'editor_unavailable';
  const editor=editors[0];if((editor.innerText??editor.textContent??'')!==expected)return 'draft_changed';
  const controls=Array.from(document.querySelectorAll('button,[role="button"]')).filter(button=>visible(button) && (button.getAttribute('aria-label')||button.innerText||'').trim().toLowerCase()==='send');
  if(controls.length!==1)return 'send_ambiguous_or_missing';
  const send=controls[0];if(send.disabled || send.matches(':disabled') || send.closest('fieldset[disabled]'))return 'send_disabled';
  let common=editor,depth=0;while(common && !common.contains(send) && depth<12){common=common.parentElement;depth++;}
  if(!common || !common.contains(send) || ['BODY','HTML'].includes(common.tagName))return 'send_unscoped';
  const handled=!editor.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',code:'Enter',keyCode:13,which:13,bubbles:true,cancelable:true,composed:true}));
  return handled?'dispatched':'not_handled';
})()

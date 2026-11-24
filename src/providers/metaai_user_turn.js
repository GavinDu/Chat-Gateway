(() => {
  if (!['https://www.meta.ai', 'https://meta.ai'].includes(location.origin)) return {valid:false,count:0,draft_empty:false};
  const expected=__PROMPT__, canonical=text=>(text||'').replace(/\r\n?/g,'\n');
  const excluded='[hidden],[inert],[aria-hidden="true"],aside,nav,header,dialog,[role="dialog"],button,[role="button"],script,style,[contenteditable="true"],div.prose';
  const visible=node=>{
    if(!node || !node.getClientRects().length || node.closest(excluded))return false;
    for(let current=node;current;current=current.parentElement){
      const style=getComputedStyle(current);
      if(style.visibility==='hidden' || Number(style.opacity)===0)return false;
    }
    return true;
  };
  const visibleText=node=>{
    if(node.nodeType===3)return visible(node.parentElement)?node.textContent:'';
    if(node.nodeType!==1 || !visible(node))return '';
    if(node.tagName==='BR')return '\n';
    return Array.from(node.childNodes).map(visibleText).join('');
  };
  const matches=Array.from(document.querySelectorAll('main div,main p')).filter(node=>
    visible(node) && !node.querySelector('[contenteditable="true"],[role="textbox"],div.prose') &&
    canonical(visibleText(node))===canonical(expected));
  const leaves=matches.filter(node=>!matches.some(other=>other!==node && node.contains(other)));
  const editors=Array.from(document.querySelectorAll('div[contenteditable="true"][role="textbox"][data-lexical-editor="true"]'));
  return {valid:true,count:Math.min(32,leaves.length),draft_empty:editors.length===1 && (() => {const draft=(__COMPOSER_TEXT__)(editors[0]);return draft!==null && !draft.trim();})()};
})()

(() => {
  const root = document.querySelector('[data-split-layout]'); if (!root) return;
  const divider = root.querySelector('[data-splitter]'), feed = root.querySelector('[data-feed-panel]'), chat = root.querySelector('[data-chat-panel]');
  const key = 'crypto-layout-v19-' + (location.protocol === 'file:' ? 'prototype' : 'app');
  let ratio = 100 / 3 * 2, hidden = false, dragging = false;
  try { const saved = JSON.parse(localStorage.getItem(key) || '{}'); if(Number.isFinite(saved.ratio)) ratio=Math.min(72,Math.max(28,saved.ratio));hidden=saved.hidden===true; } catch (_) {}
  function paint(){root.style.setProperty('--feed-fr',ratio+'fr');root.style.setProperty('--chat-fr',(100-ratio)+'fr');root.classList.toggle('chat-hidden',hidden);divider.setAttribute('aria-valuenow',Math.round(ratio));root.querySelectorAll('[data-collapse-chat]').forEach(b=>b.setAttribute('aria-expanded',String(!hidden)));}
  function save(){try{localStorage.setItem(key,JSON.stringify({ratio,hidden}));}catch(_){}}
  function expand(){hidden=false;paint();save();}
  root.addEventListener('expand-chat',expand);
  root.addEventListener('click',e=>{if(e.target.closest('[data-collapse-chat]')){hidden=true;paint();save();root.querySelector('[data-expand-chat]').focus();}if(e.target.closest('[data-expand-chat]'))expand();});
  divider.addEventListener('pointerdown',e=>{if(e.button!==0)return;dragging=true;divider.setPointerCapture(e.pointerId);root.classList.add('resizing');e.preventDefault();});
  divider.addEventListener('pointermove',e=>{if(!dragging)return;const f=feed.getBoundingClientRect(),c=chat.getBoundingClientRect();const total=f.width+c.width;ratio=Math.min(72,Math.max(28,(e.clientX-f.left)/total*100));paint();});
  function finish(){if(!dragging)return;dragging=false;root.classList.remove('resizing');save();}
  divider.addEventListener('pointerup',finish);divider.addEventListener('pointercancel',finish);divider.addEventListener('lostpointercapture',finish);
  divider.addEventListener('dblclick',()=>{ratio=100 / 3 * 2;paint();save();});
  divider.addEventListener('keydown',e=>{if(!['ArrowLeft','ArrowRight','Home','End'].includes(e.key))return;e.preventDefault();ratio=e.key==='Home'?28:e.key==='End'?72:Math.min(72,Math.max(28,ratio+(e.key==='ArrowLeft'?-2:2)));paint();save();});
  paint();
})();

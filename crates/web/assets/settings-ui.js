/* One settings view, shared by the home dialog and direct settings links. */
(() => {
  const dialog = document.getElementById('app-settings');
  let active = 's-watch', opening = null;
  const loaders = new WeakMap();
  function dirty(node) {
    return [...node.querySelectorAll('form input,form select,form textarea')].some(input => {
      if (input.type === 'search' || input.type === 'hidden') return false;
      if (input.type === 'checkbox' || input.type === 'radio') return input.checked !== input.defaultChecked;
      if (input.tagName === 'SELECT') return [...input.options].some(o => o.selected !== o.defaultSelected);
      return input.value !== input.defaultValue;
    });
  }
  function invalidate(shell) {shell.querySelectorAll('[data-settings-url]').forEach(node => node.dataset.settingsLoaded = 'false')}
  window.addEventListener('app:configuration-changed', () => document.querySelectorAll('[data-settings-shell]').forEach(invalidate));
  async function load(node) {
    if (!node || node.dataset.settingsLoaded === 'true' || dirty(node)) return;
    if (loaders.has(node)) return loaders.get(node);
    const task = (async () => {
      node.textContent = '正在加载…';
      try {
        const response = await fetch(node.dataset.settingsUrl, {cache:'no-store', signal:AbortSignal.timeout(10000)});
        if (!response.ok) throw Error('settings unavailable');
        node.innerHTML = await response.text();
        window.htmx.process(node);
        node.dataset.settingsLoaded = 'true';
        document.dispatchEvent(new CustomEvent('settings:ready'));
        window.AppStatus?.paint(); window.AppStatus?.sync();
      } catch (_) {
        node.replaceChildren();
        const message = document.createElement('p'); message.setAttribute('role', 'alert');
        message.textContent = '加载失败，请重试。';
        const retry = document.createElement('button'); retry.type = 'button'; retry.textContent = '重新加载';
        retry.onclick = () => load(node); node.append(message, retry);
      } finally { loaders.delete(node); }
    })();
    loaders.set(node, task); return task;
  }
  function select(shell, id) {
    const tabs = [...shell.querySelectorAll('[data-s]')];
    if (!tabs.some(t => t.dataset.s === id)) id = 's-watch';
    active = id;
    tabs.forEach(tab => {
      const on = tab.dataset.s === id;
      tab.classList.toggle('on', on); tab.setAttribute('aria-selected', String(on)); tab.tabIndex = on ? 0 : -1;
    });
    shell.querySelectorAll('.settings-pane').forEach(p => p.hidden = p.id !== id);
    shell.querySelector('.sc').scrollTop = 0;
    const pane = [...shell.querySelectorAll('.settings-pane')].find(p => p.id === id);
    load(pane.querySelector('[data-settings-url]'));
    if (!shell.closest('dialog')) history.replaceState(null, '', '#' + id);
  }
  function mount(shell, id) {
    if (!shell.dataset.settingsReady) {
      shell.dataset.settingsReady = 'true';
      shell.querySelectorAll('[data-s]').forEach(tab => {
        tab.onclick = e => {e.preventDefault(); select(shell, tab.dataset.s)};
        tab.onkeydown = e => {
          const tabs = [...shell.querySelectorAll('[data-s]')], index = tabs.indexOf(tab);
          let next;
          if (e.key === 'ArrowDown' || e.key === 'ArrowRight') next = tabs[(index + 1) % tabs.length];
          if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') next = tabs[(index + tabs.length - 1) % tabs.length];
          if (e.key === 'Home') next = tabs[0]; if (e.key === 'End') next = tabs.at(-1);
          if (next) {e.preventDefault(); select(shell, next.dataset.s); next.focus()}
        };
      });
      shell.querySelector('[data-settings-close]').onclick = () => shell.closest('dialog') ? dialog.close() : location.assign('/');
      shell.addEventListener('click', e => {
        const button = e.target.closest('[data-settings-question]'); if (!button) return;
        const input = document.getElementById('q');
        if (!input) {location.assign('/?question=' + encodeURIComponent(button.dataset.settingsQuestion)); return}
        dialog.close(); input.value = button.dataset.settingsQuestion;
        input.dispatchEvent(new Event('input', {bubbles:true})); input.focus();
      });
      document.dispatchEvent(new CustomEvent('settings:ready'));
    }
    select(shell, id);
  }
  async function open(id) {
    const standalone = document.querySelector('[data-settings-page] [data-settings-shell]');
    if (standalone) {mount(standalone, id || active); return}
    if (!dialog.open) {
      const shell = dialog.querySelector('[data-settings-shell]'); if (shell) invalidate(shell);
      dialog.showModal();
    }
    if (!dialog.querySelector('[data-settings-shell]')) {
      if (!opening) opening = (async () => {
        const response = await fetch('/settings/panel', {cache:'no-store'});
        if (!response.ok) throw Error('无法读取设置，请重试。');
        dialog.innerHTML = await response.text(); window.htmx.process(dialog);
      })().finally(() => opening = null);
      try {await opening} catch (_) {
        dialog.replaceChildren();
        const text = document.createElement('p'); text.className = 'settings-loading'; text.textContent = '设置加载失败，请关闭后重试。';
        const close = document.createElement('button'); close.type = 'button'; close.textContent = '关闭'; close.onclick = () => dialog.close();
        text.append(close); dialog.append(text); return;
      }
    }
    mount(dialog.querySelector('[data-settings-shell]'), id || active);
    dialog.querySelector('[data-settings-close]').focus();
  }
  document.addEventListener('click', e => {
    if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) return;
    const link = e.target.closest('a[href]'); if (!link || link.closest('[data-settings-shell]')) return;
    const url = new URL(link.href, location.href);
    if (url.origin !== location.origin || url.pathname !== '/settings') return;
    e.preventDefault(); open(url.hash.slice(1));
  });
  dialog?.addEventListener('click', e => {
    if (e.target !== dialog) return;
    const r = dialog.getBoundingClientRect();
    if (e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom) dialog.close();
  });
  const observedLists = new WeakSet();
  function scrollHints() {
    document.querySelectorAll('.x-panel').forEach(panel => {
      const list = panel.querySelector('.x-accounts'), hint = panel.querySelector('[data-x-scroll-hint]');
      if (!list || !hint) return;
      const update = () => {hint.hidden = list.scrollHeight - list.clientHeight - list.scrollTop < 3};
      if (!observedLists.has(list)) {observedLists.add(list); list.addEventListener('scroll', update, {passive:true})}
      update();
    });
  }
  const refreshHints = () => requestAnimationFrame(scrollHints);
  document.addEventListener('settings:ready', refreshHints);
  document.body.addEventListener('htmx:afterSettle', refreshHints);
  document.addEventListener('input', e => {if (e.target.matches('[data-x-search]')) refreshHints()});
  window.addEventListener('resize', refreshHints);
  const standalone = document.querySelector('[data-settings-page] [data-settings-shell]');
  if (standalone) {mount(standalone, location.hash.slice(1)); window.addEventListener('hashchange', () => select(standalone, location.hash.slice(1)))}
  const question = new URLSearchParams(location.search).get('question'), input = document.getElementById('q');
  if (question && input) {input.value = question; input.dispatchEvent(new Event('input', {bubbles:true})); input.focus()}
})();

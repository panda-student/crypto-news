(() => {
  let channels = null, busy = false, failed = false, xSearch = '';
  const topics = new Map(), listeners = new Map();
  function subscribe(topic, listener) {
    if (!listeners.has(topic)) listeners.set(topic, new Set());
    listeners.get(topic).add(listener);
    if (topics.has(topic)) listener(topics.get(topic));
    return () => listeners.get(topic)?.delete(listener);
  }
  function accept(topic, result, at) {
    const previous = topics.get(topic);
    const state = {data: result.ok ? result.data : previous?.data, stale: !result.ok, error: result.error || null, updatedAt: result.ok ? at : previous?.updatedAt};
    topics.set(topic, state);
    listeners.get(topic)?.forEach(listener => { try {listener(state);} catch (_) {} });
  }
  const date = value => {
    const time = value && new Date(value);
    return time && !isNaN(time) ? time.toLocaleString() : '尚未收到';
  };
  function describeSources(rows) {
    const x=rows.filter(s=>s.id.startsWith('x:')), other=rows.filter(s=>!s.id.startsWith('x:'));
    const ok=items=>items.filter(s=>s.state==='ok').length;
    return {compact:`信源 ${ok(rows)}/${rows.length} 正常 · 含 ${x.length} 个 X 账号`,
      detail:`信源 ${ok(rows)}/${rows.length} 正常 · 订阅/公告 ${ok(other)}/${other.length} 正常 · X 账号 ${ok(x)}/${x.length} 正常`,
      title:'每个订阅、公告源和 X 账号分别计数；正常指采集状态，X 已登录不等于各账号采集正常。'};
  }
  function paint() {
    document.querySelectorAll('[data-x-search]').forEach(input=>{
      if(input.dataset.bound) return;
      input.dataset.bound='true';input.value=xSearch;
      const filter=()=>{
        xSearch=input.value;
        const query=xSearch.trim().replace(/^@/,'').toLowerCase();
        const panel=input.closest('.x-panel');let visible=0;
        panel.querySelectorAll('[data-x-handle]').forEach(row=>{row.hidden=!row.dataset.xHandle.includes(query);if(!row.hidden) visible++;});
        const empty=panel.querySelector('[data-x-empty]');if(empty) empty.hidden=!query||visible>0;
      };
      input.addEventListener('input',filter);filter();
    });
    document.querySelectorAll('[data-channel-live]').forEach(node => {
      const channel = channels?.find(c => c.id === node.dataset.channelLive);
      let text = channel
        ? `微信（仅自动推送紧急事件） · ${channel.state_label} · 剩余推送 ${Number.isFinite(channel.quota_left) ? channel.quota_left + ' 条' : '未知'} · 最近收到消息：${date(channel.last_received_at)}`
        : channels ? '微信渠道未启用' : '微信状态同步中…';
      if (channel?.expires_at) text += ` · 推送窗口至 ${date(channel.expires_at)}`;
      if (failed) text += ' · 同步失败，显示上次状态';
      if (node.textContent !== text) node.textContent = text;
      node.dataset.stale = String(failed);
      node.title = '每 2 秒同步应用记录的推送余额和最近收消息时间；实际发送仍以微信服务端结果为准。';
    });
    document.querySelectorAll('[data-channel-meter]').forEach(node=>{
      const c=channels?.find(c=>c.id===node.dataset.channelMeter), left=Number.isFinite(c?.quota_left)?c.quota_left:null, limit=Number.isFinite(c?.quota_limit)&&c.quota_limit>0?c.quota_limit:null;
      node.querySelector('[data-quota-number]').textContent=left===null?'—':String(left);
      node.querySelector('[data-quota-limit]').textContent=limit===null?(left===null?'':' 条'):` / ${limit} 条`;
      const bar=node.querySelector('[data-quota-progress]');bar.hidden=left===null||limit===null;
      if(!bar.hidden){bar.max=limit;bar.value=Math.max(0,Math.min(left,limit));bar.setAttribute('aria-valuetext',`剩余 ${left} 条，共 ${limit} 条`);}
      node.classList.toggle('low',left!==null&&limit!==null&&left/limit<=.2);
      node.classList.toggle('empty',left===0);node.dataset.stale=String(failed);
      const received=node.querySelector('[data-channel-received]'), expires=node.querySelector('[data-channel-expires]');
      if(received) received.textContent=date(c?.last_received_at);
      if(expires) expires.textContent=c?.expires_at?date(c.expires_at):'等待建立会话';
      node.querySelector('[data-quota-note]').textContent=failed?'同步失败，显示上次额度':c?(left===null?'尚无可用额度，请检查渠道配置':''):channels?'渠道未启用':'状态同步中…';
    });
    for (const [topic,selector,key,label] of [['plugins','[data-plugin-badge]','name','pluginBadge'],['sources','[data-source-badge]','id','sourceBadge']]) {
      const state=topics.get(topic);
      document.querySelectorAll(selector).forEach(node=>{
        const item=state?.data?.find(row=>row[key]===node.dataset[label]);
        node.textContent=state?.stale ? '同步异常' : item ? (item.status || ({ok:'正常',failing:'异常',unknown:'未知'}[item.state] || item.state)) : '状态未知';
        node.className='bd '+(!state?.stale && item && (item.status==='运行中'||item.state==='ok')?'ok':'bad');
      });
    }
    const pluginState=topics.get('plugins');
    document.querySelectorAll('[data-plugin-switch]').forEach(node=>{
      const p=pluginState?.data?.find(p=>p.name===node.dataset.pluginSwitch);
      if(!p||pluginState.stale) {node.disabled=true;node.title='状态同步失败，恢复同步后可操作';return;}
      node.disabled=node.classList.contains('htmx-request');
      node.classList.toggle('on',p.enabled);
      node.setAttribute('aria-checked',String(p.enabled));
      node.setAttribute('hx-vals',JSON.stringify({enabled:String(!p.enabled)}));
      node.title=p.enabled?'点击停用':'点击开启';
    });
    const source=topics.get('sources'), count=document.getElementById('srcsum');
    const summaries=Array.from(document.querySelectorAll('[data-source-summary]'));
    if(count) summaries.push(count);
    if(source) summaries.forEach(node=>{
      const summary=source.data?describeSources(source.data):null;
      node.textContent=summary?(node.dataset?.sourceSummary==='detail'?summary.detail:summary.compact)+(source.stale?' · 同步失败，显示上次状态':''):'信源状态同步异常';
      node.title=summary?.title||'状态同步失败，尚无可用统计';
      if(node.style) node.style.color=source.stale||source.data?.some(s=>s.state!=='ok')?'var(--text-warning)':'';
    });
    const queue=topics.get('queue');
    document.querySelectorAll('[data-dispatch-queue]').forEach(node=>{
      const total=node.dataset.dispatchQueue==='total';
      const counts=total?queue?.data?.total:queue?.data?.channels?.find(c=>c.channel===node.dataset.dispatchQueue);
      let text=counts?`剩余待发 ${counts.pending} 条 · 当前待发送 ${counts.ready} · 延后重试 ${counts.delayed} · 待人工处理 ${counts.manual}`
        : queue?.data?'剩余待发 0 条 · 待人工处理 0':'队列状态同步中…';
      if(total && queue?.data) text+=` · 等待合并 ${queue.data.merging_events} 个事件`;
      if(queue?.stale) text=queue.data?text+' · 同步失败，显示上次状态':(queue.error||'队列状态暂不可用');
      node.textContent=text;
    });
    document.querySelectorAll('[data-queue-card]').forEach(node=>{
      const state=topics.get('queue'), data=state?.data;
      const counts=data?.channels?.find(c=>c.channel===node.dataset.queueCard);
      node.querySelector('[data-queue-number]').textContent=data?String(counts?.pending||0):'—';
      node.querySelector('[data-queue-detail]').textContent=data?`当前待发送 ${counts?.ready||0} · 延后重试 ${counts?.delayed||0} · 待人工 ${counts?.manual||0}`:'队列状态同步中…';
      if(state?.stale) node.querySelector('[data-queue-detail]').textContent=data?'同步失败，以上为上次记录':state.error||'队列状态暂不可读取';
      node.dataset.stale=String(!!state?.stale);
    });
    if (!channels || failed) return;
    document.querySelectorAll('[data-channel-badge]').forEach(node => {
      const p=pluginState?.data?.find(p=>p.name===node.dataset.channelBadge);
      if(p&&!p.enabled) {node.textContent='已停用';node.className='bd';return;}
      const channel = channels.find(c => c.id === node.dataset.channelBadge);
      if (!channel) return;
      node.textContent = channel.state_label || '未知';
      node.className = 'bd ' + (channel.state === 'ok' ? 'ok' : 'bad');
    });
    document.querySelectorAll('[data-channel-detail]').forEach(node => {
      const channel = channels.find(c => c.id === node.dataset.channelDetail);
      if (channel) node.textContent = channel.detail || '';
    });
    const available = channels.some(c => c.state === 'ok');
    const warning = available ? '' : '没有可用的推送渠道（' + channels.map(c => `${c.name || c.id}：${c.state_label || '读取失败'}`).join('；') + '），消息不会推送出去。';
    document.querySelectorAll('[data-gap="push"],#push-hint').forEach(node => {
      node.hidden = available;
      const label = node.querySelector('[data-channel-warning]') || node.querySelector('span');
      if (label) label.textContent = warning;
    });
    const banner = document.getElementById('first-run');
    if (banner) banner.hidden = !Array.from(banner.children).some(node => !node.hidden);
  }
  async function sync() {
    if (busy || document.hidden) return;
    busy = true;
    try {
      const response = await fetch('/api/v1/status', {cache: 'no-store', signal: AbortSignal.timeout(8000)});
      if (!response.ok) throw new Error('status');
      const data = await response.json();
      for(const topic of ['channels','sources','plugins','model','queue','mcp','updates']) {
        const result=data[topic];
        accept(topic, result && typeof result.ok==='boolean' && (!result.ok || (['model','queue','mcp','updates'].includes(topic) ? result.data && typeof result.data==='object' : Array.isArray(result.data))) ? result : {ok:false,error:'状态响应不完整'}, data.updated_at);
      }
      const channelState=topics.get('channels');
      channels=channelState.data || null; failed=channelState.stale;

    } catch (_) { failed = true; for(const topic of ['channels','sources','plugins','model','queue','mcp','updates']) accept(topic,{ok:false,error:'状态同步失败'},null); }
    finally { busy = false; paint(); }
  }
  window.AppStatus = {paint, sync, subscribe, describeSources, get:topic=>topics.get(topic)};
  setInterval(sync, 2000);
  window.addEventListener('focus', sync);
  document.addEventListener('visibilitychange', sync);
  document.body.addEventListener('htmx:afterSettle', sync);
  sync();
})();

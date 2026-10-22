//! 事件：通知事件先写日志再按游标投递，至少投递一次；ephemeral 事件只投递给在线订阅者。

use std::sync::Arc;
use std::time::Duration;

use contracts::{new_trace_id, now_rfc3339, Event, Intercept};
use plugin_sdk::{EventFn, PluginError};
use serde_json::Value;

use crate::{db, redact, Inner};

const BATCH: i64 = 100;

pub(crate) async fn publish(
    inner: &Arc<Inner>,
    source: &str,
    kind: &str,
    payload: Value,
    trace_id: Option<String>,
) -> Result<i64, PluginError> {
    let mut ev = Event {
        id: 0,
        kind: kind.to_string(),
        trace_id: trace_id.unwrap_or_else(new_trace_id),
        source: source.to_string(),
        created_at: now_rfc3339(),
        payload,
    };
    if inner.is_ephemeral(kind) {
        let subs: Vec<EventFn> = {
            let st = inner.state.lock().unwrap();
            st.subscribers
                .iter()
                .filter(|((k, _), _)| k == kind)
                .map(|(_, f)| f.clone())
                .collect()
        };
        for f in subs {
            let ev = ev.clone();
            tokio::spawn(async move {
                if let Err(e) = f(ev).await {
                    tracing::debug!("ephemeral 事件处理失败：{}", redact(&e.0));
                }
            });
        }
        return Ok(0);
    }
    let id = db::insert_event(inner, &ev).await?;
    ev.id = id;
    inner.notify_for(kind).notify_waiters();
    Ok(id)
}

/// 依次交给拦截者（按插件名），返回最终载荷；被拦下时返回 None。
pub(crate) async fn emit_intercept(
    inner: &Arc<Inner>,
    source: &str,
    kind: &str,
    mut payload: Value,
    trace_id: Option<String>,
) -> Option<Value> {
    let fs: Vec<(String, plugin_sdk::InterceptFn)> = {
        let st = inner.state.lock().unwrap();
        st.interceptors
            .iter()
            .filter(|((k, _), _)| k == kind)
            .map(|((_, p), f)| (p.clone(), f.clone()))
            .collect()
    };
    let trace_id = trace_id.unwrap_or_else(new_trace_id);
    for (plugin, f) in fs {
        let ev = Event {
            id: 0,
            kind: kind.to_string(),
            trace_id: trace_id.clone(),
            source: source.to_string(),
            created_at: now_rfc3339(),
            payload: payload.clone(),
        };
        match f(ev).await {
            Ok(Intercept::Continue(p)) => payload = p,
            Ok(Intercept::Stop) => return None,
            Err(e) => tracing::warn!("插件 {plugin} 拦截 {kind} 失败，跳过：{}", redact(&e.0)),
        }
    }
    Some(payload)
}

/// 一个订阅者的投递循环：失败的事件按退避重试，直到成功才前进游标。
pub(crate) async fn deliver_loop(
    inner: Arc<Inner>,
    plugin: String,
    kind: String,
    f: EventFn,
) -> Result<(), PluginError> {
    let notify = inner.notify_for(&kind);
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let batch = db::fetch_events(&inner, &plugin, &kind, BATCH).await?;
        if batch.is_empty() {
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            continue;
        }
        for ev in batch {
            let id = ev.id;
            let mut delay = Duration::from_secs(1);
            loop {
                match f(ev.clone()).await {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::warn!(
                            "插件 {plugin} 处理事件 {kind}#{id} 失败，{}s 后重试：{}",
                            delay.as_secs(),
                            redact(&e.0)
                        );
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(60));
                    }
                }
            }
            db::set_cursor(&inner, &plugin, &kind, id).await?;
        }
    }
}

/// 每天清理一次过期事件。
pub(crate) async fn cleanup_loop(inner: Arc<Inner>) {
    loop {
        if let Err(e) = db::cleanup(&inner).await {
            tracing::warn!("清理事件日志失败：{}", redact(&e.message));
        }
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    }
}

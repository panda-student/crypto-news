//! 调度轮次：列出信源、判断到期、并发抓取、去重入库、更新状态。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use plugin_sdk::contracts::events::{
    ItemCollected, SourceFailed, SourceRecovered, ITEM_COLLECTED, SOURCE_FAILED, SOURCE_RECOVERED,
};
use plugin_sdk::contracts::source::{truncate_raw, Item, SourceClient, SourceInfo, SERVICE};
use plugin_sdk::contracts::{now_rfc3339, parse_rfc3339};
use plugin_sdk::{Context, PluginError};
use serde::Deserialize;
use serde_json::json;
use tokio::task::JoinSet;

/// 连续失败到这个次数时报警。
const FAIL_THRESHOLD: i64 = 3;

/// 信源是否到期：从未尝试过、时间解析失败或已过 `interval_secs` 都算到期。`now` 为 Unix 秒。
pub fn is_due(last_attempt_at: Option<&str>, interval_secs: u64, now: i64) -> bool {
    match last_attempt_at.and_then(parse_rfc3339) {
        None => true,
        Some(t) => now >= t.unix_timestamp().saturating_add(interval_secs as i64),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct State {
    id: String,
    cursor: Option<String>,
    last_attempt_at: Option<String>,
    consecutive_failures: i64,
}

/// 跑一轮。所有错误只记日志，不中断调度。一轮内同一信源只抓一次，轮次之间串行，
/// 所以同一信源不会同时有两个抓取。
pub async fn run_round(ctx: &Context) {
    let mut all: Vec<(SourceInfo, SourceClient)> = vec![];
    for (provider, svc) in ctx.services(SERVICE) {
        let client = SourceClient(svc);
        match client.list().await {
            Ok(list) => all.extend(list.into_iter().map(|i| (i, client.clone()))),
            Err(e) => tracing::warn!(
                "信源提供者 {provider} list 失败：{}",
                sanitize_error(&e.message)
            ),
        }
    }
    if all.is_empty() {
        return;
    }
    let store = ctx.store();
    for (info, _) in &all {
        let r = store
            .execute(
                "INSERT INTO sources (id, name, interval_secs) VALUES (?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
                 interval_secs = excluded.interval_secs",
                vec![json!(info.id), json!(info.name), json!(info.interval_secs)],
            )
            .await;
        if let Err(e) = r {
            tracing::warn!("写入信源 {} 失败：{}", info.id, e.message);
        }
    }
    let states: HashMap<String, State> = match store
        .query_as::<State>(
            "SELECT id, cursor, last_attempt_at, consecutive_failures FROM sources",
            vec![],
        )
        .await
    {
        Ok(v) => v.into_iter().map(|s| (s.id.clone(), s)).collect(),
        Err(e) => {
            tracing::warn!("读取信源状态失败：{}", e.message);
            return;
        }
    };

    let now = unix_now();
    let mut set = JoinSet::new();
    for (info, client) in all {
        let Some(st) = states.get(&info.id) else {
            continue;
        };
        if !is_due(st.last_attempt_at.as_deref(), info.interval_secs, now) {
            continue;
        }
        // 先记下尝试时间，失败后也要等满间隔再试
        if let Err(e) = store
            .execute(
                "UPDATE sources SET last_attempt_at = ? WHERE id = ?",
                vec![json!(now_rfc3339()), json!(info.id)],
            )
            .await
        {
            tracing::warn!("更新信源 {} 尝试时间失败：{}", info.id, e.message);
            continue;
        }
        let ctx = ctx.clone();
        let cursor = st.cursor.clone();
        let failures = st.consecutive_failures;
        set.spawn(async move {
            if let Err(e) = fetch_one(&ctx, &client, &info.id, cursor, failures).await {
                tracing::warn!("信源 {} 处理失败：{e}", info.id);
            }
        });
    }
    while let Some(r) = set.join_next().await {
        if let Err(e) = r {
            tracing::warn!("抓取任务异常：{e}");
        }
    }
}

async fn fetch_one(
    ctx: &Context,
    client: &SourceClient,
    id: &str,
    cursor: Option<String>,
    prev_failures: i64,
) -> Result<(), PluginError> {
    let store = ctx.store();
    let now = now_rfc3339();
    match client.fetch(id, cursor).await {
        Ok(res) => {
            let mut inserted = 0i64;
            for item in res.items {
                let meta = (
                    item.url.clone(),
                    item.title.clone(),
                    item.published_at.clone(),
                );
                if let Some(item_id) = insert_item(ctx, id, item, &now).await? {
                    inserted += 1;
                    let ev = ItemCollected {
                        item_id,
                        source_id: id.to_string(),
                        url: meta.0,
                        title: meta.1,
                        published_at: meta.2,
                    };
                    publish(ctx, ITEM_COLLECTED, &ev).await;
                }
            }
            store
                .execute(
                    "UPDATE sources SET cursor = COALESCE(?, cursor), last_success_at = ?, \
                     last_error = NULL, consecutive_failures = 0, total_items = total_items + ? \
                     WHERE id = ?",
                    vec![json!(res.cursor), json!(now), json!(inserted), json!(id)],
                )
                .await?;
            if prev_failures >= FAIL_THRESHOLD {
                let ev = SourceRecovered {
                    source_id: id.into(),
                };
                publish(ctx, SOURCE_RECOVERED, &ev).await;
            }
        }
        Err(e) => {
            let msg = sanitize_error(&e.message);
            let n = prev_failures + 1;
            store
                .execute(
                    "UPDATE sources SET consecutive_failures = ?, last_error = ? WHERE id = ?",
                    vec![json!(n), json!(msg), json!(id)],
                )
                .await?;
            tracing::warn!("信源 {id} 抓取失败（连续 {n} 次）：{msg}");
            if n == FAIL_THRESHOLD {
                let ev = SourceFailed {
                    source_id: id.into(),
                    consecutive_failures: n,
                    error: msg,
                };
                publish(ctx, SOURCE_FAILED, &ev).await;
            }
        }
    }
    Ok(())
}

/// 入库一条；已存在时返回 None。
async fn insert_item(
    ctx: &Context,
    source_id: &str,
    item: Item,
    now: &str,
) -> Result<Option<i64>, PluginError> {
    let external_id = item.external_id.filter(|s| !s.is_empty());
    let dedup_key = match &external_id {
        Some(e) => format!("id:{e}"),
        None => format!("url:{}", item.url),
    };
    let r = ctx
        .store()
        .execute(
            "INSERT OR IGNORE INTO raw_items \
             (source_id, dedup_key, external_id, url, title, summary, published_at, \
             collected_at, raw) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                json!(source_id),
                json!(dedup_key),
                json!(external_id),
                json!(item.url),
                json!(item.title),
                json!(item.summary),
                json!(item.published_at),
                json!(now),
                json!(truncate_raw(item.raw)),
            ],
        )
        .await?;
    Ok((r.rows_affected == 1).then_some(r.last_insert_id))
}

async fn publish<T: serde::Serialize>(ctx: &Context, kind: &str, payload: &T) {
    if let Err(e) = ctx.publish(kind, payload).await {
        tracing::warn!("发出事件 {kind} 失败：{e}");
    }
}

/// 去掉错误消息里形如 `?key=value` 的查询参数，避免泄露密钥。
pub fn sanitize_error(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('?') {
        let (head, tail) = rest.split_at(pos);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '）'))
            .unwrap_or(tail.len());
        if tail[..end].contains('=') {
            out.push_str("?…");
            rest = &tail[end..];
        } else {
            out.push('?');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn sanitize_strips_query() {
        assert_eq!(
            sanitize_error("GET https://a.com/x?key=SECRET&b=1 失败"),
            "GET https://a.com/x?… 失败"
        );
        assert_eq!(sanitize_error("真的吗? 不是"), "真的吗? 不是");
    }

    #[test]
    fn due_logic() {
        let t = "2026-01-01T00:00:00Z";
        let base = parse_rfc3339(t).unwrap().unix_timestamp();
        assert!(is_due(None, 60, base));
        assert!(!is_due(Some(t), 60, base + 59));
        assert!(is_due(Some(t), 60, base + 60));
        assert!(is_due(Some("bad"), 60, base));
    }
}

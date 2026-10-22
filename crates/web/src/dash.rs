//! 主界面的数据接口：最近 7 个本机日历日的消息、信源状态与推送记录，由页面脚本在本地统计和下钻。

use std::collections::HashMap;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::dispatch::{DispatchClient, OutboxQuery, OutboxStatus};
use plugin_sdk::contracts::source::SourceStatusClient;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use crate::feed::{load_base, offset_of, svc_note, ts, tz_offset, Filters, Row};
use crate::{check, AppState, Shared};

const DAYS: i64 = 7;

/// X 链接里的账号名（`https://x.com/<name>/status/...`）。
fn x_account(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    let host = host
        .trim_start_matches("www.")
        .trim_start_matches("mobile.");
    if host != "x.com" && host != "twitter.com" {
        return None;
    }
    let name = path.split(['/', '?', '#']).next()?;
    if name.is_empty() || matches!(name, "i" | "home" | "search" | "intent" | "hashtag") {
        return None;
    }
    Some(format!("@{name}"))
}

fn lv_code(r: &Row) -> &'static str {
    match r.level {
        "urgent" => "u",
        "important" => "i",
        "archive" => "x",
        _ => "n",
    }
}

fn dir_code(r: &Row) -> &'static str {
    match r.dir {
        "up" => "up",
        "down" => "dn",
        "neutral" => "nu",
        _ => "",
    }
}

/// 抓取延迟（分钟）：入库时间减发布时间，缺发布时间或为负时没有。
fn lag_minutes(r: &Row) -> Option<i64> {
    let p = ts(r.published_at.as_deref()?)?;
    let c = ts(&r.collected_at)?;
    (c >= p).then(|| (c - p).whole_minutes())
}

async fn pushed_map(st: &AppState) -> (HashMap<i64, Vec<String>>, bool) {
    match DispatchClient(st.ctx.service("dispatch"))
        .outbox(&OutboxQuery {
            status: Some(OutboxStatus::Sent),
            limit: Some(500),
            ..Default::default()
        })
        .await
    {
        Ok(v) => {
            let mut m: HashMap<i64, Vec<String>> = HashMap::new();
            for e in v.into_iter().filter(|e| e.status == OutboxStatus::Sent) {
                for id in e.item_ids {
                    let c = m.entry(id).or_default();
                    if !c.contains(&e.channel) {
                        c.push(e.channel.clone());
                    }
                }
            }
            (m, false)
        }
        Err(_) => (HashMap::new(), true),
    }
}

pub(crate) async fn api_dash(State(st): Shared, headers: HeaderMap) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let tz = tz_offset(&headers);
    let now = OffsetDateTime::now_utc();
    let off = offset_of(tz);
    let today = now.to_offset(off).date();
    let first = today - Duration::days(DAYS - 1);
    let labels: Vec<String> = (0..DAYS)
        .map(|i| {
            let d = first + Duration::days(i);
            format!("{:02}-{:02}", d.month() as u8, d.day())
        })
        .collect();

    let mut error: Option<String> = None;
    let mut degraded = false;
    let rows = match load_base(&st, &Filters::recent(DAYS + 1), tz, now).await {
        Ok(b) => {
            degraded = b.degraded;
            b.rows
        }
        Err(e) => {
            error = Some(svc_note(&e, "消息"));
            Vec::new()
        }
    };
    let (pushed, push_unknown) = pushed_map(&st).await;

    let status = SourceStatusClient(st.ctx.service("source-status"))
        .list()
        .await;
    let (sources, sources_unknown): (Vec<Value>, bool) = match &status {
        Ok(v) => (
            v.iter()
                .map(|s| {
                    json!({
                        "id": s.id, "name": s.name, "state": s.state,
                        "last_error": s.last_error, "last_success_at": s.last_success_at,
                        "total_items": s.total_items, "interval_secs": s.interval_secs,
                    })
                })
                .collect(),
            false,
        ),
        Err(_) => (Vec::new(), true),
    };
    let names: HashMap<&str, &str> = status
        .as_ref()
        .map(|v| v.iter().map(|s| (s.id.as_str(), s.name.as_str())).collect())
        .unwrap_or_default();

    let items: Vec<Value> = rows
        .iter()
        .filter_map(|r| {
            let t = ts(&r.collected_at)?.to_offset(off);
            let d = (t.date() - first).whole_days();
            if !(0..DAYS).contains(&d) {
                return None;
            }
            let p = pushed.get(&r.id);
            Some(json!({
                "id": r.id, "d": d, "h": t.hour(), "lv": lv_code(r),
                "s": r.source_id,
                "sn": names.get(r.source_id.as_str()).copied().unwrap_or(&r.source_id),
                "a": x_account(&r.url),
                "t": r.title, "ot": r.orig_title, "focus":r.focus,"event_key":r.event_key,"published_at":r.published_at,"sz": r.summary_zh, "impact": r.impact, "summary": r.summary,
                "url": r.url, "link": r.link, "time": r.time_label,
                "dir": r.dir_label, "dc": dir_code(r), "assets": r.assets,
                "ca": r.collected_at, "at": r.published_at.as_deref().unwrap_or(&r.collected_at),
                "lag": lag_minutes(r),
                "pushed": p.is_some(), "pushed_to": p.cloned().unwrap_or_default(),
                "unread": r.unread, "starred": r.starred,
            }))
        })
        .collect();
    Json(json!({
        "days": labels, "items": items, "sources": sources,
        "sources_unknown": sources_unknown, "push_unknown": push_unknown,
        "degraded": degraded, "error": error,
        "now": plugin_sdk::contracts::format_rfc3339(now),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct HistoryQuery {
    before_id: Option<i64>,
}

/// Cursor pagination for the continuous stream, independent of the seven-day overview.
pub(crate) async fn api_history(
    State(st): Shared,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    use plugin_sdk::contracts::screen::ScreeningClient;
    use plugin_sdk::contracts::source::{ItemQuery, ItemQueryClient};
    let items = match ItemQueryClient(st.ctx.service("item-query"))
        .list(&ItemQuery {
            before_id: q.before_id,
            limit: Some(100),
            ..Default::default()
        })
        .await
    {
        Ok(v) => v,
        Err(e) => return crate::err_response(e),
    };
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    let screens = match ScreeningClient(st.ctx.service("screening")).get(&ids).await {
        Ok(v) => v,
        Err(e) => return crate::err_response(e),
    };
    let states = match crate::feed::db(&st).states(&ids).await {
        Ok(v) => v,
        Err(e) => return crate::err_response(e),
    };
    let now = OffsetDateTime::now_utc();
    let tz = tz_offset(&headers);
    let first = now.to_offset(offset_of(tz)).date() - Duration::days(DAYS - 1);
    let rows: Vec<Value> = items.iter().map(|it| {
        let r = crate::feed::make_row(it, screens.iter().find(|s|s.item_id==it.id),states.get(&it.id).copied().unwrap_or_default(),now,tz);
        let day = ts(&r.collected_at).map(|t|(t.to_offset(offset_of(tz)).date()-first).whole_days()).unwrap_or(-1);
        json!({"id":r.id,"d":day,"lv":lv_code(&r),"s":r.source_id,"sn":r.source_id,"a":x_account(&r.url),"t":r.title,"ot":r.orig_title,"focus":r.focus,"event_key":r.event_key,"published_at":r.published_at,"sz":r.summary_zh,"impact":r.impact,"summary":r.summary,"url":r.url,"dc":dir_code(&r),"assets":r.assets,"ca":r.collected_at,"at":r.published_at.as_deref().unwrap_or(&r.collected_at),"unread":r.unread,"starred":r.starred})
    }).collect();
    Json(json!({"items":rows,"next":items.last().map(|it|it.id),"more":items.len()==100}))
        .into_response()
}

//! 统计页与设置页。

use std::collections::BTreeMap;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::screen::{Level, ScreeningClient, StatsCount, StatsQuery};
use plugin_sdk::contracts::{format_rfc3339, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use crate::data::PREF_WATCH;
use crate::feed::{db, enc, load_base, tz_offset, Filters};
use crate::pages::Body;
use crate::{check, err_response, render, AppState, Shared};

#[derive(Deserialize, Default)]
pub(crate) struct StatsParams {
    days: Option<String>,
}

fn days_of(p: &StatsParams) -> Result<i64, ServiceError> {
    match p.days.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(7),
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|d| (1..=30).contains(d))
            .ok_or_else(|| ServiceError::invalid_params(format!("无效的天数：{s}"))),
    }
}

fn utc_midnight(t: OffsetDateTime) -> OffsetDateTime {
    t.date().midnight().assume_utc()
}

#[derive(Default, Clone, Copy)]
struct Tally {
    urgent: i64,
    important: i64,
    other: i64,
}

impl Tally {
    fn add(&mut self, c: &StatsCount) {
        match c.level {
            Level::Urgent => self.urgent += c.count,
            Level::Important => self.important += c.count,
            _ => self.other += c.count,
        }
    }
    fn total(&self) -> i64 {
        self.urgent + self.important + self.other
    }
}

fn tally_by_key(v: &[StatsCount]) -> BTreeMap<String, Tally> {
    let mut m: BTreeMap<String, Tally> = BTreeMap::new();
    for c in v {
        m.entry(c.key.clone()).or_default().add(c);
    }
    m
}

fn top(v: &[StatsCount], link: &str, suffix: &str) -> Vec<Value> {
    let mut items: Vec<(String, Tally)> = tally_by_key(v).into_iter().collect();
    items.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then(a.0.cmp(&b.0)));
    let max = items.first().map_or(1, |x| x.1.total()).max(1);
    items
        .into_iter()
        .take(10)
        .map(|(k, t)| {
            json!({
                "key": k,
                "href": format!("/?{link}={}{suffix}", enc(&k)),
                "total": t.total(),
                "urgent": t.urgent,
                "important": t.important,
                "pct": t.total() * 100 / max,
            })
        })
        .collect()
}

/// 服务端画的堆叠柱状图：紧急 / 重要 / 其余，每段链接回动态页。
fn chart(days: &[(String, Tally)], suffix: &str) -> String {
    let n = days.len().max(1) as i64;
    let max = days.iter().map(|d| d.1.total()).max().unwrap_or(0).max(1);
    let (w, h, pad) = (700i64, 180i64, 24i64);
    let slot = w / n;
    let bw = (slot * 6 / 10).max(4);
    let ph = h - pad;
    let mut s = format!(
        "<svg viewBox=\"0 0 {w} {h}\" class=\"chart\" role=\"img\" aria-label=\"每日消息数\">"
    );
    for (i, (day, t)) in days.iter().enumerate() {
        let x = i as i64 * slot + (slot - bw) / 2;
        let mut y = ph;
        for (cls, cnt, lv) in [
            ("c-other", t.other, ""),
            ("c-imp", t.important, "&amp;lv=i"),
            ("c-urg", t.urgent, "&amp;lv=u"),
        ] {
            if cnt == 0 {
                continue;
            }
            let bh = (cnt * (ph - 8) / max).max(2);
            y -= bh;
            s.push_str(&format!(
                "<a href=\"/?day={day}{lv}{suffix}\"><rect class=\"{cls}\" x=\"{x}\" y=\"{y}\" width=\"{bw}\" height=\"{bh}\"><title>{day} {cnt}</title></rect></a>"
            ));
        }
        let label = day.get(5..).unwrap_or(day);
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" class=\"c-lbl\" text-anchor=\"middle\">{label}</text>",
            x + bw / 2,
            h - 8
        ));
    }
    s.push_str("</svg>");
    s
}

async fn stats_data(
    st: &AppState,
    days: i64,
    tz: i32,
) -> Result<serde_json::Map<String, Value>, ServiceError> {
    let now = OffsetDateTime::now_utc();
    let first = utc_midnight(now) - Duration::days(days - 1);
    let stats = ScreeningClient(st.ctx.service("screening"))
        .stats(&StatsQuery {
            since: format_rfc3339(first),
            until: None,
        })
        .await?;
    let by_day = tally_by_key(&stats.by_day);
    let list: Vec<(String, Tally)> = (0..days)
        .map(|i| {
            let d = (first + Duration::days(i)).date();
            let key = format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day());
            let t = by_day.get(&key).copied().unwrap_or_default();
            (key, t)
        })
        .collect();
    let mut all = Tally::default();
    for (_, t) in &list {
        all.urgent += t.urgent;
        all.important += t.important;
        all.other += t.other;
    }
    let suffix = if days == 7 {
        String::new()
    } else {
        format!("&days={days}")
    };
    let f = Filters::recent(days);
    let unread = match load_base(st, &f, tz, now).await {
        Ok(b) => b.rows.iter().filter(|r| r.unread).count() as i64,
        Err(_) => 0,
    };
    let suffix_q = if days == 7 {
        String::new()
    } else {
        format!("?days={days}")
    };
    let mut m = serde_json::Map::new();
    m.insert("days".into(), json!(days));
    m.insert(
        "cards".into(),
        json!([
            { "label": "消息", "n": all.total(), "href": format!("/{suffix_q}") },
            { "label": "紧急", "n": all.urgent, "href": format!("/?lv=u{suffix}") },
            { "label": "重要", "n": all.important, "href": format!("/?lv=i{suffix}") },
            { "label": "未读", "n": unread, "href": format!("/?unread=1{suffix}") },
        ]),
    );
    m.insert("chart".into(), json!(chart(&list, &suffix)));
    m.insert(
        "daily".into(),
        json!(list
            .iter()
            .map(|(d, t)| json!({ "day": d, "urgent": t.urgent, "important": t.important, "other": t.other }))
            .collect::<Vec<_>>()),
    );
    m.insert(
        "assets".into(),
        json!(top(&stats.by_asset, "asset", &suffix)),
    );
    m.insert(
        "sources".into(),
        json!(top(&stats.by_source, "src", &suffix)),
    );
    m.insert("unread".into(), json!(unread));
    Ok(m)
}

/// 统计页：页面脚本从 `/api/v1/dash` 取数后在本地统计，点任意数字或图形跳回 `/` 并带上筛选。
pub(crate) async fn stats_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    render("stats.html", minijinja::context! { nav => "stats" })
}

pub(crate) async fn api_stats(
    State(st): Shared,
    headers: HeaderMap,
    Query(p): Query<StatsParams>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let days = match days_of(&p) {
        Ok(d) => d,
        Err(e) => return err_response(e),
    };
    match stats_data(&st, days, tz_offset(&headers)).await {
        Ok(mut m) => {
            m.remove("chart");
            Json(Value::Object(m)).into_response()
        }
        Err(e) => err_response(e),
    }
}

// ---------------------------------------------------------------- 设置

pub(crate) async fn settings_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let watch = db(&st).watch_assets().await.join(",");
    render(
        "settings.html",
        minijinja::context! { nav => "settings", watch => watch },
    )
}

pub(crate) async fn api_watch_get(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    Json(json!({"watch_assets":db(&st).watch_assets().await})).into_response()
}

#[derive(Deserialize)]
pub(crate) struct WatchForm {
    assets: String,
}

/// 只留大写字母和数字组成的代码，去重，最多 20 个。
fn sanitize_assets(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(|c: char| c == ',' || c == '，' || c.is_whitespace()) {
        let a: String = part
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_uppercase();
        if !a.is_empty() && a.len() <= 10 && !out.contains(&a) {
            out.push(a);
        }
        if out.len() >= 20 {
            break;
        }
    }
    out
}

pub(crate) async fn api_watch_assets(State(st): Shared, body: Body<WatchForm>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let list = match save_watch_assets(&st, &body.v.assets).await {
        Ok(list) => list,
        Err(e) => return err_response(e),
    };
    if body.json {
        return Json(json!({ "watch_assets": list })).into_response();
    }
    render(
        "msg.html",
        minijinja::context! { ok => true, text => format!("已保存：{}", list.join(", ")) },
    )
}

/// Shared GUI and Hub workspace write: one sanitizer and one owned preference table.
pub(crate) async fn save_watch_assets(
    st: &AppState,
    raw: &str,
) -> Result<Vec<String>, ServiceError> {
    let list = sanitize_assets(raw);
    db(st).set_pref(PREF_WATCH, &list.join(",")).await?;
    Ok(list)
}

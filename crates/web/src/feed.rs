//! 动态页：消息流、详情、已读与待跟进、打开原文。

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::screen::{Direction, Level, Screening, ScreeningClient};
use plugin_sdk::contracts::source::{ItemQuery, ItemQueryClient, StoredItem};
use plugin_sdk::contracts::{now_rfc3339, parse_rfc3339, ServiceError, OWNER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::{Date, Duration, Month, OffsetDateTime, UtcOffset};

use crate::data::{Db, PREF_LAST_SEEN};
use crate::opener::is_http;
use crate::{check, err_response, render, AppState, Shared};

const FETCH_LIMIT: u32 = 500;
const DEFAULT_DAYS: i64 = 7;
const MAX_DAYS: i64 = 30;

// ---------------------------------------------------------------- 工具

pub(crate) fn db(st: &AppState) -> Db {
    Db(st.ctx.store())
}

/// 百分号编码（保留 RFC 3986 的非保留字符）。
pub(crate) fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

pub(crate) fn ts(s: &str) -> Option<OffsetDateTime> {
    parse_rfc3339(s)
}

/// 浏览器写在 cookie `tz` 里的时区偏移（分钟，东为正）。
pub(crate) fn tz_offset(h: &HeaderMap) -> i32 {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|kv| kv.trim().strip_prefix("tz="))
        .find_map(|v| v.parse::<i32>().ok())
        .map_or(0, |m| m.clamp(-14 * 60, 14 * 60))
}

pub(crate) fn offset_of(tz: i32) -> UtcOffset {
    UtcOffset::from_whole_seconds(tz * 60).unwrap_or(UtcOffset::UTC)
}

pub(crate) fn wants_json(h: &HeaderMap) -> bool {
    h.get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("application/json"))
}

fn blank(s: &Option<String>) -> Option<String> {
    s.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn svc_note(e: &ServiceError, what: &str) -> String {
    if e.code == ServiceError::UNAVAILABLE {
        format!("{what}服务当前不可用，请稍后刷新。")
    } else {
        format!("读取{what}失败：{}", e.message)
    }
}

fn two(n: u8) -> String {
    format!("{n:02}")
}

/// 本机时区的相对时间："3 分钟前"、"今天 14:02"、"昨天 09:10"、"10-12 08:00"。
pub(crate) fn rel_time(t: OffsetDateTime, now: OffsetDateTime, tz: i32) -> String {
    let secs = (now - t).whole_seconds();
    if (0..60).contains(&secs) {
        return "刚刚".into();
    }
    if (60..3600).contains(&secs) {
        return format!("{} 分钟前", secs / 60);
    }
    let off = offset_of(tz);
    let l = t.to_offset(off);
    let today = now.to_offset(off).date();
    let hm = format!("{}:{}", two(l.hour()), two(l.minute()));
    if l.date() == today {
        format!("今天 {hm}")
    } else if today.previous_day() == Some(l.date()) {
        format!("昨天 {hm}")
    } else {
        format!("{}-{} {hm}", two(l.month() as u8), two(l.day()))
    }
}

fn parse_day(s: &str) -> Option<Date> {
    let mut it = s.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Date::from_calendar_date(y, Month::try_from(m).ok()?, d).ok()
}

// ---------------------------------------------------------------- 筛选

#[derive(Deserialize, Default, Clone)]
pub(crate) struct FeedParams {
    asset: Option<String>,
    lv: Option<String>,
    dir: Option<String>,
    star: Option<String>,
    q: Option<String>,
    src: Option<String>,
    day: Option<String>,
    days: Option<String>,
    since: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lv {
    All,
    /// 紧急和重要
    Ui,
    /// 仅紧急
    U,
    /// 仅重要（统计页跳转用）
    I,
}

impl Lv {
    fn parse(s: &str) -> Option<Lv> {
        match s {
            "all" => Some(Lv::All),
            "ui" => Some(Lv::Ui),
            "u" => Some(Lv::U),
            "i" => Some(Lv::I),
            _ => None,
        }
    }
    fn ok(self, level: Option<Level>) -> bool {
        match self {
            Lv::All => true,
            Lv::Ui => matches!(level, Some(Level::Urgent | Level::Important)),
            Lv::U => level == Some(Level::Urgent),
            Lv::I => level == Some(Level::Important),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Filters {
    asset: Option<String>,
    lv: Lv,
    dir: Option<Direction>,
    star: bool,
    q: Option<String>,
    src: Option<String>,
    day: Option<String>,
    days: i64,
    since: Option<String>,
}

impl Filters {
    /// 无筛选，最近 `days` 天。
    pub(crate) fn recent(days: i64) -> Filters {
        let mut f = Filters::from_params(&FeedParams::default()).expect("默认参数有效");
        f.days = days;
        f
    }

    pub(crate) fn from_params(p: &FeedParams) -> Result<Filters, ServiceError> {
        let lv = match blank(&p.lv) {
            None => Lv::All,
            Some(s) => Lv::parse(&s)
                .ok_or_else(|| ServiceError::invalid_params(format!("无效的级别：{s}")))?,
        };
        let dir = match blank(&p.dir) {
            None => None,
            Some(s) => match s.as_str() {
                "up" => Some(Direction::Up),
                "down" => Some(Direction::Down),
                _ => return Err(ServiceError::invalid_params(format!("无效的方向：{s}"))),
            },
        };
        let day = match blank(&p.day) {
            None => None,
            Some(s) => {
                parse_day(&s)
                    .ok_or_else(|| ServiceError::invalid_params(format!("无效的日期：{s}")))?;
                Some(s)
            }
        };
        let days = match blank(&p.days) {
            None => DEFAULT_DAYS,
            Some(s) => s
                .parse::<i64>()
                .ok()
                .filter(|d| (1..=MAX_DAYS).contains(d))
                .ok_or_else(|| ServiceError::invalid_params(format!("无效的天数：{s}")))?,
        };
        let on = |v: &Option<String>| blank(v).is_some_and(|s| s == "1" || s == "true");
        Ok(Filters {
            asset: blank(&p.asset).map(|a| a.to_uppercase()),
            lv,
            dir,
            star: on(&p.star),
            q: blank(&p.q),
            src: blank(&p.src),
            day,
            days,
            since: blank(&p.since),
        })
    }

    /// 取数时间窗：指定某天时是本机时区的那一天，否则是最近 N 天。
    fn window(&self, tz: i32, now: OffsetDateTime) -> (OffsetDateTime, Option<OffsetDateTime>) {
        if let Some(d) = self.day.as_deref().and_then(parse_day) {
            let start = d.midnight().assume_offset(offset_of(tz));
            return (start, Some(start + Duration::days(1)));
        }
        (now - Duration::days(self.days), None)
    }
}

// ---------------------------------------------------------------- 数据行

#[derive(Serialize, Clone)]
pub(crate) struct Row {
    pub focus: Option<plugin_sdk::contracts::focus::Focus>,
    pub event_key: String,
    pub id: i64,
    /// 显示用标题：有中文标题用中文，否则原标题
    pub title: String,
    /// 原标题；仅在与显示标题不同时给出
    pub orig_title: Option<String>,
    pub summary_zh: Option<String>,
    pub url: String,
    pub link: bool,
    pub source_id: String,
    pub collected_at: String,
    pub published_at: Option<String>,
    pub time_label: String,
    pub level: &'static str,
    pub level_label: &'static str,
    pub hot: bool,
    pub dir: &'static str,
    pub dir_label: &'static str,
    pub assets: Vec<String>,
    pub impact: Option<String>,
    pub unread: bool,
    pub starred: bool,
    pub screened: bool,
    #[serde(skip)]
    pub summary: Option<String>,
    #[serde(skip)]
    pub ct: i64,
    #[serde(skip)]
    pub lvl: Option<Level>,
    #[serde(skip)]
    pub direction: Option<Direction>,
    #[serde(skip)]
    pub search: String,
}

pub(crate) fn make_row(
    item: &StoredItem,
    s: Option<&Screening>,
    st: crate::data::ItemState,
    now: OffsetDateTime,
    tz: i32,
) -> Row {
    let lvl = s.and_then(|s| s.level);
    let (level, level_label) = match lvl {
        Some(Level::Urgent) => ("urgent", "紧急"),
        Some(Level::Important) => ("important", "重要"),
        Some(Level::Archive) => ("archive", "归档"),
        Some(Level::Normal) => ("normal", "一般"),
        None => ("normal", "未分级"),
    };
    let direction = s.and_then(|s| s.direction);
    let (dir, dir_label) = match direction {
        Some(Direction::Up) => ("up", "↑ 利好"),
        Some(Direction::Down) => ("down", "↓ 利空"),
        Some(Direction::Neutral) => ("neutral", "中性"),
        None => ("", ""),
    };
    let assets: Vec<String> = s.map(|s| s.assets.clone()).unwrap_or_default();
    let shown = item
        .published_at
        .as_deref()
        .and_then(ts)
        .or_else(|| ts(&item.collected_at));
    let title_zh = s
        .and_then(|s| s.title_zh.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let summary_zh = s
        .and_then(|s| s.summary_zh.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let search = format!(
        "{} {} {} {}",
        item.title,
        title_zh.unwrap_or(""),
        item.summary.as_deref().unwrap_or(""),
        assets.join(" ")
    )
    .to_lowercase();
    Row {
        focus: s.and_then(|s| s.focus.clone()),
        event_key: plugin_sdk::contracts::focus::identity(
            &item.title,
            item.published_at.as_deref(),
            s.and_then(|s| s.focus.as_ref()),
            item.id,
        ),
        id: item.id,
        title: title_zh.unwrap_or(&item.title).to_string(),
        orig_title: title_zh
            .filter(|t| *t != item.title)
            .map(|_| item.title.clone()),
        summary_zh,
        url: item.url.clone(),
        link: is_http(&item.url),
        source_id: item.source_id.clone(),
        collected_at: item.collected_at.clone(),
        published_at: item.published_at.clone(),
        time_label: shown.map_or_else(String::new, |t| rel_time(t, now, tz)),
        level,
        level_label,
        hot: matches!(lvl, Some(Level::Urgent | Level::Important)),
        dir,
        dir_label,
        assets,
        impact: s.and_then(|s| s.impact.clone()).filter(|s| !s.is_empty()),
        unread: !st.read,
        starred: st.starred,
        screened: lvl.is_some(),
        summary: item.summary.clone().filter(|s| !s.trim().is_empty()),
        ct: ts(&item.collected_at).map_or(0, |t| t.unix_timestamp()),
        lvl,
        direction,
        search,
    }
}

pub(crate) struct Base {
    pub rows: Vec<Row>,
    pub degraded: bool,
}

/// 取一个时间窗内的消息，并合并初筛结果与已读状态。
pub(crate) async fn load_base(
    st: &AppState,
    f: &Filters,
    tz: i32,
    now: OffsetDateTime,
) -> Result<Base, ServiceError> {
    let (since, until) = f.window(tz, now);
    let iq = ItemQueryClient(st.ctx.service("item-query"));
    let items = iq
        .list(&ItemQuery {
            since: Some(plugin_sdk::contracts::format_rfc3339(since)),
            until: until.map(plugin_sdk::contracts::format_rfc3339),
            source_id: f.src.clone(),
            text: f.q.clone(),
            limit: Some(FETCH_LIMIT),
            ..Default::default()
        })
        .await?;
    let items: Vec<StoredItem> = items
        .into_iter()
        .filter(|i| ts(&i.collected_at).is_some_and(|t| t >= since && until.is_none_or(|u| t < u)))
        .collect();
    let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
    let (screens, degraded): (HashMap<i64, Screening>, bool) = if ids.is_empty() {
        (HashMap::new(), false)
    } else {
        match ScreeningClient(st.ctx.service("screening")).get(&ids).await {
            Ok(v) => (v.into_iter().map(|s| (s.item_id, s)).collect(), false),
            Err(e) if e.code == ServiceError::UNAVAILABLE => (HashMap::new(), true),
            Err(e) => return Err(e),
        }
    };
    let states = db(st).states(&ids).await?;
    let mut rows: Vec<Row> = items
        .iter()
        .map(|i| {
            make_row(
                i,
                screens.get(&i.id),
                states.get(&i.id).copied().unwrap_or_default(),
                now,
                tz,
            )
        })
        .collect();
    rows.sort_by(|a, b| b.ct.cmp(&a.ct).then(b.id.cmp(&a.id)));
    Ok(Base { rows, degraded })
}

/// 除未读、待跟进以外的筛选。
fn apply_basic(rows: &[Row], f: &Filters) -> Vec<Row> {
    let q = f.q.as_ref().map(|q| q.to_lowercase());
    rows.iter()
        .filter(|r| f.asset.as_ref().is_none_or(|a| r.assets.contains(a)))
        .filter(|r| f.lv.ok(r.lvl))
        .filter(|r| f.dir.is_none_or(|d| r.direction == Some(d)))
        .filter(|r| f.src.as_ref().is_none_or(|s| &r.source_id == s))
        .filter(|r| q.as_ref().is_none_or(|q| r.search.contains(q.as_str())))
        .cloned()
        .collect()
}

fn ctx_of(m: serde_json::Map<String, Value>) -> minijinja::Value {
    minijinja::Value::from_serialize(Value::Object(m))
}

pub(crate) async fn home(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let last_seen = db(&st).pref(PREF_LAST_SEEN).await.ok().flatten();
    let _ = db(&st).set_pref(PREF_LAST_SEEN, &now_rfc3339()).await;
    let mut m = serde_json::Map::new();
    m.insert("nav".into(), json!("feed"));
    m.insert("last_seen".into(), json!(last_seen.unwrap_or_default()));
    m.insert(
        "watch".into(),
        json!(db(&st).watch_assets().await.join(",")),
    );
    m.insert(
        "gaps".into(),
        Value::Array(crate::settings::setup_gaps(&st).await),
    );
    render("main.html", ctx_of(m))
}

/// 新消息计数（轮询）。
pub(crate) async fn feed_new(
    State(st): Shared,
    headers: HeaderMap,
    Query(p): Query<FeedParams>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let f = match Filters::from_params(&p) {
        Ok(f) => f,
        Err(e) => return err_response(e),
    };
    let tz = tz_offset(&headers);
    let now = OffsetDateTime::now_utc();
    let since = f
        .since
        .clone()
        .unwrap_or_else(|| plugin_sdk::contracts::format_rfc3339(now));
    let since_t = ts(&since).unwrap_or(now).unix_timestamp();
    let n = match load_base(&st, &f, tz, now).await {
        Ok(b) => apply_basic(&b.rows, &f)
            .iter()
            .filter(|r| r.ct > since_t)
            .count(),
        Err(_) => 0,
    };
    Json(json!({ "n": n, "now": plugin_sdk::contracts::format_rfc3339(now) })).into_response()
}

pub(crate) async fn api_read(
    State(st): Shared,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    if let Err(e) = db(&st).mark_read(&[id]).await {
        return err_response(e);
    }
    if wants_json(&headers) {
        Json(json!({ "item_id": id, "read": true })).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

pub(crate) async fn api_read_all(
    State(st): Shared,
    headers: HeaderMap,
    Query(p): Query<FeedParams>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let f = match Filters::from_params(&p) {
        Ok(f) => f,
        Err(e) => return err_response(e),
    };
    let tz = tz_offset(&headers);
    match mark_filtered_read(&st, &f, tz).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => err_response(e),
    }
}

/// The assistant has no browser filters; its tool explicitly targets the default feed.
pub(crate) async fn mark_default_read(st: &AppState) -> Result<Value, ServiceError> {
    let f = Filters::from_params(&FeedParams::default())?;
    mark_filtered_read(st, &f, 0).await
}

async fn mark_filtered_read(st: &AppState, f: &Filters, tz: i32) -> Result<Value, ServiceError> {
    let base = load_base(st, f, tz, OffsetDateTime::now_utc()).await?;
    let ids: Vec<i64> = apply_basic(&base.rows, f)
        .iter()
        .filter(|r| r.unread && (!f.star || r.starred))
        .map(|r| r.id)
        .collect();
    db(st).mark_read(&ids).await?;
    Ok(json!({"marked":ids.len(),"ids":ids}))
}

pub(crate) async fn api_star(State(st): Shared, Path(id): Path<i64>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    match db(&st).toggle_star(id).await {
        Ok(on) => Json(json!({ "item_id": id, "starred": on })).into_response(),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn api_open(
    State(st): Shared,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let item = match ItemQueryClient(st.ctx.service("item-query"))
        .get(&[id])
        .await
    {
        Ok(v) => v.into_iter().next(),
        Err(e) => return err_response(e),
    };
    let Some(item) = item else {
        return (StatusCode::NOT_FOUND, "消息不存在").into_response();
    };
    if !is_http(&item.url) {
        return err_response(ServiceError::invalid_params("只能打开 http 或 https 链接"));
    }
    if let Err(e) = st.opener.open(item.url.trim()) {
        return err_response(ServiceError::internal(e));
    }
    if wants_json(&headers) {
        Json(json!({ "opened": true, "url": item.url })).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

// ---------------------------------------------------------------- 顶栏信源摘要

const _: &str = OWNER;

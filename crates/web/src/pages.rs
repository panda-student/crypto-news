//! 阶段 2、3 页面与接口：规则、消息、渠道、推送记录、日报、密钥、用量。
//!
//! 依赖的服务都是可选的：服务不可用时页面仍返回 200，并显示"该功能未启用"。

use std::collections::HashMap;

use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use plugin_sdk::contracts::channel::{BindInfo, ChannelClient, ChannelInfo, ChannelState};
use plugin_sdk::contracts::dispatch::{
    DispatchClient, OutboxEntry, OutboxKind, OutboxQuery, OutboxStatus,
};
use plugin_sdk::contracts::llm::{LlmClient, UsageQuery};
use plugin_sdk::contracts::rulebook::RulebookClient;
use plugin_sdk::contracts::screen::{Level, ScreenMethod, ScreenStatus, Screening};
use plugin_sdk::contracts::screen::{ScreeningClient, ScreeningQuery};
use plugin_sdk::contracts::secrets::{SecretEntry, SecretsClient};
use plugin_sdk::contracts::source::{ItemQuery, ItemQueryClient, StoredItem};
use plugin_sdk::contracts::{format_rfc3339, now_rfc3339, parse_rfc3339, ServiceError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{check, err_response, render, AppState, Shared};

/// 固定显示的密钥键，即使还没配置。
const KNOWN_SECRETS: [&str; 2] = ["llm.api_key", "wecom-webhook.webhook_key"];
const LIST_LIMIT: i64 = 100;

// ---------------------------------------------------------------- 公共

/// 同时接受 form 和 JSON 的请求体；`json` 记录请求是否为 JSON，决定响应形态。
pub(crate) struct Body<T> {
    pub(crate) v: T,
    pub(crate) json: bool,
}

impl<S, T> FromRequest<S> for Body<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.starts_with("application/json"));
        if json {
            let Json(v) = Json::<T>::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            Ok(Body { v, json })
        } else {
            let Form(v) = Form::<T>::from_request(req, state)
                .await
                .map_err(IntoResponse::into_response)?;
            Ok(Body { v, json })
        }
    }
}

/// 读取失败时页面上的提示条。
pub(crate) fn note(e: &ServiceError, what: &str) -> String {
    if e.code == ServiceError::UNAVAILABLE {
        format!("该功能未启用（{what}服务当前不可用）。")
    } else {
        format!("读取失败：{}", e.message)
    }
}

/// 把成功数据（JSON 对象）或失败提示变成页面上下文。
fn page_ctx(nav: &str, r: Result<Value, ServiceError>, what: &str) -> minijinja::Value {
    let mut m = match r {
        Ok(Value::Object(m)) => m,
        Ok(_) => serde_json::Map::new(),
        Err(e) => {
            let mut m = serde_json::Map::new();
            m.insert("error".into(), Value::String(note(&e, what)));
            m
        }
    };
    m.insert("nav".into(), Value::String(nav.into()));
    minijinja::Value::from_serialize(Value::Object(m))
}

pub(crate) fn msg(ok: bool, text: &str) -> Response {
    render("msg.html", minijinja::context! { ok => ok, text => text })
}

fn json_or<T: Serialize>(v: &T) -> Response {
    Json(v).into_response()
}

pub(crate) fn blank(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_enum<T: DeserializeOwned>(s: &str, what: &str) -> Result<T, ServiceError> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| ServiceError::invalid_params(format!("无效的{what}：{s}")))
}

fn enum_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn http_link(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn preview(text: &str) -> String {
    let one: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let mut it = one.chars();
    let head: String = it.by_ref().take(60).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

// ---------------------------------------------------------------- 规则

async fn rules_data(st: &AppState) -> Result<Value, ServiceError> {
    let rb = RulebookClient(st.ctx.service("rulebook")).get().await?;
    Ok(json!({ "text": rb.text, "updated_at": rb.updated_at }))
}

pub(crate) async fn rules_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "rules.read").await {
        return r;
    }
    render(
        "rules.html",
        page_ctx("rules", rules_data(&st).await, "规则"),
    )
}

pub(crate) async fn api_rules_get(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "rules.read").await {
        return r;
    }
    match rules_data(&st).await {
        Ok(v) => json_or(&v),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct RulesForm {
    text: String,
}

pub(crate) async fn api_rules_set(State(st): Shared, body: Body<RulesForm>) -> Response {
    if let Some(r) = check(&st, "rules.write").await {
        return r;
    }
    match RulebookClient(st.ctx.service("rulebook"))
        .set(&body.v.text)
        .await
    {
        Ok(rb) if body.json => json_or(&rb),
        Ok(rb) => {
            let at = rb.updated_at.unwrap_or_else(now_rfc3339);
            msg(true, &format!("已保存，更新时间 {at}"))
        }
        Err(e) => err_response(e),
    }
}

// ---------------------------------------------------------------- 消息

#[derive(Deserialize, Default)]
pub(crate) struct ItemsParams {
    text: Option<String>,
    level: Option<String>,
}

fn status_label(s: ScreenStatus) -> &'static str {
    match s {
        ScreenStatus::Pending => "初筛中",
        ScreenStatus::Done => "已分级",
        ScreenStatus::Manual => "待人工",
    }
}

fn method_label(m: ScreenMethod) -> &'static str {
    match m {
        ScreenMethod::Llm => "模型",
        ScreenMethod::Rule => "规则",
        ScreenMethod::Keyword => "关键词",
        ScreenMethod::Manual => "人工",
    }
}

fn item_row(item: &StoredItem, s: Option<&Screening>) -> Value {
    json!({
        "id": item.id,
        "source_id": item.source_id,
        "url": item.url,
        "link": http_link(&item.url),
        "title": item.title,
        "summary": item.summary,
        "published_at": item.published_at,
        "collected_at": item.collected_at,
        "level": s.and_then(|s| s.level).map(Level::as_str),
        "level_label": s.and_then(|s| s.level).map(Level::label),
        "reason": s.and_then(|s| s.reason.clone()),
        "impact": s.and_then(|s| s.impact.clone()),
        "score": s.and_then(|s| s.score),
        "status": s.map(|s| enum_str(&s.status)),
        "status_label": s.map(|s| status_label(s.status)).unwrap_or("未初筛"),
        "method_label": s.and_then(|s| s.method).map(method_label),
    })
}

fn matches_text(item: &StoredItem, text: &str) -> bool {
    let t = text.to_lowercase();
    item.title.to_lowercase().contains(&t)
        || item
            .summary
            .as_deref()
            .is_some_and(|s| s.to_lowercase().contains(&t))
}

async fn load_items(st: &AppState, p: &ItemsParams) -> Result<Vec<Value>, ServiceError> {
    let text = blank(p.text.clone());
    let level = blank(p.level.clone());
    let iq = ItemQueryClient(st.ctx.service("item-query"));
    let sc = ScreeningClient(st.ctx.service("screening"));
    if let Some(level) = level {
        let level = Level::parse(&level)
            .ok_or_else(|| ServiceError::invalid_params(format!("无效的级别：{level}")))?;
        let list = sc
            .list(&ScreeningQuery {
                level: Some(level),
                limit: Some(LIST_LIMIT),
                ..Default::default()
            })
            .await?;
        let ids: Vec<i64> = list.iter().map(|s| s.item_id).collect();
        let items: HashMap<i64, StoredItem> = if ids.is_empty() {
            HashMap::new()
        } else {
            iq.get(&ids).await?.into_iter().map(|i| (i.id, i)).collect()
        };
        Ok(list
            .iter()
            .filter_map(|s| items.get(&s.item_id).map(|i| (i, s)))
            .filter(|(i, _)| text.as_deref().is_none_or(|t| matches_text(i, t)))
            .map(|(i, s)| item_row(i, Some(s)))
            .collect())
    } else {
        let items = iq
            .list(&ItemQuery {
                text,
                limit: Some(LIST_LIMIT as u32),
                ..Default::default()
            })
            .await?;
        let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
        let screens: HashMap<i64, Screening> = if ids.is_empty() {
            HashMap::new()
        } else {
            sc.get(&ids)
                .await?
                .into_iter()
                .map(|s| (s.item_id, s))
                .collect()
        };
        Ok(items
            .iter()
            .map(|i| item_row(i, screens.get(&i.id)))
            .collect())
    }
}

pub(crate) async fn items_page(State(st): Shared, Query(p): Query<ItemsParams>) -> Response {
    if let Some(r) = check(&st, "items.read").await {
        return r;
    }
    let rows = load_items(&st, &p).await.map(|rows| {
        json!({
            "rows": rows,
            "text": p.text.clone().unwrap_or_default(),
            "level": p.level.clone().unwrap_or_default(),
        })
    });
    let ctx = match rows {
        Ok(v) => page_ctx("items", Ok(v), ""),
        Err(e) if e.code == ServiceError::INVALID_PARAMS => {
            return err_response(e);
        }
        Err(e) => {
            // 保留筛选条件，方便用户修改
            let what = if e.message.contains("screening") {
                "初筛"
            } else {
                "消息查询"
            };
            let mut m = serde_json::Map::new();
            m.insert("error".into(), Value::String(note(&e, what)));
            m.insert("nav".into(), Value::String("items".into()));
            m.insert("text".into(), Value::String(p.text.unwrap_or_default()));
            m.insert("level".into(), Value::String(p.level.unwrap_or_default()));
            minijinja::Value::from_serialize(Value::Object(m))
        }
    };
    render("items.html", ctx)
}

pub(crate) async fn api_items(State(st): Shared, Query(p): Query<ItemsParams>) -> Response {
    if let Some(r) = check(&st, "items.read").await {
        return r;
    }
    match load_items(&st, &p).await {
        Ok(rows) => json_or(&json!({ "items": rows })),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct LevelForm {
    level: String,
}

pub(crate) async fn api_item_level(
    State(st): Shared,
    Path(id): Path<i64>,
    body: Body<LevelForm>,
) -> Response {
    if let Some(r) = check(&st, "items.write").await {
        return r;
    }
    let Some(level) = Level::parse(&body.v.level) else {
        return err_response(ServiceError::invalid_params("无效的级别"));
    };
    let screening = match ScreeningClient(st.ctx.service("screening"))
        .review(id, level)
        .await
    {
        Ok(s) => s,
        Err(e) => return err_response(e),
    };
    if body.json {
        return json_or(&screening);
    }
    match ItemQueryClient(st.ctx.service("item-query"))
        .get(&[id])
        .await
    {
        Ok(items) => match items.first() {
            Some(item) => render(
                "item_row.html",
                minijinja::context! { r => minijinja::Value::from_serialize(item_row(item, Some(&screening))) },
            ),
            None => (StatusCode::NOT_FOUND, "消息不存在").into_response(),
        },
        Err(e) => err_response(e),
    }
}

// ---------------------------------------------------------------- 渠道

pub(crate) fn channel_state_label(id: &str, state: ChannelState) -> &'static str {
    if id == "wecom-webhook" && state == ChannelState::Expired {
        "链接无效"
    } else {
        state_label(state)
    }
}

pub(crate) fn state_label(s: ChannelState) -> &'static str {
    match s {
        ChannelState::Ok => "正常",
        ChannelState::Unbound => "未绑定",
        ChannelState::Expired => "已过期",
        ChannelState::QuotaExhausted => "额度用完",
        ChannelState::Offline => "离线",
    }
}

fn channel_view(id: &str, info: Result<ChannelInfo, ServiceError>) -> Value {
    match info {
        Ok(i) => {
            let mut v = serde_json::to_value(&i).unwrap_or_else(|_| json!({}));
            v["id"] = json!(id);
            v["state_label"] = json!(channel_state_label(id, i.state));
            v["format_label"] = json!(match i.format {
                plugin_sdk::contracts::channel::TextFormat::Markdown => "markdown",
                plugin_sdk::contracts::channel::TextFormat::Plain => "纯文本",
            });
            v
        }
        Err(e) => json!({ "id": id, "error": format!("读取渠道信息失败：{}", e.message) }),
    }
}

async fn channels_data(st: &AppState) -> Result<Value, ServiceError> {
    let mut list = st.ctx.services("channel");
    if list.is_empty() {
        return Err(ServiceError::unavailable("channel"));
    }
    list.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (id, svc) in list {
        out.push(channel_view(&id, ChannelClient(svc).info().await));
    }
    Ok(json!({ "channels": out }))
}

pub(crate) async fn channels_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "channels.read").await {
        return r;
    }
    render(
        "channels.html",
        page_ctx("channels", channels_data(&st).await, "渠道"),
    )
}

pub(crate) async fn api_channels(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "channels.read").await {
        return r;
    }
    match channels_data(&st).await {
        Ok(v) => json_or(&v),
        Err(e) => err_response(e),
    }
}

fn find_channel(st: &AppState, id: &str) -> Option<ChannelClient> {
    st.ctx
        .services("channel")
        .into_iter()
        .find(|(p, _)| p == id)
        .map(|(_, s)| ChannelClient(s))
}

fn bind_fragment(id: &str, b: &BindInfo) -> Response {
    let qrcode_image = b
        .qrcode_image
        .clone()
        .filter(|s| s.starts_with("data:image/"));
    let qrcode_link = b.qrcode.as_deref().is_some_and(http_link);
    let qrcode_svg = if qrcode_image.is_none() {
        b.qrcode.as_deref().and_then(crate::qr::qr_svg)
    } else {
        None
    };
    render(
        "bind.html",
        minijinja::context! {
            id => id,
            ok => b.state == ChannelState::Ok,
            state_label => state_label(b.state),
            qrcode_image => qrcode_image,
            qrcode_svg => qrcode_svg,
            qrcode => b.qrcode,
            qrcode_link => qrcode_link,
            detail => b.detail,
        },
    )
}

pub(crate) async fn api_channel_test(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "channels.write").await {
        return r;
    }
    match DispatchClient(st.ctx.service("dispatch"))
        .test_send(&id)
        .await
    {
        Ok(e) => msg(true, &format!("测试消息已加入发送队列（#{}）", e.id)),
        Err(e) if e.code == ServiceError::UNAVAILABLE => msg(
            false,
            "测试消息未入队：推送调度未运行或暂不可用，请打开上方“发送队列”旁的推送调度开关并确认运行状态后重试。",
        ),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn api_channel_bind(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "channels.write").await {
        return r;
    }
    let Some(client) = find_channel(&st, &id) else {
        return (StatusCode::NOT_FOUND, "渠道不存在").into_response();
    };
    match client.bind().await {
        Ok(b) => bind_fragment(&id, &b),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn channel_bind_status(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "channels.read").await {
        return r;
    }
    let Some(client) = find_channel(&st, &id) else {
        return (StatusCode::NOT_FOUND, "渠道不存在").into_response();
    };
    match client.bind_status().await {
        Ok(b) => bind_fragment(&id, &b),
        Err(e) => err_response(e),
    }
}

// ---------------------------------------------------------------- 推送记录

#[derive(Deserialize, Default)]
pub(crate) struct OutboxParams {
    status: Option<String>,
}

fn outbox_status_label(s: OutboxStatus) -> &'static str {
    match s {
        OutboxStatus::Pending => "等待发送",
        OutboxStatus::Sent => "已发送",
        OutboxStatus::Manual => "转人工",
        OutboxStatus::Dropped => "已丢弃",
    }
}

fn outbox_kind_label(k: OutboxKind) -> &'static str {
    match k {
        OutboxKind::Flash => "快讯",
        OutboxKind::Digest => "日报",
        OutboxKind::Alert => "提醒",
    }
}

fn outbox_row(e: &OutboxEntry) -> Value {
    json!({
        "id": e.id,
        "kind": enum_str(&e.kind),
        "kind_label": outbox_kind_label(e.kind),
        "channel": e.channel,
        "status": enum_str(&e.status),
        "status_label": outbox_status_label(e.status),
        "attempts": e.attempts,
        "batch": e.batch,
        "preview": preview(&e.text),
        "text": e.text,
        "last_error": e.last_error,
        "created_at": e.created_at,
        "sent_at": e.sent_at,
        "can_retry": matches!(e.status, OutboxStatus::Manual | OutboxStatus::Dropped),
    })
}

async fn outbox_rows(st: &AppState, p: &OutboxParams) -> Result<Vec<Value>, ServiceError> {
    let status = blank(p.status.clone())
        .map(|s| parse_enum::<OutboxStatus>(&s, "状态"))
        .transpose()?;
    let entries = DispatchClient(st.ctx.service("dispatch"))
        .outbox(&OutboxQuery {
            status,
            kind: None,
            limit: Some(LIST_LIMIT),
        })
        .await?;
    Ok(entries.iter().map(outbox_row).collect())
}

async fn outbox_ctx(st: &AppState, p: &OutboxParams) -> minijinja::Value {
    let status = blank(p.status.clone()).unwrap_or_default();
    let r = outbox_rows(st, p)
        .await
        .map(|rows| json!({ "rows": rows, "status": status }));
    let mut ctx = match r {
        Ok(Value::Object(m)) => m,
        Ok(_) => serde_json::Map::new(),
        Err(e) => {
            let mut m = serde_json::Map::new();
            m.insert("error".into(), Value::String(note(&e, "推送")));
            m.insert("status".into(), Value::String(status));
            m
        }
    };
    ctx.insert("nav".into(), Value::String("outbox".into()));
    minijinja::Value::from_serialize(Value::Object(ctx))
}

pub(crate) async fn outbox_page(State(st): Shared, Query(p): Query<OutboxParams>) -> Response {
    if let Some(r) = check(&st, "outbox.read").await {
        return r;
    }
    render("outbox.html", outbox_ctx(&st, &p).await)
}

pub(crate) async fn outbox_table(State(st): Shared, Query(p): Query<OutboxParams>) -> Response {
    if let Some(r) = check(&st, "outbox.read").await {
        return r;
    }
    render("outbox_table.html", outbox_ctx(&st, &p).await)
}

pub(crate) async fn api_outbox(State(st): Shared, Query(p): Query<OutboxParams>) -> Response {
    if let Some(r) = check(&st, "outbox.read").await {
        return r;
    }
    match outbox_rows(&st, &p).await {
        Ok(rows) => json_or(&json!({ "entries": rows })),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn api_outbox_retry(
    State(st): Shared,
    Path(id): Path<i64>,
    Query(p): Query<OutboxParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(r) = check(&st, "outbox.write").await {
        return r;
    }
    let entry = match DispatchClient(st.ctx.service("dispatch")).retry(id).await {
        Ok(e) => e,
        Err(e) => return err_response(e),
    };
    // htmx 请求回表格片段，其他调用方回 JSON
    if headers.contains_key("hx-request") {
        render("outbox_table.html", outbox_ctx(&st, &p).await)
    } else {
        json_or(&outbox_row(&entry))
    }
}

// ---------------------------------------------------------------- 日报

async fn digests_data(st: &AppState) -> Result<Value, ServiceError> {
    let d = DispatchClient(st.ctx.service("dispatch"))
        .digests(30)
        .await?;
    Ok(json!({ "digests": d }))
}

pub(crate) async fn digests_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "digests.read").await {
        return r;
    }
    render(
        "digests.html",
        page_ctx("digests", digests_data(&st).await, "日报"),
    )
}

pub(crate) async fn api_digests(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "digests.read").await {
        return r;
    }
    match digests_data(&st).await {
        Ok(v) => json_or(&v),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn digests_preview(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "digests.read").await {
        return r;
    }
    match DispatchClient(st.ctx.service("dispatch"))
        .preview_digest()
        .await
    {
        Ok(p) => render("digest_preview.html", minijinja::Value::from_serialize(&p)),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn api_digests_preview(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "digests.read").await {
        return r;
    }
    match DispatchClient(st.ctx.service("dispatch"))
        .preview_digest()
        .await
    {
        Ok(p) => json_or(&p),
        Err(e) => err_response(e),
    }
}

// ---------------------------------------------------------------- 密钥

fn secret_view(e: &SecretEntry) -> Value {
    // 只暴露 key、configured、updated_at，永远不带值
    json!({ "key": e.key, "configured": e.configured, "updated_at": e.updated_at })
}

async fn secret_entries(st: &AppState) -> Result<Vec<SecretEntry>, ServiceError> {
    let mut list = SecretsClient(st.ctx.service("secrets")).list().await?;
    for k in KNOWN_SECRETS.iter().rev() {
        if !list.iter().any(|e| e.key == *k) {
            list.insert(
                0,
                SecretEntry {
                    key: (*k).to_string(),
                    configured: false,
                    updated_at: None,
                },
            );
        }
    }
    list.sort_by_key(|e| {
        KNOWN_SECRETS
            .iter()
            .position(|k| *k == e.key)
            .unwrap_or(KNOWN_SECRETS.len())
    });
    Ok(list)
}

async fn secrets_data(st: &AppState) -> Result<Value, ServiceError> {
    let list = secret_entries(st).await?;
    Ok(json!({ "secrets": list.iter().map(secret_view).collect::<Vec<_>>() }))
}

pub(crate) async fn secrets_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "secrets.read").await {
        return r;
    }
    render(
        "secrets.html",
        page_ctx("secrets", secrets_data(&st).await, "密钥"),
    )
}

pub(crate) async fn api_secrets(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "secrets.read").await {
        return r;
    }
    match secrets_data(&st).await {
        Ok(v) => json_or(&v),
        Err(e) => err_response(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct SecretForm {
    key: String,
    value: String,
}

#[derive(Deserialize)]
pub(crate) struct SecretKeyForm {
    key: String,
}

/// 写入后回显该键的当前状态行（不含值）。
async fn secret_result(st: &AppState, key: &str, json_out: bool) -> Response {
    let entry = match secret_entries(st).await {
        Ok(l) => l.into_iter().find(|e| e.key == key),
        Err(e) => return err_response(e),
    };
    let entry = entry.unwrap_or(SecretEntry {
        key: key.to_string(),
        configured: false,
        updated_at: None,
    });
    let view = secret_view(&entry);
    if json_out {
        json_or(&view)
    } else {
        render(
            "secret_row.html",
            minijinja::context! { s => minijinja::Value::from_serialize(&view) },
        )
    }
}

pub(crate) async fn api_secret_set(State(st): Shared, body: Body<SecretForm>) -> Response {
    if let Some(r) = check(&st, "secrets.write").await {
        return r;
    }
    let key = body.v.key.trim();
    if key.is_empty() {
        return err_response(ServiceError::invalid_params("密钥键不能为空"));
    }
    if body.v.value.is_empty() {
        return err_response(ServiceError::invalid_params("密钥值不能为空"));
    }
    if let Err(e) = SecretsClient(st.ctx.service("secrets"))
        .set(key, &body.v.value)
        .await
    {
        return err_response(e);
    }
    secret_result(&st, key, body.json).await
}

pub(crate) async fn api_secret_delete(State(st): Shared, body: Body<SecretKeyForm>) -> Response {
    if let Some(r) = check(&st, "secrets.write").await {
        return r;
    }
    let key = body.v.key.trim();
    if key.is_empty() {
        return err_response(ServiceError::invalid_params("密钥键不能为空"));
    }
    if let Err(e) = SecretsClient(st.ctx.service("secrets")).delete(key).await {
        return err_response(e);
    }
    secret_result(&st, key, body.json).await
}

// ---------------------------------------------------------------- 用量

/// 30 天前的 UTC 时刻（RFC3339）。
fn thirty_days_ago() -> Option<String> {
    let now = parse_rfc3339(&now_rfc3339())?;
    let mut d = now.date();
    for _ in 0..30 {
        d = d.previous_day()?;
    }
    Some(format_rfc3339(now.replace_date(d)))
}

async fn usage_data(st: &AppState) -> Result<Value, ServiceError> {
    let llm = LlmClient(st.ctx.service("llm"));
    let rows = llm
        .usage(&UsageQuery {
            since: thirty_days_ago(),
            until: None,
        })
        .await?;
    let ready = llm.ready().await.unwrap_or(false);
    Ok(json!({ "ready": ready, "rows": rows }))
}

pub(crate) async fn usage_page(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "usage.read").await {
        return r;
    }
    render(
        "usage.html",
        page_ctx("usage", usage_data(&st).await, "模型"),
    )
}

pub(crate) async fn api_usage(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "usage.read").await {
        return r;
    }
    match usage_data(&st).await {
        Ok(v) => json_or(&v),
        Err(e) => err_response(e),
    }
}

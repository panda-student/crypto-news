//! 设置页：模型服务、信源、推送、插件、系统五个分区，每个分区是一个由 htmx 加载的片段。
//!
//! 页面外壳在 `stats::settings_page`，这里提供分区片段、写操作和插件管理接口。
//! 密钥（模型服务 API Key、企业微信 Webhook）只收不回显，页面上只显示"已设置/未设置"。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use plugin_sdk::contracts::channel::{ChannelClient, ChannelInfo, ChannelState};
use plugin_sdk::contracts::hub_admin::{app_keys, HubAdminClient, PluginInfo};
use plugin_sdk::contracts::llm::{
    provider_kinds, purposes, Assignment, LlmClient, ProviderInfo, ProviderInput, ProviderList,
    TestRequest, UsageQuery, UsageRow,
};
use plugin_sdk::contracts::secrets::SecretsClient;
use plugin_sdk::contracts::source::{
    FeedAdminClient, FeedInfo, FeedInput, SourceStatus, SourceStatusClient,
};
use plugin_sdk::contracts::x::{XAccount, XAdminClient, XStatus};
use plugin_sdk::contracts::{format_rfc3339, now_rfc3339, parse_rfc3339, ServiceError};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::{check, err_response, pages, render, AppState, Shared};

pub(crate) const TEMPLATES: &[(&str, &str)] = &[
    (
        "settings_panel.html",
        include_str!("../templates/settings_panel.html"),
    ),
    (
        "settings_mcp.html",
        include_str!("../templates/settings_mcp.html"),
    ),
    ("settings.html", include_str!("../templates/settings.html")),
    (
        "settings_icons.html",
        include_str!("../templates/settings_icons.html"),
    ),
    (
        "settings_llm.html",
        include_str!("../templates/settings_llm.html"),
    ),
    (
        "settings_sources.html",
        include_str!("../templates/settings_sources.html"),
    ),
    (
        "settings_push.html",
        include_str!("../templates/settings_push.html"),
    ),
    (
        "settings_plugins.html",
        include_str!("../templates/settings_plugins.html"),
    ),
    (
        "settings_plugin_cfg.html",
        include_str!("../templates/settings_plugin_cfg.html"),
    ),
    (
        "settings_system.html",
        include_str!("../templates/settings_system.html"),
    ),
    (
        "settings_network.html",
        include_str!("../templates/settings_network.html"),
    ),
];

const WECOM_KEY: &str = "wecom-webhook.webhook_key";
const DEFAULT_QUIET: &str = "23:00-07:00";
const DEFAULT_PROXY_TEST: &str = "https://www.gstatic.com/generate_204";

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/llm", get(llm_get))
        .route("/settings/llm/save", post(llm_save))
        .route("/settings/llm/assign", post(llm_assign))
        .route("/settings/llm/{id}/test", post(llm_test))
        .route("/settings/llm/{id}/default", post(llm_default))
        .route("/settings/llm/{id}/delete", post(llm_delete))
        .route("/settings/sources", get(sources_get))
        .route("/settings/x/enabled", post(x_enabled))
        .route("/settings/x/open", post(x_open))
        .route("/settings/sources/save", post(sources_save))
        .route("/settings/sources/{id}/pause", post(sources_pause))
        .route("/settings/sources/{id}/delete", post(sources_delete))
        .route("/settings/sources/{id}/retry", post(sources_retry))
        .route("/settings/x/login", post(x_login))
        .route("/settings/x/add", post(x_add))
        .route("/settings/x/{handle}/pause", post(x_pause))
        .route("/settings/x/{handle}/delete", post(x_delete))
        .route("/settings/push", get(push_get))
        .route(
            "/settings/push/plugins/{name}/enabled",
            post(push_plugin_enabled),
        )
        .route("/settings/push/webhook", post(push_webhook))
        .route("/settings/push/rules", post(push_rules))
        .route("/settings/plugins", get(plugins_get))
        .route(
            "/settings/plugins/{name}/config",
            get(plugin_config_get).post(plugin_config_save),
        )
        .route("/settings/plugins/{name}/restart", post(plugin_restart))
        .route("/settings/system", get(system_get))
        .route("/settings/system/update", post(update_action))
        .route("/settings/system/auto-update", post(auto_update))
        .route("/settings/network", get(network_get))
        .route("/settings/network/proxy", post(proxy_save))
        .route("/settings/network/proxy/test", post(proxy_test))
        .route("/settings/system/proxy", post(proxy_save))
        .route("/settings/system/proxy/test", post(proxy_test))
        .route("/settings/system/app", post(app_save))
        .route(
            "/plugins",
            get(|| async { Redirect::to("/settings#s-plg") }),
        )
        .route(
            "/sources",
            get(|| async { Redirect::to("/settings#s-src") }),
        )
        .route("/api/v1/plugins", get(api_plugins))
        .route("/api/v1/sources", get(api_sources))
        .route("/api/v1/plugins/{name}/grant", post(plugin_grant))
        .route("/api/v1/plugins/{name}/enabled", post(plugin_enabled))
}

// ---------------------------------------------------------------- 公共

type Flash = Option<(bool, String)>;

fn ok_flash(t: &str) -> Flash {
    Some((true, t.to_string()))
}

fn bad_flash(e: &ServiceError) -> Flash {
    Some((false, e.message.clone()))
}

fn put_flash(m: &mut Value, flash: Flash) {
    if let Some((ok, t)) = flash {
        m["flash"] = json!(t);
        m["flash_ok"] = json!(ok);
    }
}

fn page(name: &str, v: Value) -> Response {
    render(name, minijinja::Value::from_serialize(&v))
}

/// 放进 URL 路径和查询里的 id：只保留字母数字和 `-_.`，其余百分号编码。
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 放进 DOM id 的形式。
fn dom(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn trimmed(s: &str) -> String {
    s.trim().to_string()
}

fn http_url(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://")) && s.len() > 8 && !s.contains(' ')
}

async fn guard_view(st: &AppState) -> Option<Response> {
    check(st, "web.view").await
}

async fn guard_manage(st: &AppState) -> Option<Response> {
    check(st, "web.manage").await
}

fn admin(st: &AppState) -> HubAdminClient {
    HubAdminClient(st.ctx.service("hub-admin"))
}

#[derive(Deserialize, Default)]
pub(crate) struct EditQuery {
    edit: Option<String>,
    hook: Option<String>,
}

// ---------------------------------------------------------------- 模型服务

fn kind_label(k: &str) -> &'static str {
    if k == provider_kinds::ANTHROPIC {
        "Anthropic"
    } else {
        "OpenAI 兼容"
    }
}

fn tokens_text(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn month_start() -> Option<String> {
    let now = parse_rfc3339(&now_rfc3339())?;
    let d = now.date().replace_day(1).ok()?;
    Some(format_rfc3339(
        now.replace_date(d).replace_time(time::Time::MIDNIGHT),
    ))
}

fn usage_view(rows: &[UsageRow]) -> (Vec<Value>, String) {
    let mut by: Vec<(String, u64, u64, u64)> = Vec::new();
    for r in rows {
        let label = purposes::ALL
            .iter()
            .find(|(k, _)| *k == r.purpose)
            .map(|(_, l)| (*l).to_string())
            .unwrap_or_else(|| r.purpose.clone());
        let tok = r.input_tokens + r.output_tokens;
        match by.iter_mut().find(|e| e.0 == label) {
            Some(e) => {
                e.1 += r.calls;
                e.2 += tok;
                e.3 += r.failures;
            }
            None => by.push((label, r.calls, tok, r.failures)),
        }
    }
    let max = by.iter().map(|e| e.2).max().unwrap_or(0).max(1);
    let calls: u64 = by.iter().map(|e| e.1).sum();
    let tok: u64 = by.iter().map(|e| e.2).sum();
    let list = by
        .iter()
        .map(|(label, c, t, f)| {
            let pct = if *t == 0 { 0 } else { (t * 100 / max).max(3) };
            json!({"label": label, "pct": pct, "failures": f,
                "text": format!("{c} 次 · {}", tokens_text(*t))})
        })
        .collect();
    (list, format!("共 {calls} 次 · {} tokens", tokens_text(tok)))
}

fn provider_view(p: &ProviderInfo, editing: bool) -> Value {
    let (cls, label) = match &p.last_test {
        Some(t) if t.ok => ("ok", "可用"),
        Some(_) => ("bad", "失败"),
        None => ("wa", "未测试"),
    };
    json!({
        "id": p.id, "eid": enc(&p.id), "dom": dom(&p.id), "name": p.name,
        "kind": p.kind, "kind_label": kind_label(&p.kind), "base_url": p.base_url,
        "models": p.models.join(", "), "has_key": p.has_key, "is_default": p.is_default,
        "test_cls": cls, "test_label": label, "editing": editing,
    })
}

/// 模型分区顶部的就绪提示，判断标准与主界面横幅一致。就绪且最近测试没失败时为 None。
fn llm_hint(list: &ProviderList) -> Option<Value> {
    let hint = |cls: &str, text: String, act: &str, target: &str, label: &str| {
        Some(json!({"cls": cls, "text": text, "act": act, "target": target, "label": label}))
    };
    if list.providers.is_empty() {
        return hint(
            "bad",
            "还没有模型服务，主界面的筛选和摘要用不了。".into(),
            "edit",
            "new",
            "添加模型服务",
        );
    }
    let Some(d) = list.providers.iter().find(|p| p.is_default) else {
        let first = &list.providers[0];
        return hint(
            "bad",
            "没有默认服务，请选一个设为默认。".into(),
            "default",
            &enc(&first.id),
            &format!("把“{}”设为默认", first.name),
        );
    };
    if d.models.is_empty() {
        return hint(
            "bad",
            format!("默认服务“{}”没有可用模型，请填写模型名。", d.name),
            "edit",
            &enc(&d.id),
            "填写模型",
        );
    }
    if !d.has_key {
        return hint(
            "bad",
            format!("默认服务“{}”的密钥未设置。", d.name),
            "edit",
            &enc(&d.id),
            "设置密钥",
        );
    }
    match &d.last_test {
        Some(t) if !t.ok => hint(
            "wa",
            format!("默认服务“{}”最近一次测试失败：{}", d.name, t.message),
            "edit",
            &enc(&d.id),
            "检查配置",
        ),
        _ => None,
    }
}

/// 主界面横幅：列出没就绪的项，文案与设置页的就绪提示同一套判断。
/// 每项 `{target: "llm"|"push", text, link}`；都就绪时为空。
pub(crate) async fn setup_gaps(st: &AppState) -> Vec<Value> {
    let mut gaps = Vec::new();
    let llm = LlmClient(st.ctx.service("llm"));
    match llm.providers().await {
        Ok(list) => {
            if let Some(h) = llm_hint(&list).filter(|h| h["cls"] == "bad") {
                gaps.push(json!({
                    "target": "llm", "link": "去设置模型服务",
                    "text": format!("模型服务未就绪：{}", h["text"].as_str().unwrap_or("")),
                }));
            }
        }
        Err(e) => gaps.push(json!({
            "target": "llm", "link": "去设置模型服务",
            "text": format!("模型服务未就绪：{}", pages::note(&e, "模型")),
        })),
    }
    let rules = Rules::from_config(&json!({}));
    let mut list = st.ctx.services("channel");
    list.sort_by(|a, b| a.0.cmp(&b.0));
    let mut channels = Vec::new();
    for (id, svc) in list {
        let info = ChannelClient(svc).info().await;
        channels.push(channel_view(&id, info, &rules, false, false));
    }
    let h = push_hint(&channels);
    if let Some(t) = h["text"].as_str() {
        gaps.push(json!({"target": "push", "text": t, "link": "去设置推送渠道"}));
    }
    gaps
}

async fn llm_ctx(st: &AppState, edit: Option<&str>, flash: Flash) -> Value {
    let llm = LlmClient(st.ctx.service("llm"));
    let mut m = json!({
        "blank": {"id": "", "name": "", "kind": "openai", "base_url": "", "models": "", "has_key": false},
        "new_form": edit == Some("new"),
    });
    put_flash(&mut m, flash);
    let list = match llm.providers().await {
        Ok(l) => l,
        Err(e) => {
            m["error"] = json!(pages::note(&e, "模型"));
            return m;
        }
    };
    m["hint"] = llm_hint(&list).unwrap_or(Value::Null);
    m["providers"] = Value::Array(
        list.providers
            .iter()
            .map(|p| provider_view(p, edit == Some(p.id.as_str())))
            .collect(),
    );
    let default = list
        .providers
        .iter()
        .find(|p| p.is_default)
        .or(list.providers.first());
    let mut assigns = Vec::new();
    for (purpose, label) in purposes::ALL {
        let found = list.assignments.iter().find(|a| a.purpose == *purpose);
        let (pid, model, follow) = match (found, default) {
            (Some(a), _) => (a.provider_id.clone(), a.model.clone(), false),
            (None, Some(d)) => (
                d.id.clone(),
                d.models.first().cloned().unwrap_or_default(),
                true,
            ),
            (None, None) => continue,
        };
        let mut models: Vec<String> = list
            .providers
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.models.clone())
            .unwrap_or_default();
        if !model.is_empty() && !models.contains(&model) {
            models.insert(0, model.clone());
        }
        assigns.push(
            json!({"purpose": purpose, "label": label, "provider_id": pid,
            "model": model, "models": models, "follow": follow}),
        );
    }
    m["assigns"] = Value::Array(assigns);
    m["usage_note"] = json!("本月还没有调用。");
    match llm
        .usage(&UsageQuery {
            since: month_start(),
            until: None,
        })
        .await
    {
        Ok(rows) => {
            let (u, total) = usage_view(&rows);
            m["usage"] = Value::Array(u);
            m["usage_total"] = json!(if rows.is_empty() {
                String::new()
            } else {
                total
            });
        }
        Err(_) => {
            m["usage"] = json!([]);
            m["usage_total"] = json!("");
            m["usage_note"] = json!("用量暂时读取不到。");
        }
    }
    m
}

async fn llm_render(st: &AppState, edit: Option<&str>, flash: Flash) -> Response {
    page("settings_llm.html", llm_ctx(st, edit, flash).await)
}

async fn llm_get(State(st): Shared, Query(q): Query<EditQuery>) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    llm_render(&st, q.edit.as_deref(), None).await
}

#[derive(Deserialize)]
struct ProviderForm {
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    kind: String,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    models: String,
}

async fn llm_save(State(st): Shared, Form(f): Form<ProviderForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let name = trimmed(&f.name);
    let base_url = trimmed(&f.base_url);
    let edit = if f.id.is_empty() {
        "new"
    } else {
        f.id.as_str()
    };
    if name.is_empty() || !http_url(&base_url) {
        let flash = Some((
            false,
            "名称不能为空，地址要以 http:// 或 https:// 开头。".to_string(),
        ));
        return llm_render(&st, Some(edit), flash).await;
    }
    let kind = if f.kind == provider_kinds::ANTHROPIC {
        provider_kinds::ANTHROPIC
    } else {
        provider_kinds::OPENAI
    };
    let models: Vec<String> = f
        .models
        .split([',', '，', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let key = trimmed(&f.api_key);
    let input = ProviderInput {
        id: (!f.id.is_empty()).then(|| f.id.clone()),
        name,
        kind: kind.to_string(),
        base_url,
        models,
        api_key: (!key.is_empty()).then_some(key),
    };
    match LlmClient(st.ctx.service("llm")).provider_save(&input).await {
        Ok(_) => llm_render(&st, None, ok_flash("已保存。")).await,
        Err(e) => llm_render(&st, Some(edit), bad_flash(&e)).await,
    }
}

async fn llm_test(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let r = LlmClient(st.ctx.service("llm"))
        .test(&TestRequest {
            provider_id: Some(id),
        })
        .await;
    match r {
        Ok(t) if t.ok => pages::msg(true, &format!("连接正常 · {} ms", t.latency_ms)),
        Ok(t) => pages::msg(false, &format!("失败：{}", t.message)),
        Err(e) => pages::msg(false, &e.message),
    }
}

async fn llm_default(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let r = LlmClient(st.ctx.service("llm")).set_default(&id).await;
    let flash = match r {
        Ok(()) => ok_flash("已设为默认。"),
        Err(e) => bad_flash(&e),
    };
    llm_render(&st, None, flash).await
}

async fn llm_delete(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let r = LlmClient(st.ctx.service("llm")).provider_delete(&id).await;
    let flash = match r {
        Ok(()) => ok_flash("已删除。"),
        Err(e) => bad_flash(&e),
    };
    llm_render(&st, None, flash).await
}

#[derive(Deserialize)]
struct AssignForm {
    purpose: String,
    provider_id: String,
    #[serde(default)]
    model: String,
}

async fn llm_assign(State(st): Shared, Form(f): Form<AssignForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let llm = LlmClient(st.ctx.service("llm"));
    let list = match llm.providers().await {
        Ok(l) => l,
        Err(e) => return llm_render(&st, None, bad_flash(&e)).await,
    };
    let Some(p) = list.providers.iter().find(|p| p.id == f.provider_id) else {
        let flash = Some((false, "模型服务不存在。".to_string()));
        return llm_render(&st, None, flash).await;
    };
    // 换了服务后，原来的模型可能不属于新服务：改用新服务的第一个模型
    let model = if p.models.contains(&f.model) {
        f.model.clone()
    } else {
        p.models.first().cloned().unwrap_or_default()
    };
    let a = Assignment {
        purpose: f.purpose,
        provider_id: f.provider_id,
        model,
    };
    let flash = match llm.assign(&a).await {
        Ok(()) => ok_flash("已更新用途分配。"),
        Err(e) => bad_flash(&e),
    };
    llm_render(&st, None, flash).await
}

// ---------------------------------------------------------------- 信源

fn source_line(status: Option<&SourceStatus>, f: Option<&FeedInfo>) -> String {
    match status {
        Some(s) if s.state == "failing" => format!(
            "RSS · 上次失败：{}",
            s.last_error.as_deref().unwrap_or("未知错误")
        ),
        Some(s) if s.interval_secs >= 60 => format!("RSS · 每 {} 分钟", s.interval_secs / 60),
        Some(s) if s.interval_secs > 0 => format!("RSS · 每 {} 秒", s.interval_secs),
        _ => match f {
            Some(f) => format!("RSS · {}", f.url),
            None => "RSS".to_string(),
        },
    }
}

fn source_badge(paused: bool, status: Option<&SourceStatus>) -> (&'static str, &'static str) {
    if paused {
        return ("", "已暂停");
    }
    match status.map(|s| s.state.as_str()) {
        Some("ok") => ("ok", "正常"),
        Some("failing") => ("wa", "重试中"),
        _ => ("", "等待中"),
    }
}

async fn sources_ctx(st: &AppState, edit: Option<&str>, flash: Flash) -> Value {
    let feeds = FeedAdminClient(st.ctx.service("feed-admin")).list().await;
    let statuses = SourceStatusClient(st.ctx.service("source-status"))
        .list()
        .await;
    let mut m = json!({
        "blank": {"id": "", "name": "", "url": ""},
        "new_form": edit == Some("new"),
        "can_add": feeds.is_ok(),
    });
    put_flash(&mut m, flash);
    m["x"] = x_ctx(st).await;
    if let Ok(plugins) = admin(st).plugins().await {
        if let Some(p) = plugins.iter().find(|p| p.name == "x-browser") {
            m["x"]["plugin"] = plugin_view(p).0;
        }
    }
    let status_list: &[SourceStatus] = statuses.as_deref().unwrap_or(&[]);
    let feed_list: &[FeedInfo] = feeds.as_deref().unwrap_or(&[]);
    let mut out = Vec::new();
    for f in feed_list {
        let s = status_list.iter().find(|s| s.id == f.id);
        let (cls, badge) = source_badge(f.paused, s);
        out.push(json!({
            "id": f.id, "eid": enc(&f.id), "name": f.name, "url": f.url,
            "cls": cls, "badge": badge, "line": source_line(s, Some(f)),
            "feed": true, "paused": f.paused, "can_retry": s.is_some() && !f.paused,
            "editing": edit == Some(f.id.as_str()),
        }));
    }
    for s in status_list {
        // rss 信源的有效记录以 feed-admin 列表为准：已删除订阅在状态表里的旧记录不显示。
        // x: 信源由 X 卡片统一管理
        if s.id.starts_with("x:")
            || feed_list.iter().any(|f| f.id == s.id)
            || (feeds.is_ok() && s.id.starts_with("rss:"))
        {
            continue;
        }
        let (cls, badge) = source_badge(false, Some(s));
        out.push(json!({
            "id": s.id, "eid": enc(&s.id), "name": s.name, "url": "",
            "cls": cls, "badge": badge, "line": source_line(Some(s), None),
            "feed": false, "paused": false, "can_retry": true, "editing": false,
        }));
    }
    if let (Err(e1), Err(e2)) = (&feeds, &statuses) {
        let e = if e1.code == ServiceError::UNAVAILABLE {
            e2
        } else {
            e1
        };
        m["error"] = json!(pages::note(e, "信源"));
    }
    m["sources"] = Value::Array(out);
    m
}

/// X 采集卡片的数据；x-admin 没有运行时 `available` 为 false。
async fn x_ctx(st: &AppState) -> Value {
    let admin = XAdminClient(st.ctx.service("x-admin"));
    let status: XStatus = match admin.status().await {
        Ok(s) => s,
        Err(_) => return json!({ "available": false }),
    };
    let accounts: Vec<XAccount> = admin.list().await.unwrap_or_default();
    let list: Vec<Value> = accounts
        .iter()
        .map(|a| {
            json!({
                "handle": a.handle, "eh": enc(&a.handle),
                "paused": a.paused, "builtin": a.builtin,
            })
        })
        .collect();
    json!({
        "available": true,
        "browser": status.browser,
        "logged_in": status.logged_in,
        "login_open": status.login_window_open,
        "last_error": status.last_error,
        "accounts": list,
    })
}

async fn x_login(State(st): Shared) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match XAdminClient(st.ctx.service("x-admin")).login().await {
        Ok(_) => ok_flash("已打开登录窗口，请在弹出的浏览器里登录 X，登录完成后窗口会自动关闭。"),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn x_enabled(State(st): Shared, Form(f): Form<EnabledForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match admin(&st).set_enabled("x-browser", f.enabled).await {
        Ok(()) => {
            *st.status.0.lock().await = None;
            ok_flash(if f.enabled {
                "X 采集已开启。"
            } else {
                "X 采集已停用，关注列表仍然保留。"
            })
        }
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn x_open(State(st): Shared) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    match st.ctx.service("x-admin").call("open", json!({})).await {
        Ok(_) => pages::msg(
            true,
            "已在采集使用的登录会话中打开X；浏览期间采集暂缓，关闭浏览器窗口后恢复。",
        ),
        Err(e) => pages::msg(false, &e.message),
    }
}

#[derive(Deserialize)]
struct XAddForm {
    handle: String,
}

async fn x_add(State(st): Shared, Form(f): Form<XAddForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match XAdminClient(st.ctx.service("x-admin"))
        .add(&trimmed(&f.handle))
        .await
    {
        Ok(_) => ok_flash("已添加。"),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn x_pause(
    State(st): Shared,
    Path(handle): Path<String>,
    Form(f): Form<PauseForm>,
) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let paused = f.paused == "true";
    let flash = match XAdminClient(st.ctx.service("x-admin"))
        .set_paused(&handle, paused)
        .await
    {
        Ok(_) => ok_flash(if paused {
            "已暂停。"
        } else {
            "已恢复。"
        }),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn x_delete(State(st): Shared, Path(handle): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match XAdminClient(st.ctx.service("x-admin"))
        .delete(&handle)
        .await
    {
        Ok(()) => ok_flash("已删除。"),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn sources_render(st: &AppState, edit: Option<&str>, flash: Flash) -> Response {
    page("settings_sources.html", sources_ctx(st, edit, flash).await)
}

async fn sources_get(State(st): Shared, Query(q): Query<EditQuery>) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    sources_render(&st, q.edit.as_deref(), None).await
}

#[derive(Deserialize)]
struct FeedForm {
    #[serde(default)]
    id: String,
    name: String,
    url: String,
}

async fn sources_save(State(st): Shared, Form(f): Form<FeedForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let name = trimmed(&f.name);
    let url = trimmed(&f.url);
    let edit = if f.id.is_empty() {
        "new"
    } else {
        f.id.as_str()
    };
    if name.is_empty() || !http_url(&url) {
        let flash = Some((
            false,
            "名称不能为空，地址要以 http:// 或 https:// 开头。".to_string(),
        ));
        return sources_render(&st, Some(edit), flash).await;
    }
    let input = FeedInput {
        id: (!f.id.is_empty()).then(|| f.id.clone()),
        name,
        url,
    };
    match FeedAdminClient(st.ctx.service("feed-admin"))
        .save(&input)
        .await
    {
        Ok(_) => sources_render(&st, None, ok_flash("已保存。")).await,
        Err(e) => sources_render(&st, Some(edit), bad_flash(&e)).await,
    }
}

#[derive(Deserialize)]
struct PauseForm {
    paused: String,
}

async fn sources_pause(
    State(st): Shared,
    Path(id): Path<String>,
    Form(f): Form<PauseForm>,
) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let paused = f.paused == "true";
    let flash = match FeedAdminClient(st.ctx.service("feed-admin"))
        .set_paused(&id, paused)
        .await
    {
        Ok(_) => ok_flash(if paused {
            "已暂停。"
        } else {
            "已恢复。"
        }),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn sources_delete(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match FeedAdminClient(st.ctx.service("feed-admin"))
        .delete(&id)
        .await
    {
        Ok(()) => ok_flash("已删除。"),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

async fn sources_retry(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match SourceStatusClient(st.ctx.service("source-status"))
        .retry(&id)
        .await
    {
        Ok(()) => ok_flash("已安排立即重试。"),
        Err(e) => bad_flash(&e),
    };
    sources_render(&st, None, flash).await
}

// ---------------------------------------------------------------- 推送

struct Rules {
    focus_mode: bool,
    urgent: bool,
    important: bool,
    digest: bool,
    digest_time: String,
    quiet_hours: String,
}

impl Rules {
    fn from_config(c: &Value) -> Rules {
        let b = |k: &str| c.get(k).and_then(Value::as_bool).unwrap_or(true);
        let s = |k: &str, d: &str| {
            c.get(k)
                .and_then(Value::as_str)
                .unwrap_or(d)
                .trim()
                .to_string()
        };
        Rules {
            focus_mode: b("focus_mode"),
            urgent: b("push_urgent"),
            important: b("push_important"),
            digest: b("digest_enabled"),
            digest_time: s("digest_time", "08:00"),
            quiet_hours: s("quiet_hours", ""),
        }
    }

    fn recv(&self) -> String {
        if self.focus_mode {
            return "重点事件（范围见首页推送设置）".into();
        }
        let mut v = Vec::new();
        if self.urgent {
            v.push("紧急");
        }
        if self.important {
            v.push("重要");
        }
        if self.digest {
            v.push("日报");
        }
        if v.is_empty() {
            "无".to_string()
        } else {
            v.join("、")
        }
    }

    fn view(&self) -> Value {
        let range = if self.quiet_hours.is_empty() {
            DEFAULT_QUIET
        } else {
            &self.quiet_hours
        };
        json!({"focus_mode":self.focus_mode,"urgent": self.urgent, "important": self.important, "digest": self.digest,
            "digest_time": self.digest_time, "quiet": !self.quiet_hours.is_empty(),
            "quiet_label": range.replace('-', "–")})
    }
}

fn channel_state_cls(s: ChannelState) -> &'static str {
    match s {
        ChannelState::Ok => "ok",
        ChannelState::Offline => "bad",
        _ => "wa",
    }
}

fn channel_view(
    id: &str,
    info: Result<ChannelInfo, ServiceError>,
    rules: &Rules,
    webhook_set: bool,
    hook: bool,
) -> Value {
    let kind = match id {
        "wechat-ilink" => "wechat",
        "wecom-webhook" => "wecom",
        _ => "other",
    };
    let (icon, label, field) = match kind {
        "wechat" => ("wechat", "微信", "账号"),
        "wecom" => ("msg", "企业微信机器人", "Webhook"),
        _ => ("send", id, "状态"),
    };
    let mut v = json!({
        "id": id, "dom": dom(id), "kind": kind, "icon": icon, "label": label,
        "field": field, "recv": rules.recv(), "configured": webhook_set,
        "hook_form": kind == "wecom" && hook,
    });
    match info {
        Ok(i) => {
            let state = pages::channel_state_label(id, i.state);
            v["cls"] = json!(channel_state_cls(i.state));
            v["badge"] = json!(state);
            v["detail"] = json!(i.detail);
            v["unbound"] = json!(matches!(
                i.state,
                ChannelState::Unbound | ChannelState::Expired
            ));
            v["value"] = json!(match kind {
                "wecom" if webhook_set => "已设置".to_string(),
                "wecom" => "未设置".to_string(),
                "wechat" if i.state == ChannelState::Ok => "已绑定".to_string(),
                _ => state.to_string(),
            });
        }
        Err(e) => {
            v["cls"] = json!("bad");
            v["badge"] = json!("不可用");
            v["error"] = json!(format!("读取渠道信息失败：{}", e.message));
            v["unbound"] = json!(false);
            v["value"] = json!(if kind == "wecom" && webhook_set {
                "已设置"
            } else {
                "未知"
            });
        }
    }
    v
}

async fn read_rules(st: &AppState) -> Result<Rules, ServiceError> {
    let c = admin(st).config_get("dispatch").await?;
    Ok(Rules::from_config(&c.config))
}

/// 推送分区顶部的提示：没有任何渠道状态为正常时，列出各渠道当前状态。
fn push_hint(channels: &[Value]) -> Value {
    if channels.iter().any(|c| c["badge"] == "正常") {
        return Value::Null;
    }
    let text = if channels.is_empty() {
        "没有可用的推送渠道，消息不会推送出去。".to_string()
    } else {
        let parts: Vec<String> = channels
            .iter()
            .map(|c| {
                format!(
                    "{}：{}",
                    c["label"].as_str().unwrap_or(""),
                    c["badge"].as_str().unwrap_or("")
                )
            })
            .collect();
        format!(
            "没有可用的推送渠道（{}），消息不会推送出去。",
            parts.join("；")
        )
    };
    json!({"cls": "bad", "text": text})
}

async fn push_ctx(st: &AppState, hook: bool, flash: Flash) -> Value {
    let mut m = json!({});
    put_flash(&mut m, flash);
    let (rules, rules_error) = match read_rules(st).await {
        Ok(r) => (r, None),
        Err(e) => (
            Rules::from_config(&json!({})),
            Some(format!("读取推送规则失败：{}", e.message)),
        ),
    };
    let webhook_set = match SecretsClient(st.ctx.service("secrets")).list().await {
        Ok(l) => l.iter().any(|e| e.key == WECOM_KEY && e.configured),
        Err(_) => false,
    };
    let mut list = st.ctx.services("channel");
    list.sort_by(|a, b| a.0.cmp(&b.0));
    let mut channels = Vec::new();
    for (id, svc) in list {
        let info = ChannelClient(svc).info().await;
        channels.push(channel_view(&id, info, &rules, webhook_set, hook));
    }
    let plugins = admin(st).plugins().await;
    if let Ok(plugins) = plugins {
        for p in &plugins {
            if p.name == "dispatch" {
                m["dispatch"] = plugin_view(p).0;
            } else if matches!(p.name.as_str(), "wechat-ilink" | "wecom-webhook") {
                if !channels.iter().any(|c| c["id"] == p.name) {
                    channels.push(channel_view(
                        &p.name,
                        Err(ServiceError::unavailable("渠道未运行")),
                        &rules,
                        webhook_set,
                        hook,
                    ));
                }
                if let Some(c) = channels.iter_mut().find(|c| c["id"] == p.name) {
                    c["plugin"] = plugin_view(p).0;
                    if !p.enabled {
                        c["badge"] = json!("已停用");
                        c["cls"] = json!("");
                        c["error"] = json!("渠道已停用，打开上方开关即可恢复。");
                    }
                }
            }
        }
    }
    m["hint"] = push_hint(&channels);
    m["channels"] = Value::Array(channels);
    m["rules"] = rules.view();
    m["rules_error"] = json!(rules_error);
    m
}

async fn push_render(st: &AppState, hook: bool, flash: Flash) -> Response {
    page("settings_push.html", push_ctx(st, hook, flash).await)
}

async fn push_get(State(st): Shared, Query(q): Query<EditQuery>) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    push_render(&st, q.hook.is_some(), None).await
}

async fn push_plugin_enabled(
    State(st): Shared,
    Path(name): Path<String>,
    Form(f): Form<EnabledForm>,
) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    if !matches!(name.as_str(), "dispatch" | "wechat-ilink" | "wecom-webhook") {
        return err_response(ServiceError::invalid_params("此开关仅支持推送相关插件"));
    }
    let flash = match admin(&st).set_enabled(&name, f.enabled).await {
        Ok(()) => {
            *st.status.0.lock().await = None;
            ok_flash(if f.enabled {
                "已开启。"
            } else {
                "已停用。"
            })
        }
        Err(e) => bad_flash(&e),
    };
    push_render(&st, false, flash).await
}

#[derive(Deserialize)]
struct WebhookForm {
    value: String,
}

async fn push_webhook(State(st): Shared, Form(f): Form<WebhookForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let v = trimmed(&f.value);
    if v.is_empty() {
        return push_render(&st, true, Some((false, "Webhook 不能为空。".to_string()))).await;
    }
    match SecretsClient(st.ctx.service("secrets"))
        .set(WECOM_KEY, &v)
        .await
    {
        Ok(()) => push_render(&st, false, ok_flash("Webhook 已保存。")).await,
        Err(e) => push_render(&st, true, bad_flash(&e)).await,
    }
}

#[derive(Deserialize, Default)]
struct RulesForm {
    urgent: Option<String>,
    important: Option<String>,
    digest: Option<String>,
    quiet: Option<String>,
}

async fn push_rules(State(st): Shared, Form(f): Form<RulesForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let existing = match read_rules(&st).await {
        Ok(r) => r.quiet_hours,
        Err(e) => return push_render(&st, false, bad_flash(&e)).await,
    };
    let quiet = if f.quiet.is_none() {
        String::new()
    } else if existing.is_empty() {
        DEFAULT_QUIET.to_string()
    } else {
        existing
    };
    let values = json!({
        "push_urgent": f.urgent.is_some(),
        "push_important": f.important.is_some(),
        "digest_enabled": f.digest.is_some(),
        "quiet_hours": quiet,
    });
    let flash = match admin(&st).config_set("dispatch", values).await {
        Ok(()) => ok_flash("推送规则已保存。"),
        Err(e) => bad_flash(&e),
    };
    push_render(&st, false, flash).await
}

// ---------------------------------------------------------------- 插件

fn plugin_label(name: &str) -> (&str, &'static str) {
    match name {
        "agent" => ("追问", "msg"),
        "operations" => ("应用操作", "tool"),
        "auth" => ("认证", "shield"),
        "binance-ann" => ("币安公告", "rss"),
        "collect" => ("采集调度", "rss"),
        "dispatch" => ("推送调度", "send"),
        "llm" => ("模型服务", "cpu"),
        "maintenance" => ("数据维护", "tool"),
        "okx-ann" => ("OKX 公告", "rss"),
        "rss" => ("RSS 信源", "rss"),
        "rulebook" => ("规则", "filter"),
        "scheduler" => ("定时任务", "clock"),
        "screen" => ("初筛", "filter"),
        "secrets" => ("密钥", "key"),
        "store" => ("数据存储", "db"),
        "web" => ("网页界面", "world"),
        "wechat-ilink" => ("微信推送", "wechat"),
        "wecom-webhook" => ("企业微信推送", "msg"),
        _ => (name, "plug"),
    }
}

/// 清单里声明的、可以在界面里改的字段（不含密钥）。
fn config_fields(fields: &Value) -> Vec<(String, Value)> {
    fields
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| v.get("type").and_then(Value::as_str) != Some("secret"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn plugin_view(p: &PluginInfo) -> (Value, bool) {
    let (label, icon) = plugin_label(&p.name);
    let running = p.status.contains("运行");
    let (cls, mismatch) = if running {
        ("ok", !p.enabled)
    } else if p.status.contains("停用") {
        ("", p.enabled)
    } else if p.status.contains("失败") {
        ("bad", false)
    } else {
        ("wa", false)
    };
    let perms = if p.permissions.is_empty() {
        "无".to_string()
    } else {
        p.permissions.join(", ")
    };
    let fixed = matches!(p.name.as_str(), "web" | "store" | "auth");
    let v = json!({
        "name": p.name, "dom": dom(&p.name), "label": label, "icon": icon,
        "kind_label": if p.kind == "official" { "官方" } else { "扩展" },
        "version": p.version, "perms": perms, "granted": p.granted,
        "desc": p.description, "missing": p.missing.join(", "),
        "last_error": p.last_error, "cls": cls, "badge": p.status,
        "enabled": p.enabled, "toggle": !fixed,
        "can_config": !config_fields(&p.config_fields).is_empty(),
        "can_restart": p.name != "web",
    });
    (v, mismatch)
}

async fn plugins_ctx(st: &AppState, flash: Flash) -> Value {
    let mut m = json!({});
    put_flash(&mut m, flash);
    match admin(st).plugins().await {
        Ok(list) => {
            let mut need = false;
            let mut out = Vec::new();
            for p in &list {
                let (v, mismatch) = plugin_view(p);
                need |= mismatch;
                out.push(v);
            }
            m["plugins"] = Value::Array(out);
            m["need_restart"] = json!(need);
        }
        Err(e) => m["error"] = json!(pages::note(&e, "插件管理")),
    }
    m
}

async fn plugins_render(st: &AppState, flash: Flash) -> Response {
    page("settings_plugins.html", plugins_ctx(st, flash).await)
}

async fn plugins_get(State(st): Shared) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    plugins_render(&st, None).await
}

async fn api_plugins(State(st): Shared) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    match admin(&st).plugins().await {
        Ok(p) => Json(json!({ "plugins": p })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn api_sources(State(st): Shared) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    match SourceStatusClient(st.ctx.service("source-status"))
        .list()
        .await
    {
        Ok(s) => Json(json!({ "sources": s })).into_response(),
        Err(e) => err_response(e),
    }
}

async fn plugin_grant(State(st): Shared, Path(name): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match admin(&st).grant(&name).await {
        Ok(()) => None,
        Err(e) => bad_flash(&e),
    };
    plugins_render(&st, flash).await
}

#[derive(Deserialize)]
struct EnabledForm {
    enabled: bool,
}

async fn plugin_enabled(
    State(st): Shared,
    Path(name): Path<String>,
    Form(f): Form<EnabledForm>,
) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match admin(&st).set_enabled(&name, f.enabled).await {
        Ok(()) => None,
        Err(e) => bad_flash(&e),
    };
    plugins_render(&st, flash).await
}

/// 配置面板：有 `fields` 时画表单，否则只显示一行提示。
async fn config_panel(st: &AppState, name: &str, flash: Flash) -> Response {
    let mut m = json!({"name": name, "dom": dom(name)});
    if let Some((ok, t)) = flash {
        m["text"] = json!(t);
        m["ok"] = json!(ok);
    }
    let a = admin(st);
    let loaded = async {
        let list = a.plugins().await?;
        let info = list
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| ServiceError::invalid_params("插件不存在"))?;
        let cfg = a.config_get(name).await?;
        Ok::<_, ServiceError>((info, cfg))
    }
    .await;
    match loaded {
        Ok((info, cfg)) => {
            let mut fields = Vec::new();
            for (key, f) in config_fields(&info.config_fields) {
                let ty = f.get("type").and_then(Value::as_str).unwrap_or("string");
                let cur = cfg.config.get(&key).cloned().unwrap_or(Value::Null);
                let value = match &cur {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => String::new(),
                };
                fields.push(json!({
                    "key": key, "type": ty, "value": value,
                    "checked": cur.as_bool().unwrap_or(false),
                    "label": f.get("label").and_then(Value::as_str).unwrap_or(&key),
                    "help": f.get("help").and_then(Value::as_str),
                }));
            }
            m["fields"] = Value::Array(fields);
        }
        Err(e) => {
            m["text"] = json!(e.message);
            m["ok"] = json!(false);
        }
    }
    page("settings_plugin_cfg.html", m)
}

async fn plugin_config_get(State(st): Shared, Path(name): Path<String>) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    config_panel(&st, &name, None).await
}

async fn plugin_config_save(
    State(st): Shared,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let info = match admin(&st).plugins().await {
        Ok(l) => l.into_iter().find(|p| p.name == name),
        Err(e) => return config_panel(&st, &name, bad_flash(&e)).await,
    };
    let Some(info) = info else {
        return config_panel(&st, &name, Some((false, "插件不存在。".to_string()))).await;
    };
    let mut values = Map::new();
    for (key, f) in config_fields(&info.config_fields) {
        let raw = form.get(&key).map(|s| s.trim().to_string());
        let v = match f.get("type").and_then(Value::as_str).unwrap_or("string") {
            "boolean" => json!(raw.is_some()),
            "integer" => match raw.as_deref() {
                None | Some("") => Value::Null,
                Some(s) => match s.parse::<i64>() {
                    Ok(n) => json!(n),
                    Err(_) => {
                        let t = format!("“{}”需要填整数。", f["label"].as_str().unwrap_or(&key));
                        return config_panel(&st, &name, Some((false, t))).await;
                    }
                },
            },
            "string_list" => json!(raw
                .unwrap_or_default()
                .split([',', '，', '\n'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()),
            _ => match raw {
                Some(s) if !s.is_empty() => json!(s),
                _ => Value::Null,
            },
        };
        values.insert(key, v);
    }
    let flash = match admin(&st).config_set(&name, Value::Object(values)).await {
        Ok(()) => ok_flash("已保存，插件已重启。"),
        Err(e) => bad_flash(&e),
    };
    config_panel(&st, &name, flash).await
}

async fn plugin_restart(State(st): Shared, Path(name): Path<String>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let (ok, t) = match admin(&st).restart(&name).await {
        Ok(()) => (true, "已重启。".to_string()),
        Err(e) => (false, e.message),
    };
    page(
        "settings_plugin_cfg.html",
        json!({"name": name, "dom": dom(&name), "text": t, "ok": ok}),
    )
}

// ---------------------------------------------------------------- 系统

#[derive(Deserialize, Default)]
struct SettingsConfig {
    proxy_test_url: Option<String>,
}

async fn system_ctx(st: &AppState, flash: Flash) -> Value {
    let mut m = json!({"version": env!("CARGO_PKG_VERSION")});
    put_flash(&mut m, flash);
    let a = admin(st);
    let shell = a.app_get(app_keys::SHELL).await.unwrap_or(Value::Null);
    if shell.as_str() == Some("desktop") {
        let update = a
            .app_get(app_keys::UPDATE_STATUS)
            .await
            .unwrap_or(Value::Null);
        if let Some(version) = update.get("current_version").and_then(Value::as_str) {
            m["version"] = json!(version);
        }
    }
    let autostart = a.app_get(app_keys::AUTOSTART).await.unwrap_or(Value::Null);
    let tray = a
        .app_get(app_keys::CLOSE_TO_TRAY)
        .await
        .unwrap_or(Value::Null);
    m["desktop"] = json!(shell.as_str() == Some("desktop"));
    m["autostart"] = json!(autostart.as_bool().unwrap_or(false));
    m["close_to_tray"] = json!(tray.as_bool().unwrap_or(true));
    m["auto_update"] = json!(a
        .app_get(app_keys::AUTO_UPDATE)
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(true));
    m
}

async fn system_render(st: &AppState, flash: Flash) -> Response {
    page("settings_system.html", system_ctx(st, flash).await)
}

async fn system_get(State(st): Shared) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    system_render(&st, None).await
}

#[derive(Deserialize)]
struct UpdateForm {
    action: String,
}
async fn update_action(State(st): Shared, Form(f): Form<UpdateForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    if !matches!(f.action.as_str(), "check" | "install") {
        return err_response(ServiceError::invalid_params("未知更新操作"));
    }
    let flash=match admin(&st).app_set(app_keys::UPDATE_ACTION,json!({"kind":f.action,"request_id":time::OffsetDateTime::now_utc().unix_timestamp_nanos().to_string()})).await{Ok(())=>ok_flash("更新请求已提交。"),Err(e)=>bad_flash(&e)};
    system_render(&st, flash).await
}
async fn auto_update(State(st): Shared, Form(f): Form<EnabledForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let flash = match admin(&st)
        .app_set(app_keys::AUTO_UPDATE, json!(f.enabled))
        .await
    {
        Ok(()) => ok_flash("自动检查更新设置已保存。"),
        Err(e) => bad_flash(&e),
    };
    system_render(&st, flash).await
}

async fn network_render(st: &AppState, flash: Flash) -> Response {
    let mut m = json!({});
    put_flash(&mut m, flash);
    match admin(st).app_get(app_keys::PROXY).await {
        Ok(value) => m["proxy_url"] = json!(value.as_str().unwrap_or("")),
        Err(e) => m["error"] = json!(pages::note(&e, "网络设置")),
    }
    page("settings_network.html", m)
}

async fn network_get(State(st): Shared) -> Response {
    if let Some(r) = guard_view(&st).await {
        return r;
    }
    network_render(&st, None).await
}

#[derive(Deserialize)]
struct ProxyForm {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    url: String,
}

fn valid_proxy(u: &str) -> bool {
    ["http://", "https://", "socks5://", "socks5h://"]
        .iter()
        .any(|p| u.starts_with(p) && u.len() > p.len())
        && !u.contains(' ')
}

async fn proxy_save(State(st): Shared, Form(f): Form<ProxyForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let url = if f.mode == "direct" {
        String::new()
    } else {
        trimmed(&f.url)
    };
    if f.mode != "direct" && !valid_proxy(&url) {
        let t = "代理地址要以 http://、https://、socks5:// 或 socks5h:// 开头。";
        return network_render(&st, Some((false, t.to_string()))).await;
    }
    let flash = match admin(&st).app_set(app_keys::PROXY, json!(url)).await {
        Ok(()) => ok_flash("已保存，重启程序后生效。"),
        Err(e) => bad_flash(&e),
    };
    network_render(&st, flash).await
}

async fn proxy_test(State(st): Shared) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let cfg: SettingsConfig = st.ctx.config_as().unwrap_or_default();
    let url = cfg
        .proxy_test_url
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PROXY_TEST.to_string());
    let t = Instant::now();
    match st
        .ctx
        .http()
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => {
            pages::msg(true, &format!("连接正常 · {} ms", t.elapsed().as_millis()))
        }
        Ok(r) => pages::msg(false, &format!("失败：目标返回 {}", r.status().as_u16())),
        Err(e) if e.is_timeout() => pages::msg(false, "失败：连接超时"),
        Err(_) => pages::msg(false, "失败：无法连接，请检查代理设置"),
    }
}

#[derive(Deserialize, Default)]
struct AppForm {
    autostart: Option<String>,
    close_to_tray: Option<String>,
}

async fn app_save(State(st): Shared, Form(f): Form<AppForm>) -> Response {
    if let Some(r) = guard_manage(&st).await {
        return r;
    }
    let a = admin(&st);
    let shell = a.app_get(app_keys::SHELL).await.unwrap_or(Value::Null);
    if shell.as_str() != Some("desktop") {
        return system_render(&st, Some((false, "这两项仅桌面版可用。".to_string()))).await;
    }
    let r = async {
        a.app_set(app_keys::AUTOSTART, json!(f.autostart.is_some()))
            .await?;
        a.app_set(app_keys::CLOSE_TO_TRAY, json!(f.close_to_tray.is_some()))
            .await
    }
    .await;
    let flash = match r {
        Ok(()) => ok_flash("已保存。"),
        Err(e) => bad_flash(&e),
    };
    system_render(&st, flash).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use plugin_sdk::testing::{test_http_client, TestHub};
    use serde_json::{json, Value};

    use super::*;

    const SECRET: &str = "sk-FAKE-KEY-123";
    const HOOK: &str = "https://hook.example/FAKE-HOOK-999";

    struct Fx {
        base: String,
        calls: Arc<Mutex<Vec<String>>>,
        client: reqwest::Client,
        _hub: Arc<TestHub>,
    }

    impl Fx {
        fn called(&self, prefix: &str, needle: &str) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with(prefix) && c.contains(needle))
        }
        async fn get(&self, p: &str) -> (u16, String) {
            let r = self.client.get(format!("{}{p}", self.base)).send().await;
            let r = r.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
        async fn post(&self, p: &str, form: &[(&str, &str)]) -> (u16, String) {
            let r = self.client.post(format!("{}{p}", self.base));
            let r = r.form(form).send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    }

    fn record(
        hub: &TestHub,
        plugin: &str,
        service: &str,
        calls: &Arc<Mutex<Vec<String>>>,
        f: impl Fn(&str) -> Value + Send + Sync + 'static,
    ) {
        let c = calls.clone();
        let tag = service.to_string();
        let f = Arc::new(f);
        hub.register_fn(plugin, service, move |m, p| {
            let c = c.clone();
            let f = f.clone();
            let tag = tag.clone();
            async move {
                c.lock().unwrap().push(format!("{tag}.{m} {p}"));
                Ok(f(&m))
            }
        });
    }

    async fn fx(allowed: bool, shell: Value, test_url: Option<&str>) -> Fx {
        let providers = json!({"providers":[{"id":"p1","name":"深度求索","kind":"openai",
            "base_url":"https://api.example/v1","models":["m-a","m-b"],"has_key":true,
            "is_default":true,"last_test":null}],
            "assignments":[]});
        fx_with(allowed, shell, test_url, providers, "ok").await
    }

    async fn fx_with(
        allowed: bool,
        shell: Value,
        test_url: Option<&str>,
        providers: Value,
        channel_state: &'static str,
    ) -> Fx {
        let hub = TestHub::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        hub.register_fn("auth", "auth", move |_m, _p| async move {
            Ok(json!({ "allowed": allowed }))
        });
        record(&hub, "hub", "hub-admin", &calls, move |m| match m {
            "app_get" => json!({ "value": shell.clone() }),
            "config_get" => json!({"name":"dispatch","config":{"push_urgent":true,
                "push_important":false,"digest_enabled":true,"digest_time":"09:30"}}),
            _ => json!({"plugins":[]}),
        });
        record(&hub, "llm", "llm", &calls, move |m| match m {
            "providers" => providers.clone(),
            "usage" => json!({"rows":[{"day":"2026-01-01","model":"m-a","purpose":"screen",
                "calls":3,"input_tokens":1000,"output_tokens":500,"failures":1}]}),
            "test" => json!({"ok":true,"message":"m-a","latency_ms":12}),
            _ => json!({}),
        });
        record(&hub, "rss", "feed-admin", &calls, |m| match m {
            "list" => json!({"feeds":[{"id":"rss:sec","name":"SEC","url":"https://sec.example/rss",
                "paused":false,"builtin":false}]}),
            _ => json!({}),
        });
        record(&hub, "x-browser", "x-admin", &calls, |m| match m {
            "status" => json!({"browser":"Edge","logged_in":true,"login_window_open":false,
                "last_error":null}),
            "list" => json!({"accounts":[{"handle":"binance","paused":false,"builtin":true},
                {"handle":"okx","paused":true,"builtin":true}]}),
            _ => json!({"handle":"binance","paused":false,"builtin":true}),
        });
        record(&hub, "collect", "source-status", &calls, |_| {
            json!({"sources":[{"id":"rss:sec","name":"SEC","state":"ok","last_success_at":null,
                "last_error":null,"consecutive_failures":0,"total_items":1,"interval_secs":600}]})
        });
        record(&hub, "secrets", "secrets", &calls, |m| match m {
            "list" => json!({"secrets":[{"key":"wecom-webhook.webhook_key","configured":true,
                "updated_at":"2026-01-01T00:00:00Z"}]}),
            _ => json!({}),
        });
        record(&hub, "wecom-webhook", "channel", &calls, move |_| {
            json!({"id":"wecom-webhook","name":"企业微信","format":"markdown","max_bytes":2000,
                "backup":false,"state":channel_state,"quota_left":null,"expires_at":null,"detail":null})
        });
        let cfg = match test_url {
            Some(u) => json!({ "proxy_test_url": u }),
            None => json!({}),
        };
        let ctx = hub.context("web", cfg, test_http_client());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::router(ctx);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fx {
            base: format!("http://{addr}"),
            calls,
            client: test_http_client(),
            _hub: hub,
        }
    }

    #[tokio::test]
    async fn llm_fragment_never_shows_key_and_actions_reach_service() {
        let f = fx(true, Value::Null, None).await;
        let (s, b) = f.get("/settings/llm").await;
        assert_eq!(s, 200);
        assert!(b.contains("深度求索") && b.contains("m-b"));
        let (_, b) = f.get("/settings/llm?edit=p1").await;
        assert!(!b.contains(SECRET));
        let (s, b) = f
            .post(
                "/settings/llm/save",
                &[
                    ("id", "p1"),
                    ("name", "深度求索"),
                    ("kind", "openai"),
                    ("base_url", "https://api.example/v1"),
                    ("api_key", SECRET),
                    ("models", "m-a, m-b"),
                ],
            )
            .await;
        assert_eq!(s, 200);
        assert!(!b.contains(SECRET), "密钥不得回显");
        assert!(f.called("llm.provider_save", SECRET));
        let (_, b) = f.post("/settings/llm/p1/test", &[]).await;
        assert!(b.contains("连接正常"));
        f.post("/settings/llm/p1/default", &[]).await;
        assert!(f.called("llm.set_default", "p1"));
        f.post(
            "/settings/llm/assign",
            &[
                ("purpose", "screen"),
                ("provider_id", "p1"),
                ("model", "m-b"),
            ],
        )
        .await;
        assert!(f.called("llm.assign", "m-b"));
        f.post("/settings/llm/p1/delete", &[]).await;
        assert!(f.called("llm.provider_delete", "p1"));
        // 地址不是 http(s) 时不调用服务
        let before = f.calls.lock().unwrap().len();
        let (_, b) = f
            .post(
                "/settings/llm/save",
                &[("name", "x"), ("base_url", "ftp://x"), ("models", "")],
            )
            .await;
        assert!(b.contains("http"));
        let n = f
            .calls
            .lock()
            .unwrap()
            .iter()
            .skip(before)
            .filter(|c| c.starts_with("llm.provider_save"))
            .count();
        assert_eq!(n, 0);
    }

    fn prov(models: &[&str], has_key: bool, default: bool, last_test: Value) -> Value {
        json!({"id":"p1","name":"深度求索","kind":"openai","base_url":"https://api.example/v1",
            "models":models,"has_key":has_key,"is_default":default,"last_test":last_test})
    }

    #[tokio::test]
    async fn llm_hint_covers_six_cases() {
        let ok = json!({"ok":true,"message":"m-a","latency_ms":1});
        let bad = json!({"ok":false,"message":"密钥无效","latency_ms":1});
        let cases = [
            (json!([]), Some("还没有模型服务"), "添加模型服务"),
            (
                json!([prov(&["m"], true, false, Value::Null)]),
                Some("没有默认服务"),
                "设为默认",
            ),
            (
                json!([prov(&[], true, true, Value::Null)]),
                Some("没有可用模型"),
                "填写模型",
            ),
            (
                json!([prov(&["m"], false, true, Value::Null)]),
                Some("密钥未设置"),
                "设置密钥",
            ),
            (
                json!([prov(&["m"], true, true, bad)]),
                Some("测试失败：密钥无效"),
                "检查配置",
            ),
            (json!([prov(&["m"], true, true, ok)]), None, ""),
        ];
        for (providers, want, label) in cases {
            let f = fx_with(
                true,
                Value::Null,
                None,
                json!({"providers": providers, "assignments": []}),
                "ok",
            )
            .await;
            let (s, b) = f.get("/settings/llm").await;
            assert_eq!(s, 200);
            match want {
                Some(t) => {
                    assert!(b.contains("id=\"llm-hint\"") && b.contains(t), "{t}: {b}");
                    assert!(b.contains(label), "{label}");
                }
                None => assert!(!b.contains("llm-hint")),
            }
        }
    }

    #[tokio::test]
    async fn home_banner_names_each_missing_item() {
        let ready = json!({"providers":[prov(&["m"], true, true, Value::Null)],"assignments":[]});
        let none = json!({"providers":[], "assignments": []});
        let cases = [
            // (模型, 渠道状态, 应有模型缺口, 应有推送缺口)
            (ready.clone(), "unbound", false, true),
            (none.clone(), "ok", true, false),
            (none, "unbound", true, true),
            (ready, "ok", false, false),
        ];
        for (providers, state, want_llm, want_push) in cases {
            let f = fx_with(true, Value::Null, None, providers, state).await;
            let (s, b) = f.get("/").await;
            assert_eq!(s, 200);
            assert_eq!(b.contains("data-gap=\"llm\""), want_llm, "{b}");
            assert_eq!(b.contains("data-gap=\"push\""), want_push, "{b}");
            assert_eq!(
                b.contains("href=\"/settings#s-llm\"") && b.contains("去设置模型服务"),
                want_llm
            );
            assert_eq!(
                b.contains("href=\"/settings#s-push\"") && b.contains("去设置推送渠道"),
                want_push
            );
            if want_llm {
                assert!(b.contains("模型服务未就绪：还没有模型服务"));
            }
            if want_push {
                assert!(b.contains("没有可用的推送渠道"));
            }
            if !want_llm && !want_push {
                assert!(!b.contains("id=\"first-run\""));
            }
        }
    }

    #[tokio::test]
    async fn push_hint_when_no_channel_ok() {
        let none = json!({"providers":[], "assignments": []});
        let f = fx_with(true, Value::Null, None, none.clone(), "unbound").await;
        let (_, b) = f.get("/settings/push").await;
        assert!(b.contains("id=\"push-hint\"") && b.contains("没有可用的推送渠道"));
        assert!(b.contains("企业微信机器人：未绑定"), "{b}");
        let f = fx_with(true, Value::Null, None, none, "ok").await;
        let (_, b) = f.get("/settings/push").await;
        assert!(!b.contains("push-hint"));
    }

    #[tokio::test]
    async fn push_switches_keep_disabled_channels_visible_and_use_guarded_endpoint() {
        let f = fx(true, Value::Null, None).await;
        let enabled = Arc::new(Mutex::new(false));
        let state = enabled.clone();
        f._hub.register_fn("hub", "hub-admin", move |method, params| {
            let state = state.clone();
            async move {
                match method.as_str() {
                    "plugins" => { let current = *state.lock().unwrap(); let plugins = ["dispatch","wechat-ilink","wecom-webhook"].map(|name| json!({
                        "name":name,"version":"1","kind":"official","status":if current {"运行中"} else {"已停用"},
                        "enabled":current,"permissions":[],"missing":[],"granted":true,"last_error":null
                    })); Ok(json!({"plugins":plugins})) },
                    "set_enabled" => { *state.lock().unwrap() = params["enabled"].as_bool().unwrap(); Ok(json!({})) },
                    "config_get" => Ok(json!({"name":"dispatch","config":{}})),
                    _ => Ok(json!({})),
                }
            }
        });
        let (_, body) = f.get("/settings/push").await;
        for name in ["dispatch", "wechat-ilink", "wecom-webhook"] {
            assert!(body.contains(&format!("data-plugin-switch=\"{name}\"")));
        }
        assert!(body.contains("渠道已停用，打开上方开关即可恢复"));
        let (status, body) = f
            .post(
                "/settings/push/plugins/dispatch/enabled",
                &[("enabled", "true")],
            )
            .await;
        assert_eq!(status, 200);
        assert!(*enabled.lock().unwrap());
        assert!(body.contains("aria-checked=\"true\""));
        assert!(body.contains("发送队列"));
        let (status, _) = f
            .post(
                "/settings/push/plugins/store/enabled",
                &[("enabled", "false")],
            )
            .await;
        assert_eq!(status, 400);
        assert!(*enabled.lock().unwrap());
        let denied = fx(false, Value::Null, None).await;
        assert_eq!(
            denied
                .post(
                    "/settings/push/plugins/dispatch/enabled",
                    &[("enabled", "true")]
                )
                .await
                .0,
            403
        );
    }

    #[tokio::test]
    async fn sources_pause_retry_delete_and_save() {
        let f = fx(true, Value::Null, None).await;
        let (_, b) = f.get("/settings/sources").await;
        assert!(b.contains("SEC") && b.contains("正常"));
        f.post("/settings/sources/rss%3Asec/pause", &[("paused", "true")])
            .await;
        assert!(f.called("feed-admin.set_paused", "\"paused\":true"));
        f.post("/settings/sources/rss%3Asec/retry", &[]).await;
        assert!(f.called("source-status.retry", "rss:sec"));
        f.post("/settings/sources/rss%3Asec/delete", &[]).await;
        assert!(f.called("feed-admin.delete", "rss:sec"));
        f.post(
            "/settings/sources/save",
            &[("name", "新源"), ("url", "https://n.example/rss")],
        )
        .await;
        assert!(f.called("feed-admin.save", "https://n.example/rss"));
    }

    #[tokio::test]
    async fn x_card_shows_state_and_actions_reach_service() {
        let f = fx(true, Value::Null, None).await;
        let (_, b) = f.get("/settings/sources").await;
        assert!(
            b.contains("已登录") && b.contains("Edge") && b.contains("@binance"),
            "{b}"
        );
        assert!(b.contains("重新登录") && b.contains("@okx") && b.contains("已暂停"));
        f.post("/settings/x/enabled", &[("enabled", "false")]).await;
        assert!(f.called("hub-admin.set_enabled", "x-browser"));
        let denied = fx(false, Value::Null, None).await;
        assert_eq!(
            denied
                .post("/settings/x/enabled", &[("enabled", "true")])
                .await
                .0,
            403
        );
        f.post("/settings/x/login", &[]).await;
        assert!(f.called("x-admin.login", ""));
        f.post("/settings/x/add", &[("handle", "@foo_1")]).await;
        assert!(f.called("x-admin.add", "@foo_1"));
        f.post("/settings/x/okx/pause", &[("paused", "false")])
            .await;
        assert!(f.called("x-admin.set_paused", "\"paused\":false"));
        f.post("/settings/x/binance/delete", &[]).await;
        assert!(f.called("x-admin.delete", "binance"));
    }

    #[tokio::test]
    async fn x_card_without_runtime() {
        let hub = TestHub::new();
        hub.register_fn("auth", "auth", |_m, _p| async {
            Ok(json!({"allowed": true}))
        });
        let ctx = hub.context("web", json!({}), test_http_client());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::router(ctx);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let r = test_http_client()
            .get(format!("http://{addr}/settings/sources"))
            .send()
            .await
            .unwrap();
        let b = r.text().await.unwrap();
        assert!(b.contains("X 采集插件未运行"), "{b}");
    }

    #[tokio::test]
    async fn push_rules_and_webhook_secret() {
        let f = fx(true, Value::Null, None).await;
        let (_, b) = f.get("/settings/push").await;
        assert!(b.contains("重点事件推送") && b.contains("/#push") && b.contains("企业微信"));
        let (_, b) = f.post("/settings/push/webhook", &[("value", HOOK)]).await;
        assert!(!b.contains(HOOK), "webhook 不得回显");
        assert!(f.called("secrets.set", "wecom-webhook.webhook_key"));
        let (_, b) = f.get("/settings/push?hook=1").await;
        assert!(!b.contains(HOOK));
        f.post(
            "/settings/push/rules",
            &[("urgent", "on"), ("digest", "on"), ("quiet", "on")],
        )
        .await;
        assert!(f.called("hub-admin.config_set", "\"push_urgent\":true"));
        assert!(f.called("hub-admin.config_set", "\"push_important\":false"));
        assert!(f.called("hub-admin.config_set", "23:00-07:00"));
    }

    #[tokio::test]
    async fn system_proxy_and_desktop_options() {
        let f = fx(true, Value::Null, None).await;
        let (_, network) = f.get("/settings/network").await;
        assert!(network.contains("连接方式") && !network.contains("开机自启"));
        assert!(network.contains("hx-target=\"#sec-net\""));
        let (_, system) = f.get("/settings/system").await;
        assert!(system.contains("开机自启") && !system.contains("连接方式"));
        let (_, shell) = f.get("/settings").await;
        assert!(shell.contains("id=\"s-net\"") && shell.contains("id=\"s-sys\""));
        assert!(!shell.contains("网络与系统"));
        let (_, b) = f
            .post(
                "/settings/system/proxy",
                &[("mode", "proxy"), ("url", "ftp://x")],
            )
            .await;
        assert!(b.contains("socks5"));
        assert!(!f.called("hub-admin.app_set", "proxy"));
        f.post(
            "/settings/system/proxy",
            &[("mode", "proxy"), ("url", "http://127.0.0.1:7890")],
        )
        .await;
        assert!(f.called("hub-admin.app_set", "http://127.0.0.1:7890"));
        f.post("/settings/system/proxy", &[("mode", "direct")])
            .await;
        assert!(f.called("hub-admin.app_set", "\"value\":\"\""));
        // 非桌面外壳不能改开机自启
        let (_, b) = f.post("/settings/system/app", &[("autostart", "on")]).await;
        assert!(b.contains("桌面版"));
        assert!(!f.called("hub-admin.app_set", "autostart"));

        let d = fx(true, json!("desktop"), None).await;
        d.post("/settings/system/app", &[("autostart", "on")]).await;
        assert!(d.called("hub-admin.app_set", "autostart"));
        assert!(d.called("hub-admin.app_set", "close_to_tray"));
    }

    #[tokio::test]
    async fn proxy_test_uses_injected_url() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/generate_204", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        let f = fx(true, Value::Null, Some(&url)).await;
        let (s, b) = f.post("/settings/system/proxy/test", &[]).await;
        assert_eq!(s, 200);
        assert!(b.contains("连接正常"), "{b}");
        // 目标不可达：只给通用失败说明，不带地址
        let g = fx(true, Value::Null, Some("http://127.0.0.1:1/x")).await;
        let (_, b) = g.post("/settings/system/proxy/test", &[]).await;
        assert!(b.contains("失败") && !b.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn settings_writes_denied_without_permission() {
        let f = fx(false, Value::Null, None).await;
        for p in [
            "/settings/llm/save",
            "/settings/llm/assign",
            "/settings/llm/p1/test",
            "/settings/sources/save",
            "/settings/sources/x/retry",
            "/settings/push/webhook",
            "/settings/push/rules",
            "/settings/system/proxy",
            "/settings/system/proxy/test",
            "/settings/system/app",
            "/settings/plugins/x/restart",
        ] {
            let all = [
                ("name", "a"),
                ("base_url", "https://a.example"),
                ("url", "https://a.example"),
                ("value", "a"),
                ("purpose", "screen"),
                ("provider_id", "p1"),
                ("paused", "true"),
            ];
            let (s, _) = f.post(p, &all).await;
            assert_eq!(s, 403, "{p}");
        }
        assert!(!f.called("llm.", ""));
        assert!(!f.called("secrets.set", ""));
    }
}

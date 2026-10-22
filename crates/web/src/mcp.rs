//! One MCP tool catalogue for stdio clients and the application's own model.
//! Tools prepare durable plans; only the application's confirmation UI executes writes.
use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Request};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::agent::AskRequest;
use plugin_sdk::contracts::hub_admin::HubAdminClient;
use plugin_sdk::contracts::llm::{ChatMessage, ChatRequest, LlmClient, Tier};
use plugin_sdk::contracts::source::FeedAdminClient;
use plugin_sdk::contracts::x::XAdminClient;
use plugin_sdk::contracts::{ServiceError, OWNER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tower::ServiceExt;

use crate::{check, err_response, feed::db, AppState, Shared};
#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;

struct Tool {
    name: &'static str,
    title: &'static str,
    path: &'static str,
    fields: &'static str,
    secret: &'static str,
    json: bool,
}
macro_rules! tool {
    ($n:literal,$t:literal,$p:literal,$f:literal) => {
        Tool {
            name: $n,
            title: $t,
            path: $p,
            fields: $f,
            secret: "",
            json: false,
        }
    };
    ($n:literal,$t:literal,$p:literal,$f:literal,$s:literal,$j:literal) => {
        Tool {
            name: $n,
            title: $t,
            path: $p,
            fields: $f,
            secret: $s,
            json: $j,
        }
    };
}
const TOOLS: &[Tool] = &[
    tool!(
        "source_save",
        "添加或编辑RSS信源",
        "/settings/sources/save",
        "id,name,url"
    ),
    tool!(
        "source_pause",
        "暂停或恢复信源",
        "/settings/sources/{id}/pause",
        "id,paused"
    ),
    tool!(
        "source_delete",
        "删除RSS信源",
        "/settings/sources/{id}/delete",
        "id"
    ),
    tool!(
        "source_retry",
        "立即重新采集信源",
        "/settings/sources/{id}/retry",
        "id"
    ),
    tool!("x_add", "关注X账号", "/settings/x/add", "handle"),
    tool!(
        "x_pause",
        "暂停或恢复X账号",
        "/settings/x/{handle}/pause",
        "handle,paused"
    ),
    tool!(
        "x_delete",
        "取消关注X账号",
        "/settings/x/{handle}/delete",
        "handle"
    ),
    tool!("x_login", "打开X登录窗口", "/settings/x/login", ""),
    tool!("x_open", "打开X首页", "/settings/x/open", ""),
    tool!(
        "model_save",
        "添加或编辑模型服务",
        "/settings/llm/save",
        "id,name,kind,base_url,models",
        "api_key",
        false
    ),
    tool!(
        "model_assign",
        "分配模型用途",
        "/settings/llm/assign",
        "purpose,provider_id,model"
    ),
    tool!(
        "model_default",
        "设置默认模型服务",
        "/settings/llm/{id}/default",
        "id"
    ),
    tool!(
        "model_test",
        "测试模型连接",
        "/settings/llm/{id}/test",
        "id"
    ),
    tool!(
        "model_delete",
        "删除模型服务",
        "/settings/llm/{id}/delete",
        "id"
    ),
    tool!(
        "plugin_enabled",
        "启停插件",
        "/api/v1/plugins/{name}/enabled",
        "name,enabled"
    ),
    tool!(
        "plugin_restart",
        "重启插件",
        "/settings/plugins/{name}/restart",
        "name"
    ),
    tool!(
        "plugin_grant",
        "确认插件权限",
        "/api/v1/plugins/{name}/grant",
        "name"
    ),
    tool!(
        "plugin_config",
        "配置插件（fields填写配置键值）",
        "/settings/plugins/{name}/config",
        "name,fields"
    ),
    tool!(
        "watch_assets",
        "保存完整的关注币种列表",
        "/api/v1/prefs/watch_assets",
        "assets",
        "",
        true
    ),
    tool!(
        "push_settings",
        "保存推送范围及合并间隔",
        "/api/v1/push/settings",
        "enabled,scope,assets,interval",
        "",
        true
    ),
    tool!(
        "channel_test",
        "发送渠道测试消息（占用真实额度）",
        "/api/v1/channels/{id}/test",
        "id"
    ),
    tool!(
        "channel_bind",
        "绑定微信渠道",
        "/api/v1/channels/{id}/bind",
        "id"
    ),
    tool!(
        "webhook_save",
        "设置企业微信Webhook",
        "/settings/push/webhook",
        "",
        "value",
        false
    ),
    tool!(
        "outbox_retry",
        "重试发送队列任务",
        "/api/v1/outbox/{id}/retry",
        "id"
    ),
    tool!(
        "proxy_save",
        "设置网络代理或直连",
        "/settings/network/proxy",
        "mode,url"
    ),
    tool!(
        "proxy_test",
        "测试网络连接",
        "/settings/network/proxy/test",
        ""
    ),
    tool!(
        "app_settings",
        "设置开机自启和关闭到托盘",
        "/settings/system/app",
        "autostart,close_to_tray"
    ),
    tool!(
        "update_action",
        "检查或安装签名更新（action为check/install）",
        "/settings/system/update",
        "action"
    ),
    tool!(
        "auto_update",
        "开关自动检查更新",
        "/settings/system/auto-update",
        "enabled"
    ),
    tool!(
        "secret_save",
        "设置应用密钥（不返回密钥）",
        "/api/v1/secrets",
        "key",
        "value",
        true
    ),
    tool!(
        "secret_delete",
        "删除应用密钥",
        "/api/v1/secrets/delete",
        "key",
        "",
        true
    ),
    tool!(
        "rules_save",
        "更新消息分析规则",
        "/api/v1/rules",
        "text",
        "",
        true
    ),
    tool!(
        "item_read",
        "标记消息已读",
        "/api/v1/items/{id}/read",
        "id",
        "",
        true
    ),
    tool!(
        "item_star",
        "切换消息关注状态",
        "/api/v1/items/{id}/star",
        "id",
        "",
        true
    ),
    tool!(
        "item_level",
        "设置消息级别",
        "/api/v1/items/{id}/level",
        "id,level",
        "",
        true
    ),
    tool!(
        "items_read_all",
        "将默认筛选下的全部消息标为已读",
        "/api/v1/items/read_all",
        "",
        "",
        true
    ),
    tool!(
        "session_rename",
        "重命名会话",
        "/api/v1/chat/{id}/rename",
        "id,title",
        "",
        true
    ),
    tool!(
        "session_delete",
        "删除会话",
        "/api/v1/chat/{id}/delete",
        "id",
        "",
        true
    ),
];

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Plan {
    id: String,
    tool: String,
    arguments: Value,
    origin: String,
    session_id: Option<i64>,
    created_at: i64,
    status: String,
    result: Option<String>,
}
fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}
fn spec(name: &str) -> Result<&'static Tool, ServiceError> {
    TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| ServiceError::invalid_params("未知MCP工具"))
}
fn esc(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub(crate) fn contains_secret(text: &str) -> bool {
    text.contains("sk-") || text.contains("key=") || text.contains("Bearer ")
}
fn validate(tool: &Tool, args: &Value) -> Result<(), ServiceError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ServiceError::invalid_params("工具参数必须是对象"))?;
    if obj.len() > 30 || args.to_string().len() > 16_000 {
        return Err(ServiceError::invalid_params("操作参数过长"));
    }
    for (key, value) in obj {
        if !tool.fields.split(',').any(|f| f == key) {
            return Err(ServiceError::invalid_params(format!("不支持的参数 {key}")));
        }
        if contains_secret(&value.to_string()) {
            return Err(ServiceError::invalid_params(
                "敏感信息请在应用确认面板的安全输入框填写",
            ));
        }
    }
    Ok(())
}
fn catalogue() -> Vec<Value> {
    let mut out = vec![
        json!({"name":"app_inspect","description":"读取配置与状态，密钥仅返回是否已设置","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"plan_status","description":"读取计划状态；pending须由用户在应用AI与MCP页面确认，外部AI不能自行授权","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for t in TOOLS {
        let properties: serde_json::Map<String, Value> = t
            .fields
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|f| (f.into(), json!({"description":f})))
            .collect();
        out.push(json!({"name":t.name,"description":format!("{}。只创建待确认计划，不立即执行。{}",t.title,if t.secret.is_empty(){""}else{"密钥由用户在应用确认面板填写，不要传给AI。"}),"inputSchema":{"type":"object","properties":properties,"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false}}));
    }
    out
}
async fn save_plan(st: &AppState, p: &Plan) -> Result<(), ServiceError> {
    db(st)
        .set_pref(
            &format!("mcp_plan_{}", p.id),
            &serde_json::to_string(p).unwrap_or_default(),
        )
        .await
}
async fn load_plan(st: &AppState, id: &str) -> Result<Plan, ServiceError> {
    if id.len() > 100 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ServiceError::invalid_params("无效计划编号"));
    }
    let raw = db(st)
        .pref(&format!("mcp_plan_{id}"))
        .await?
        .ok_or_else(|| ServiceError::invalid_params("计划不存在"))?;
    serde_json::from_str(&raw).map_err(|_| ServiceError::internal("计划读取失败"))
}
pub(crate) async fn pending(st: &AppState) -> Value {
    #[derive(Deserialize)]
    struct Row {
        value: String,
    }
    let rows=st.ctx.store().query_as::<Row>("SELECT value FROM prefs WHERE user_id=? AND key LIKE 'mcp_plan_%' ORDER BY key DESC LIMIT 100",vec![json!(OWNER)]).await.unwrap_or_default();
    let plans: Vec<Plan> = rows
        .into_iter()
        .filter_map(|r| serde_json::from_str(&r.value).ok())
        .collect();
    json!({"plans":plans.iter().map(plan_view).collect::<Vec<_>>(),"tools":TOOLS.len()+2})
}
fn plan_view(p: &Plan) -> Value {
    let t = spec(&p.tool).ok();
    json!({"id":p.id,"tool":p.tool,"title":t.map(|t|t.title),"arguments":p.arguments,"status":if p.status=="pending"&&now()-p.created_at>=3600{"expired"}else{&p.status},"result":p.result,"origin":p.origin,"secret":t.map(|t|t.secret).unwrap_or("")})
}
async fn inspect(st: &AppState) -> Value {
    let admin = HubAdminClient(st.ctx.service("hub-admin"));
    let llm = LlmClient(st.ctx.service("llm"));
    let feed = FeedAdminClient(st.ctx.service("feed-admin"));
    let x_admin = XAdminClient(st.ctx.service("x-admin"));
    let dispatch = st.ctx.service("dispatch");
    let empty = json!({});
    let (models, feeds, x, plugins, push) = tokio::join!(
        llm.providers(),
        feed.list(),
        x_admin.list(),
        admin.plugins(),
        plugin_sdk::contracts::call_typed::<_, Value>(&*dispatch, "focus_settings", &empty)
    );
    let mut data = json!({"watch_assets":db(st).watch_assets().await,"models":models.ok(),"feeds":feeds.ok(),"x_accounts":x.ok(),"plugins":plugins.ok().map(|v|v.into_iter().map(|p|json!({"name":p.name,"enabled":p.enabled,"status":p.status})).collect::<Vec<_>>()),"push":push.ok()});
    for key in ["autostart", "close_to_tray", "auto_update", "proxy"] {
        data["app_settings"][key] = admin.app_get(key).await.unwrap_or(Value::Null);
    }
    // Never return URL credentials or query-string keys to models/clients.
    scrub(&mut data);
    data
}
fn scrub(value: &mut Value) {
    match value {
        Value::Object(m) => {
            for (k, v) in m {
                if matches!(k.as_str(), "api_key" | "password" | "token" | "webhook_key") {
                    *v = json!("[已隐藏]")
                } else {
                    scrub(v)
                }
            }
        }
        Value::Array(v) => v.iter_mut().for_each(scrub),
        Value::String(s) if contains_secret(s) || s.contains('@') && s.contains("://") => {
            *s = "[包含敏感信息，已隐藏]".into()
        }
        _ => {}
    }
}
pub(crate) async fn call_tool(
    st: &AppState,
    name: &str,
    args: Value,
    origin: &str,
    sid: Option<i64>,
) -> Result<Value, ServiceError> {
    match name {
        "app_inspect" => Ok(inspect(st).await),
        "plan_status" => Ok(plan_view(
            &load_plan(st, args["id"].as_str().unwrap_or("")).await?,
        )),
        _ => {
            let t = spec(name)?;
            validate(t, &args)?;
            let p = Plan {
                id: st.chat.new_id(),
                tool: name.into(),
                arguments: args,
                origin: origin.into(),
                session_id: sid,
                created_at: now(),
                status: "pending".into(),
                result: None,
            };
            save_plan(st, &p).await?;
            Ok(plan_view(&p))
        }
    }
}
pub(crate) async fn rpc(
    State(st): Shared,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    if headers.contains_key("origin") {
        return (axum::http::StatusCode::FORBIDDEN, "请通过本机MCP客户端连接").into_response();
    }
    let id = request.get("id").cloned();
    let method = request["method"].as_str().unwrap_or("");
    if id.is_none() {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }
    let result = match method {
        "initialize" => Ok(
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"crypto-news","version":env!("CARGO_PKG_VERSION")},"instructions":"查询可直接使用；修改工具只生成计划，须由用户在应用中确认。密钥不进入MCP参数。"}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":catalogue()})),
        "tools/call" => {
            let p = &request["params"];
            match call_tool(
                &st,
                p["name"].as_str().unwrap_or(""),
                p.get("arguments").cloned().unwrap_or(json!({})),
                "external",
                None,
            )
            .await
            {
                Ok(data) => Ok(
                    json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data,"isError":false}),
                ),
                Err(e) => Ok(json!({"content":[{"type":"text","text":e.message}],"isError":true})),
            }
        }
        _ => Err(ServiceError::method_not_found(method)),
    };
    Json(match result {
        Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":e.message}}),
    })
    .into_response()
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn request_for(
    t: &Tool,
    arguments: &Value,
    secrets: &HashMap<String, String>,
) -> Result<Request<Body>, ServiceError> {
    let mut path = t.path.to_string();
    let mut args = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| ServiceError::invalid_params("参数无效"))?;
    for field in ["id", "name", "handle"] {
        let placeholder = format!("{{{field}}}");
        if path.contains(&placeholder) {
            let value = args
                .remove(field)
                .ok_or_else(|| ServiceError::invalid_params(format!("缺少 {field}")))?;
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(ServiceError::invalid_params("目标标识无效"));
            }
            path = path.replace(&placeholder, &encode(&value));
        }
    }
    if !t.secret.is_empty() {
        if let Some(v) = secrets.get(t.secret).filter(|v| !v.is_empty()) {
            args.insert(t.secret.into(), json!(v));
        } else if matches!(t.name, "webhook_save" | "secret_save") {
            return Err(ServiceError::invalid_params("请填写安全输入框"));
        }
    }
    if let Some(Value::Object(fields)) = args.remove("fields") {
        for (k, v) in fields {
            args.insert(k, v);
        }
    }
    let (content_type, body) = if t.json {
        ("application/json", Value::Object(args).to_string())
    } else {
        (
            "application/x-www-form-urlencoded",
            args.iter()
                .filter_map(|(k, v)| {
                    if (matches!(k.as_str(), "autostart" | "close_to_tray")
                        || t.name == "plugin_config")
                        && v == &json!(false)
                    {
                        return None;
                    }
                    let value = match v {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                        _ => v.to_string(),
                    };
                    Some(format!("{}={}", encode(k), encode(&value)))
                })
                .collect::<Vec<_>>()
                .join("&"),
        )
    };
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", content_type)
        .body(Body::from(body))
        .map_err(|_| ServiceError::invalid_params("无法构造操作请求"))
}
#[derive(Deserialize, Default)]
pub(crate) struct Confirmation {
    #[serde(default)]
    secrets: HashMap<String, String>,
}
pub(crate) async fn confirm(
    State(st): Shared,
    Path(id): Path<String>,
    Json(input): Json<Confirmation>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let _guard = st.chat.operations.lock().await;
    let mut p = match load_plan(&st, &id).await {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    if p.status != "pending" || now() - p.created_at >= 3600 {
        return err_response(ServiceError::invalid_params(
            "计划已处理或已过期，请重新发起",
        ));
    }
    let t = match spec(&p.tool) {
        Ok(t) => t,
        Err(e) => return err_response(e),
    };
    let mut arguments = p.arguments.clone();
    if t.name == "app_settings" {
        let admin = HubAdminClient(st.ctx.service("hub-admin"));
        for (key, default) in [("autostart", false), ("close_to_tray", true)] {
            if arguments.get(key).is_none() {
                arguments[key] = admin
                    .app_get(key)
                    .await
                    .ok()
                    .and_then(|v| v.as_bool())
                    .map(|v| json!(v))
                    .unwrap_or(json!(default));
            }
        }
    }
    if t.name == "plugin_config" {
        let name = arguments["name"].as_str().unwrap_or("");
        if let Ok(config) = HubAdminClient(st.ctx.service("hub-admin"))
            .config_get(name)
            .await
        {
            let mut fields = config.config.as_object().cloned().unwrap_or_default();
            if let Some(changes) = arguments["fields"].as_object() {
                fields.extend(changes.clone());
            }
            arguments["fields"] = json!(fields);
        }
    }
    let request = match request_for(t, &arguments, &input.secrets) {
        Ok(r) => r,
        Err(e) => return err_response(e),
    };
    p.status = "running".into();
    if let Err(e) = save_plan(&st, &p).await {
        return err_response(e);
    }
    let response = match crate::business_routes(st.clone()).oneshot(request).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 512_000)
        .await
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&bytes);
    let failed = !status.is_success()
        || body.contains("fl bad")
        || body.contains("s-bad")
        || body.contains("<p class=\"note\">");
    p.status = if failed { "failed" } else { "completed" }.into();
    p.result = Some(
        if failed {
            "应用校验未通过或服务不可用，请检查对应设置；未自动重试。"
        } else {
            "应用已处理该操作，请核对对应设置。"
        }
        .into(),
    );
    if let Err(e) = save_plan(&st, &p).await {
        return err_response(e);
    }
    *st.status.0.lock().await = None;
    let mut result = plan_view(&p);
    if !failed && t.name == "channel_bind" {
        result["ui_html"] = json!(body.as_ref());
    }
    Json(result).into_response()
}
pub(crate) async fn cancel(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let _guard = st.chat.operations.lock().await;
    let mut p = match load_plan(&st, &id).await {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    if p.status != "pending" {
        return err_response(ServiceError::invalid_params("计划已处理"));
    }
    p.status = "cancelled".into();
    if let Err(e) = save_plan(&st, &p).await {
        return err_response(e);
    }
    Json(plan_view(&p)).into_response()
}
pub(crate) async fn plans(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    Json(pending(&st).await).into_response()
}
pub(crate) async fn settings(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let command = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "desktop.exe".into());
    let config = serde_json::to_string_pretty(
        &json!({"mcpServers":{"crypto-news":{"command":command,"args":["mcp"]}}}),
    )
    .unwrap_or_default();
    crate::render("settings_mcp.html", minijinja::context! { config=>config })
}

pub(crate) fn operation_html(operation: &Value) -> String {
    let mut out = String::new();
    if let Some(answer) = operation["answer"].as_str() {
        out.push_str(&format!("<p>{}</p>", esc(answer)));
    }
    if let Some(plans) = operation["plans"].as_array() {
        for p in plans {
            out.push_str(&format!("<section class=\"operation-plan\" data-operation-plan=\"{}\"><b>{}</b><pre>{}</pre><div data-operation-controls>",esc(p["id"].as_str().unwrap_or("")),esc(p["title"].as_str().unwrap_or("待确认操作")),esc(&serde_json::to_string_pretty(&p["arguments"]).unwrap_or_default())));
            if let Some(secret) = p["secret"].as_str().filter(|s| !s.is_empty()) {
                out.push_str(&format!("<input type=\"password\" data-operation-secret=\"{}\" aria-label=\"安全填写密钥\" placeholder=\"密钥（仅本机提交，不发送给模型）\" autocomplete=\"new-password\">",esc(secret)));
            }
            out.push_str("<button type=\"button\" data-operation-cancel>取消</button><button type=\"button\" class=\"primary\" data-operation-confirm>确认执行</button></div><p data-operation-result role=\"status\">尚未修改配置，确认后执行。</p></section>");
        }
    }
    out
}
pub(crate) async fn ask(st: &Arc<AppState>, sid: i64, request: &AskRequest) -> Option<Value> {
    let q = request.question.to_lowercase();
    if ![
        "配置",
        "设置",
        "信源",
        "rss",
        "模型",
        "插件",
        "推送",
        "关注",
        "代理",
        "队列",
        "自启",
        "托盘",
        "mcp",
        "x账号",
        "x 账号",
        "会话重命名",
    ]
    .iter()
    .any(|k| q.contains(k))
    {
        return None;
    }
    let result=async{
        let snapshot=inspect(st).await;
        let prompt=format!("你是本应用的MCP客户端。只根据用户的明确意图选择工具。普通新闻分析返回{{\"mode\":\"analysis\"}}。配置查询根据给定快照简要回答。修改只能调用清单工具创建待确认计划，不得声称已执行。不要猜测目标ID，不明确就提问。watch_assets必须提交完整assets逗号分隔字符串；修改时保留用户未要求删除的已关注项。paused/enabled使用布尔值。涉及API密钥，留给确认面板，不询问或输出密钥。最多3个工具。返回JSON:{{\"mode\":\"operation\",\"answer\":\"中文简述\",\"tools\":[{{\"name\":\"工具名称\",\"arguments\":{{}}}}]}}。清单：{}。当前只读快照：{}",serde_json::to_string(&catalogue()).unwrap_or_default(),snapshot);
        let response=LlmClient(st.ctx.service("llm")).chat(&ChatRequest{messages:vec![ChatMessage::system(prompt),ChatMessage::user(&request.question)],tier:Tier::Large,json:true,max_tokens:Some(1800),temperature:Some(0.0),purpose:Some("app_mcp".into())}).await?;
        let value:Value=serde_json::from_str(&response.content).map_err(|_|ServiceError::internal("模型未返回有效操作计划，请重试"))?;
        if value["mode"]=="analysis"{return Ok(None)}
        let mut plans=Vec::new();for call in value["tools"].as_array().unwrap_or(&vec![]).iter().take(3){let data=call_tool(st,call["name"].as_str().unwrap_or(""),call.get("arguments").cloned().unwrap_or(json!({})),"internal",Some(sid)).await?;if data.get("id").is_some(){plans.push(data)}}
        Ok::<_,ServiceError>(Some(json!({"answer":value["answer"].as_str().unwrap_or("请查看待确认操作。"),"operation":{"answer":value["answer"],"plans":plans}})))
    }.await;
    match result {
        Ok(v) => v,
        Err(e) => Some(
            json!({"answer":format!("应用操作暂未执行：{}",e.message),"operation":{"answer":format!("应用操作暂未执行：{}",e.message),"plans":[]}}),
        ),
    }
}

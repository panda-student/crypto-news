//! Local MCP transport and confirmation UI. Domain operations live in app-operations.
use crate::{check, err_response, AppState, Shared};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::agent::{AgentClient, AskRequest, OperateRequest};
pub(crate) use plugin_sdk::contracts::operations::{contains_secret, Confirmation};
use plugin_sdk::contracts::operations::{OperationsClient, PrepareRequest, SERVICE};
use plugin_sdk::ServiceError;
use serde_json::{json, Value};
use std::sync::Arc;
#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
fn client(st: &AppState) -> OperationsClient {
    OperationsClient(st.ctx.service(SERVICE))
}
pub(crate) async fn pending(st: &AppState) -> Result<Value, ServiceError> {
    client(st).plans().await
}
pub(crate) async fn call_tool(
    st: &AppState,
    name: &str,
    args: Value,
    _origin: &str,
    sid: Option<i64>,
) -> Result<Value, ServiceError> {
    match name {
        "app_inspect" => client(st).inspect().await,
        "plan_status" => client(st).status(args["id"].as_str().unwrap_or("")).await,
        _ => {
            client(st)
                .prepare(&PrepareRequest {
                    tool: name.into(),
                    arguments: args,
                    session_id: sid,
                })
                .await
        }
    }
}
pub(crate) async fn confirm(
    State(st): Shared,
    Path(id): Path<String>,
    Json(input): Json<Confirmation>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    match client(&st).confirm(&id, input).await {
        Ok(mut v) => {
            *st.status.0.lock().await = None;
            if let Some(binding) = v.as_object_mut().and_then(|m| m.remove("binding")) {
                if let Ok(info) = serde_json::from_value(binding) {
                    let channel = v["arguments"]["id"].as_str().unwrap_or("");
                    let response = crate::pages::bind_fragment(channel, &info);
                    if let Ok(bytes) = axum::body::to_bytes(response.into_body(), 512_000).await {
                        v["ui_html"] = json!(String::from_utf8_lossy(&bytes));
                    }
                }
            }
            Json(v).into_response()
        }
        Err(e) => err_response(e),
    }
}
pub(crate) async fn cancel(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    match client(&st).cancel(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}
pub(crate) async fn plans(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    match client(&st).plans().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}
pub(crate) async fn ask(st: &Arc<AppState>, sid: i64, request: &AskRequest) -> Option<Value> {
    // If the optional planner is unavailable, the normal question path reports
    // the Agent's availability using the existing chat error UI.
    AgentClient(st.ctx.service("agent"))
        .operate(&OperateRequest {
            session_id: sid,
            request: request.clone(),
        })
        .await
        .unwrap_or_default()
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
        "tools/list" => client(&st)
            .catalogue()
            .await
            .map(|tools| json!({"tools":tools})),
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
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":{"code":e.code,"message":e.message}}),
    })
    .into_response()
}

fn esc(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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
    if let Some(reads) = operation["reads"].as_array() {
        for read in reads {
            out.push_str(&format!(
                "<pre>{}</pre>",
                esc(&serde_json::to_string_pretty(read).unwrap_or_default())
            ));
        }
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

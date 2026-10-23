use super::*;
use crate::feed::db;
use axum::body::to_bytes;
use plugin_sdk::contracts::operations::OperationsClient;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::Plugin;

async fn fixture_without_operations(allowed: bool) -> (Arc<AppState>, Arc<TestHub>) {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    hub.register_fn("auth", "auth", move |_, _| async move {
        Ok(json!({"allowed":allowed}))
    });
    let ctx = hub.context("web", json!({}), test_http_client());
    ctx.store().migrate(1, crate::data::SCHEMA).await.unwrap();
    ctx.store()
        .migrate(2, crate::data::CONVERSATION_SCHEMA)
        .await
        .unwrap();
    let st = Arc::new(AppState {
        ctx,
        opener: Arc::new(crate::opener::SystemOpener),
        chat: crate::chat::ChatState::default(),
        status: crate::status::Coordinator::default(),
    });
    crate::workspace::register(&st).unwrap();
    (st, hub)
}
async fn fixture(allowed: bool) -> (Arc<AppState>, Arc<TestHub>) {
    let (st, hub) = fixture_without_operations(allowed).await;
    operations::OperationsPlugin
        .start(hub.context("operations", json!({}), test_http_client()))
        .await
        .unwrap();
    (st, hub)
}
async fn json_response(r: Response) -> Value {
    serde_json::from_slice(&to_bytes(r.into_body(), 1_000_000).await.unwrap()).unwrap()
}
fn empty() -> Confirmation {
    Confirmation::default()
}
async fn tool(st: &AppState, name: &str, args: Value) -> Value {
    call_tool(st, name, args, "external", None).await.unwrap()
}

#[tokio::test]
async fn external_plan_does_not_write_until_confirmed_and_cannot_replay() {
    let (st, _hub) = fixture(true).await;
    let original = db(&st).watch_assets().await;
    let p = tool(&st, "watch_assets", json!({"assets":"BTC,ETH,DOGE"})).await;
    let id = p["id"].as_str().unwrap().to_string();
    assert_eq!(db(&st).watch_assets().await, original);
    assert_eq!(client(&st).status(&id).await.unwrap()["origin"], "external");
    let r = confirm(State(st.clone()), Path(id.clone()), Json(empty())).await;
    assert_eq!(r.status(), 200);
    assert_eq!(json_response(r).await["status"], "completed");
    assert_eq!(db(&st).watch_assets().await, vec!["BTC", "ETH", "DOGE"]);
    assert_eq!(
        confirm(State(st), Path(id), Json(empty())).await.status(),
        400
    );
}

#[tokio::test]
async fn cancel_and_expiry_prevent_execution() {
    let (st, hub) = fixture(true).await;
    let original = db(&st).watch_assets().await;
    for expired in [false, true] {
        let p = tool(&st, "watch_assets", json!({"assets":"DOGE"})).await;
        let id = p["id"].as_str().unwrap().to_string();
        if expired {
            let store = hub
                .context("operations", json!({}), test_http_client())
                .store();
            let mut rows = store
                .query(
                    "SELECT value FROM operation_plans WHERE id=?",
                    vec![json!(id)],
                )
                .await
                .unwrap();
            let mut plan: Value =
                serde_json::from_str(rows.remove(0)["value"].as_str().unwrap()).unwrap();
            plan["created_at"] = json!(0);
            store
                .execute(
                    "UPDATE operation_plans SET value=? WHERE id=?",
                    vec![json!(plan.to_string()), json!(id)],
                )
                .await
                .unwrap();
            assert_eq!(client(&st).status(&id).await.unwrap()["status"], "expired");
        } else {
            assert_eq!(
                cancel(State(st.clone()), Path(id.clone())).await.status(),
                200
            );
        }
        assert_eq!(
            confirm(State(st.clone()), Path(id), Json(empty()))
                .await
                .status(),
            400
        );
    }
    assert_eq!(db(&st).watch_assets().await, original);
}

#[tokio::test]
async fn parallel_confirm_executes_at_most_once() {
    let (st, _hub) = fixture(true).await;
    let p = tool(&st, "watch_assets", json!({"assets":"BTC"})).await;
    let id = p["id"].as_str().unwrap().to_string();
    let (a, b) = tokio::join!(
        confirm(State(st.clone()), Path(id.clone()), Json(empty())),
        confirm(State(st.clone()), Path(id), Json(empty()))
    );
    let mut codes = [a.status().as_u16(), b.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 400]);
    assert_eq!(db(&st).watch_assets().await, vec!["BTC"]);
}

#[tokio::test]
async fn rpc_catalogue_negotiation_permission_and_origin() {
    let (st, _hub) = fixture(true).await;
    let init = json_response(
        rpc(
            State(st.clone()),
            HeaderMap::new(),
            Json(json!({"jsonrpc":"2.0","id":1,"method":"initialize"})),
        )
        .await,
    )
    .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "crypto-news");
    let tools = json_response(
        rpc(
            State(st.clone()),
            HeaderMap::new(),
            Json(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})),
        )
        .await,
    )
    .await;
    let names = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(names.len(), 40);
    assert!(names.iter().any(|t| t["name"] == "model_save"));
    assert!(!names.iter().any(|t| t["name"] == "confirm"));
    let p = json_response(rpc(State(st.clone()), HeaderMap::new(), Json(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"watch_assets","arguments":{"assets":"BTC"}}}))).await).await;
    assert_eq!(p["result"]["structuredContent"]["status"], "pending");
    assert!(db(&st).watch_assets().await.is_empty());
    let mut headers = HeaderMap::new();
    headers.insert("origin", "https://evil.test".parse().unwrap());
    assert_eq!(
        rpc(State(st), headers, Json(json!({"id":1})))
            .await
            .status(),
        403
    );
    let (denied, _hub) = fixture(false).await;
    assert_eq!(
        rpc(
            State(denied.clone()),
            HeaderMap::new(),
            Json(json!({"id":1}))
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        confirm(State(denied), Path("unknown".into()), Json(empty()))
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn unsupported_fields_and_secrets_are_not_accepted_from_models() {
    let (st, _hub) = fixture(true).await;
    for (name, args) in [
        ("shell_exec", json!({"command":"anything"})),
        ("watch_assets", json!({"assets":"BTC","path":"/secrets"})),
        ("model_save", json!({"api_key":"not-accepted"})),
        (
            "source_save",
            json!({"url":"https://example.test?key=private"}),
        ),
    ] {
        assert!(call_tool(&st, name, args, "external", None).await.is_err());
    }
    assert!(pending(&st).await.unwrap()["plans"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn internal_real_agent_uses_same_durable_hub_plan() {
    let (st, hub) = fixture(true).await;
    hub.register_fn("llm","llm",|method,_|async move{Ok(match method.as_str(){"providers"=>json!({"providers":[],"assignments":[]}),"chat"=>json!({"content":json!({"mode":"operation","answer":"待确认添加DOGE","tools":[{"name":"watch_assets","arguments":{"assets":"BTC,ETH,SOL,DOGE"}}]}).to_string(),"model":"mock"}),_=>json!({})})});
    agent::AgentPlugin
        .start(hub.context("agent", json!({}), test_http_client()))
        .await
        .unwrap();
    let result = ask(
        &st,
        1,
        &AskRequest {
            question: "关注 DOGE".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let id = result["operation"]["plans"][0]["id"].as_str().unwrap();
    let p = client(&st).status(id).await.unwrap();
    assert_eq!(p["origin"], "internal");
    assert_eq!(p["status"], "pending");
    assert!(!db(&st).watch_assets().await.contains(&"DOGE".to_string()));
    let agent_client = OperationsClient(
        hub.context("agent", json!({}), test_http_client())
            .service(SERVICE),
    );
    assert_eq!(
        agent_client.confirm(id, empty()).await.unwrap_err().code,
        ServiceError::FORBIDDEN
    );
    assert_eq!(
        json_response(confirm(State(st.clone()), Path(id.into()), Json(empty())).await).await
            ["status"],
        "completed"
    );
    assert!(db(&st).watch_assets().await.contains(&"DOGE".to_string()));
    hub.stop().await;
}

#[tokio::test]
async fn domain_validation_failure_is_not_reported_as_success() {
    let (st, _hub) = fixture(true).await;
    let p = tool(&st, "source_save", json!({"name":"","url":"invalid"})).await;
    let r = confirm(
        State(st),
        Path(p["id"].as_str().unwrap().to_string()),
        Json(empty()),
    )
    .await;
    assert_eq!(json_response(r).await["status"], "failed");
}

#[tokio::test]
async fn unavailable_operations_is_reported_in_status_and_http_endpoint() {
    let (st, _hub) = fixture_without_operations(true).await;
    assert_eq!(
        pending(&st).await.unwrap_err().code,
        ServiceError::UNAVAILABLE
    );
    let snapshot = json_response(crate::status::get(State(st.clone())).await).await;
    assert_eq!(snapshot["mcp"]["ok"], false);
    assert!(snapshot["mcp"]["data"].is_null());
    assert!(snapshot["mcp"]["error"].is_string());
    assert_eq!(plans(State(st.clone())).await.status(), 503);
    assert_eq!(
        confirm(State(st.clone()), Path("missing".into()), Json(empty()))
            .await
            .status(),
        503
    );
    let result = json_response(rpc(State(st), HeaderMap::new(), Json(json!({"id":1,"method":"tools/call","params":{"name":"watch_assets","arguments":{"assets":"BTC"}}}))).await).await;
    assert_eq!(result["result"]["isError"], true);
}

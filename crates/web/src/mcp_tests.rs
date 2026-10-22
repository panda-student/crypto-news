use super::*;
use plugin_sdk::testing::{test_http_client, TestHub};

async fn fixture(allowed: bool) -> (Arc<AppState>, Arc<TestHub>) {
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
    (
        Arc::new(AppState {
            ctx,
            opener: Arc::new(crate::opener::SystemOpener),
            chat: crate::chat::ChatState::default(),
            status: crate::status::Coordinator::default(),
        }),
        hub,
    )
}
async fn json_response(r: Response) -> Value {
    serde_json::from_slice(&to_bytes(r.into_body(), 1_000_000).await.unwrap()).unwrap()
}
fn empty() -> Confirmation {
    Confirmation::default()
}

#[tokio::test]
async fn external_plan_does_not_write_until_confirmed_and_cannot_replay() {
    let (st, _hub) = fixture(true).await;
    let original = db(&st).watch_assets().await;
    let p = call_tool(
        &st,
        "watch_assets",
        json!({"assets":"BTC,ETH,DOGE"}),
        "external",
        None,
    )
    .await
    .unwrap();
    let id = p["id"].as_str().unwrap().to_string();
    assert_eq!(db(&st).watch_assets().await, original);
    assert_eq!(load_plan(&st, &id).await.unwrap().origin, "external");
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
    let (st, _hub) = fixture(true).await;
    let original = db(&st).watch_assets().await;
    for expired in [false, true] {
        let p = call_tool(
            &st,
            "watch_assets",
            json!({"assets":"DOGE"}),
            "external",
            None,
        )
        .await
        .unwrap();
        let id = p["id"].as_str().unwrap().to_string();
        if expired {
            let mut plan = load_plan(&st, &id).await.unwrap();
            plan.created_at = 0;
            save_plan(&st, &plan).await.unwrap()
        } else {
            assert_eq!(
                cancel(State(st.clone()), Path(id.clone())).await.status(),
                200
            )
        }
        assert_eq!(
            confirm(State(st.clone()), Path(id), Json(empty()))
                .await
                .status(),
            400
        )
    }
    assert_eq!(db(&st).watch_assets().await, original);
}
#[tokio::test]
async fn parallel_confirm_executes_at_most_once() {
    let (st, _hub) = fixture(true).await;
    let p = call_tool(
        &st,
        "watch_assets",
        json!({"assets":"BTC"}),
        "internal",
        Some(1),
    )
    .await
    .unwrap();
    let id = p["id"].as_str().unwrap().to_string();
    let (a, b) = tokio::join!(
        confirm(State(st.clone()), Path(id.clone()), Json(empty())),
        confirm(State(st), Path(id), Json(empty()))
    );
    let mut codes = [a.status().as_u16(), b.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 400]);
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
    assert!(tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "model_save"));
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
    assert!(call_tool(
        &st,
        "shell_exec",
        json!({"command":"anything"}),
        "external",
        None
    )
    .await
    .is_err());
    assert!(call_tool(
        &st,
        "watch_assets",
        json!({"assets":"BTC","path":"/secrets"}),
        "external",
        None
    )
    .await
    .is_err());
    assert!(call_tool(
        &st,
        "model_save",
        json!({"api_key":"not-accepted"}),
        "external",
        None
    )
    .await
    .is_err());
    assert!(call_tool(
        &st,
        "source_save",
        json!({"url":"https://example.test?key=private"}),
        "external",
        None
    )
    .await
    .is_err());
    let mut data = json!({"api_key":"PRIVATE","url":"https://user:password@host/path"});
    scrub(&mut data);
    assert!(!data.to_string().contains("PRIVATE"));
    assert!(!data.to_string().contains("password@"));
}
#[tokio::test]
async fn internal_model_uses_same_durable_mcp_plan() {
    let (st, hub) = fixture(true).await;
    hub.register_fn("llm","llm",|method,_|async move{Ok(match method.as_str(){"providers"=>json!({"providers":[],"assignments":[]}),"chat"=>json!({"content":json!({"mode":"operation","answer":"待确认添加DOGE","tools":[{"name":"watch_assets","arguments":{"assets":"BTC,ETH,SOL,DOGE"}}]}).to_string(),"model":"mock"}),_=>json!({})})});
    let result = ask(
        &st,
        1,
        &AskRequest {
            question: "关注 DOGE".into(),
            item_id: None,
            item_ids: None,
            history: vec![],
        },
    )
    .await
    .unwrap();
    let id = result["operation"]["plans"][0]["id"].as_str().unwrap();
    let p = load_plan(&st, id).await.unwrap();
    assert_eq!(p.origin, "internal");
    assert_eq!(p.session_id, Some(1));
    assert_eq!(p.status, "pending");
    assert!(!db(&st).watch_assets().await.contains(&"DOGE".to_string()));
}
#[tokio::test]
async fn gui_validation_failure_is_not_reported_as_success() {
    let (st, _hub) = fixture(true).await;
    let p = call_tool(
        &st,
        "source_save",
        json!({"name":"","url":"invalid"}),
        "external",
        None,
    )
    .await
    .unwrap();
    let r = confirm(
        State(st),
        Path(p["id"].as_str().unwrap().to_string()),
        Json(empty()),
    )
    .await;
    assert_eq!(json_response(r).await["status"], "failed");
}

use super::*;
use plugin_sdk::contracts::operations::OperationsClient;
use plugin_sdk::testing::{test_http_client, TestHub};

async fn fixture() -> (Arc<TestHub>, OperationsClient, OperationsClient) {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    hub.register_fn("auth", "auth", |_, _| async { Ok(json!({"allowed":true})) });
    hub.register_fn("web", "workspace-state", |method, _| async move {
        Ok(if method == "legacy_plans" {
            json!({"plans":[]})
        } else {
            json!({"watch_assets":[]})
        })
    });
    OperationsPlugin
        .start(hub.context("operations", json!({}), test_http_client()))
        .await
        .unwrap();
    let web = OperationsClient(
        hub.context("web", json!({}), test_http_client())
            .service(SERVICE),
    );
    let agent = OperationsClient(
        hub.context("agent", json!({}), test_http_client())
            .service(SERVICE),
    );
    (hub, web, agent)
}
async fn prepare(client: &OperationsClient) -> String {
    client
        .prepare(&PrepareRequest {
            tool: "watch_assets".into(),
            arguments: json!({"assets":"BTC"}),
            session_id: None,
        })
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}
#[tokio::test]
async fn models_cannot_confirm_cancel_or_spoof_origin() {
    let (hub, web, agent) = fixture().await;
    let id = prepare(&agent).await;
    assert_eq!(web.status(&id).await.unwrap()["origin"], "internal");
    for method in ["confirm", "cancel", "plans"] {
        let e = agent.0.call(method, json!({"id":id})).await.unwrap_err();
        assert_eq!(e.code, ServiceError::FORBIDDEN);
    }
    assert!(hub
        .call(
            SERVICE,
            "prepare",
            json!({"tool":"watch_assets","arguments":{"assets":"BTC"}})
        )
        .await
        .is_err());
    assert_eq!(web.status(&id).await.unwrap()["status"], "pending");
}
#[tokio::test]
async fn preview_then_parallel_confirmation_has_one_side_effect_and_no_replay() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (hub, web, agent) = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    hub.register_fn("web", "workspace-state", move |_, _| {
        let c = c.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(json!({}))
        }
    });
    let id = prepare(&agent).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (a, b) = tokio::join!(
        web.confirm(&id, Confirmation::default()),
        web.confirm(&id, Confirmation::default())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(web.status(&id).await.unwrap()["status"], "completed");
    assert!(web.confirm(&id, Confirmation::default()).await.is_err());
}
#[tokio::test]
async fn cancellation_expiry_and_restart_preserve_at_most_once() {
    let (hub, web, _) = fixture().await;
    let cancelled = prepare(&web).await;
    web.cancel(&cancelled).await.unwrap();
    assert!(web
        .confirm(&cancelled, Confirmation::default())
        .await
        .is_err());
    let ctx = hub.context("operations", json!({}), test_http_client());
    for state in ["pending", "running", "completed"] {
        let id = prepare(&web).await;
        let rows: Vec<Row> = ctx
            .store()
            .query_as(
                "SELECT value FROM operation_plans WHERE id=?",
                vec![json!(id)],
            )
            .await
            .unwrap();
        let mut p: Plan = serde_json::from_str(&rows[0].value).unwrap();
        p.created_at = 0;
        p.status = state.into();
        ctx.store()
            .execute(
                "UPDATE operation_plans SET value=? WHERE id=?",
                vec![json!(serde_json::to_string(&p).unwrap()), json!(id)],
            )
            .await
            .unwrap();
        OperationsPlugin.start(ctx.clone()).await.unwrap();
        if state == "running" {
            assert_eq!(web.status(&id).await.unwrap()["status"], "interrupted");
        }
        assert!(web.confirm(&id, Confirmation::default()).await.is_err());
    }
}
#[tokio::test]
async fn legacy_import_keeps_status_and_does_not_overwrite_new_state() {
    let (hub, web, _) = fixture().await;
    let legacy = Plan {
        id: "legacy-one".into(),
        tool: "watch_assets".into(),
        arguments: json!({"assets":"ETH"}),
        origin: "external".into(),
        session_id: None,
        created_at: now(),
        status: "pending".into(),
        result: None,
    };
    hub.register_fn("web", "workspace-state", move |_, _| {
        let legacy = legacy.clone();
        async move { Ok(json!({"plans":[legacy]})) }
    });
    assert_eq!(web.status("legacy-one").await.unwrap()["status"], "pending");
    web.cancel("legacy-one").await.unwrap();
    OperationsPlugin
        .start(hub.context("operations", json!({}), test_http_client()))
        .await
        .unwrap();
    web.plans().await.unwrap();
    assert_eq!(
        web.status("legacy-one").await.unwrap()["status"],
        "cancelled"
    );
}
#[tokio::test]
async fn validation_and_service_failure_cannot_report_success_or_persist_secrets() {
    let (_, web, _) = fixture().await;
    for args in [
        json!({"assets":"BTC","unknown":true}),
        json!({"assets":"sk-PRIVATE"}),
    ] {
        assert!(web
            .prepare(&PrepareRequest {
                tool: "watch_assets".into(),
                arguments: args,
                session_id: None
            })
            .await
            .is_err());
    }
    let plan = web
        .prepare(&PrepareRequest {
            tool: "secret_save".into(),
            arguments: json!({"key":"llm.api_key"}),
            session_id: None,
        })
        .await
        .unwrap();
    let id = plan["id"].as_str().unwrap();
    assert!(web.confirm(id, Confirmation::default()).await.is_err());
    let input = Confirmation {
        secrets: std::collections::HashMap::from([("value".into(), "PRIVATE".into())]),
    };
    let result = web.confirm(id, input).await.unwrap();
    assert_eq!(result["status"], "failed");
    assert!(!web.plans().await.unwrap().to_string().contains("PRIVATE"));
}

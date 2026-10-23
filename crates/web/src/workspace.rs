//! Hub adapter for data owned by the web workspace, not a general command executor.
//! Only the confirmed-operation plugin can write it; legacy plans are exported for migration.
use std::sync::Arc;

use plugin_sdk::contracts::auth::AuthClient;
use plugin_sdk::contracts::{Caller, ServiceError, OWNER};
use plugin_sdk::PluginError;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{feed::db, AppState};

pub(crate) fn register(st: &Arc<AppState>) -> Result<(), PluginError> {
    let weak = Arc::downgrade(st);
    st.ctx
        .provide_fn("workspace-state", move |caller, method, params| {
            let weak = weak.clone();
            async move {
                let st = weak
                    .upgrade()
                    .ok_or_else(|| ServiceError::unavailable("界面工作区已停止"))?;
                call(&st, caller, &method, params).await
            }
        })
}

async fn call(
    st: &AppState,
    caller: Caller,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    if caller.plugin != "operations" || caller.user_id != OWNER {
        return Err(ServiceError::forbidden("只有应用操作插件能访问工作区状态"));
    }
    let action = if matches!(method, "inspect" | "legacy_plans") {
        "web.view"
    } else {
        "web.manage"
    };
    if !AuthClient(st.ctx.service("auth")).check(action).await? {
        return Err(ServiceError::forbidden("没有权限"));
    }
    match method {
        "inspect" => {
            let raw = db(st)
                .pref(crate::data::PREF_WATCH)
                .await?
                .unwrap_or_default();
            let assets: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            Ok(json!({"watch_assets": assets}))
        }
        "legacy_plans" => {
            #[derive(Deserialize)]
            struct Row {
                value: String,
            }
            let rows = st.ctx.store().query_as::<Row>(
                "SELECT value FROM prefs WHERE user_id=? AND key LIKE 'mcp_plan_%' ORDER BY key",
                vec![json!(OWNER)],
            ).await?;
            // Do not silently lose completed or cancelled records: these block replay after migration.
            let plans: Vec<Value> = rows
                .into_iter()
                .map(|r| {
                    serde_json::from_str(&r.value)
                        .map_err(|_| ServiceError::internal("旧版操作计划读取失败"))
                })
                .collect::<Result<_, _>>()?;
            Ok(json!({"plans":plans}))
        }
        "watch_assets" => Ok(
            json!({"watch_assets":crate::stats::save_watch_assets(st, required_string(&params, "assets")?).await?}),
        ),
        "item_read" => {
            let id = required_id(&params)?;
            db(st).mark_read(&[id]).await?;
            Ok(json!({"item_id":id,"read":true}))
        }
        "item_star" => {
            let id = required_id(&params)?;
            Ok(json!({"item_id":id,"starred":db(st).toggle_star(id).await?}))
        }
        "items_read_all" => crate::feed::mark_default_read(st).await,
        "session_rename" => crate::chat::rename_session(
            st,
            required_id(&params)?,
            required_string(&params, "title")?,
        )
        .await?
        .ok_or_else(|| ServiceError::invalid_params("会话不存在")),
        "session_delete" => crate::chat::delete_session(st, required_id(&params)?)
            .await?
            .ok_or_else(|| ServiceError::invalid_params("会话不存在")),
        other => Err(ServiceError::method_not_found(other)),
    }
}

fn required_string<'a>(params: &'a Value, key: &str) -> Result<&'a str, ServiceError> {
    params[key]
        .as_str()
        .ok_or_else(|| ServiceError::invalid_params(format!("缺少 {key}")))
}

fn required_id(params: &Value) -> Result<i64, ServiceError> {
    params["id"]
        .as_i64()
        .or_else(|| params["id"].as_str()?.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| ServiceError::invalid_params("无效的目标编号"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};

    async fn fixture() -> Arc<AppState> {
        let hub = TestHub::new();
        hub.with_store(store::open_memory().await.unwrap());
        hub.register_fn("auth", "auth", |_, _| async { Ok(json!({"allowed":true})) });
        let ctx = hub.context("web", json!({}), test_http_client());
        ctx.store().migrate(1, crate::data::SCHEMA).await.unwrap();
        ctx.store()
            .migrate(2, crate::data::CONVERSATION_SCHEMA)
            .await
            .unwrap();
        Arc::new(AppState {
            ctx,
            opener: Arc::new(crate::opener::SystemOpener),
            chat: crate::chat::ChatState::default(),
            status: crate::status::Coordinator::default(),
        })
    }

    #[tokio::test]
    async fn caller_identity_is_not_supplied_by_arguments() {
        let st = fixture().await;
        let result = call(
            &st,
            Caller::owner("agent"),
            "watch_assets",
            json!({"assets":"BTC","plugin":"operations"}),
        )
        .await;
        assert_eq!(result.unwrap_err().code, ServiceError::FORBIDDEN);
        assert!(db(&st).watch_assets().await.is_empty());
        let mut caller = Caller::owner("operations");
        caller.user_id = "other".into();
        assert_eq!(
            call(&st, caller, "inspect", json!({}))
                .await
                .unwrap_err()
                .code,
            ServiceError::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn session_operations_validate_and_share_workspace_data() {
        let st = fixture().await;
        let id = db(&st).create_session("原名", None).await.unwrap();
        assert!(call(
            &st,
            Caller::owner("operations"),
            "session_rename",
            json!({"id":id,"title":"  "})
        )
        .await
        .is_err());
        let renamed = call(
            &st,
            Caller::owner("operations"),
            "session_rename",
            json!({"id":id,"title":" 新名 "}),
        )
        .await
        .unwrap();
        assert_eq!(renamed["title"], "新名");
        assert_eq!(db(&st).session(id).await.unwrap().unwrap().title, "新名");
        let deleted = call(
            &st,
            Caller::owner("operations"),
            "session_delete",
            json!({"id":id}),
        )
        .await
        .unwrap();
        assert_eq!(deleted["deleted"], true);
        assert!(deleted["undo"].is_string());
        assert!(db(&st).session(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn legacy_export_keeps_all_terminal_records() {
        let st = fixture().await;
        for i in 0..105 {
            db(&st)
                .set_pref(
                    &format!("mcp_plan_{i}"),
                    &json!({"id":i.to_string(),"status":"completed"}).to_string(),
                )
                .await
                .unwrap();
        }
        let exported = call(&st, Caller::owner("operations"), "legacy_plans", json!({}))
            .await
            .unwrap();
        assert_eq!(exported["plans"].as_array().unwrap().len(), 105);
        assert!(exported["plans"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["status"] == "completed"));
    }
}

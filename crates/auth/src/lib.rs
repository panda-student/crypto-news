//! 认证与权限的统一入口。首版只在本机运行，调用方恒为主人，所有动作都允许。
//! 以后加多用户和远程访问时只改这里，调用方不变。

use contracts::auth::{CheckResult, CurrentUser, SERVICE};
use contracts::{to_result, ServiceError};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError};
use serde_json::Value;

pub struct AuthPlugin;

impl Plugin for AuthPlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            ctx.provide_fn(SERVICE, |caller, method, params| async move {
                handle(&caller.user_id, &method, params)
            })
        })
    }
}

fn handle(user_id: &str, method: &str, params: Value) -> Result<Value, ServiceError> {
    match method {
        "current" => to_result(&CurrentUser {
            user_id: user_id.to_string(),
            role: "owner".to_string(),
        }),
        "check" => {
            if !params.get("action").is_some_and(Value::is_string) {
                return Err(ServiceError::invalid_params("缺少 action"));
            }
            to_result(&CheckResult { allowed: true })
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contracts::auth::AuthClient;
    use plugin_sdk::testing::{test_http_client, TestHub};

    #[tokio::test]
    async fn owner_is_always_allowed() {
        let hub = TestHub::new();
        let ctx = hub.context("auth", Value::Null, test_http_client());
        AuthPlugin.start(ctx.clone()).await.unwrap();
        let client = AuthClient(ctx.service(SERVICE));
        let me = client.current().await.unwrap();
        assert_eq!(me.role, "owner");
        assert_eq!(me.user_id, contracts::OWNER);
        assert!(client.check("plugins.grant").await.unwrap());
        assert!(hub.call(SERVICE, "nope", Value::Null).await.is_err());
    }
}

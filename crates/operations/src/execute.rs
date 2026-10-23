//! Execute confirmed application operations through Hub-owned service handles.
//! No HTTP request or UI router participates in the command path.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use plugin_sdk::contracts::{
    channel::ChannelClient,
    dispatch::DispatchClient,
    focus::PushSettings,
    hub_admin::{app_keys, HubAdminClient},
    llm::{provider_kinds, Assignment, LlmClient, ProviderInput, TestRequest},
    rulebook::RulebookClient,
    screen::{Level, ScreeningClient},
    secrets::SecretsClient,
    source::{FeedAdminClient, FeedInput, SourceStatusClient},
    to_result,
    x::XAdminClient,
    ServiceError,
};
use plugin_sdk::Context;
use serde_json::{json, Value};

fn text(args: &Value, key: &str) -> Result<String, ServiceError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| ServiceError::invalid_params(format!("缺少字符串参数 {key}")))
}

fn target(args: &Value, key: &str) -> Result<String, ServiceError> {
    let value = text(args, key)?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ServiceError::invalid_params(format!("无效目标 {key}")));
    }
    Ok(value)
}

fn boolean(args: &Value, key: &str) -> Result<bool, ServiceError> {
    args.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| ServiceError::invalid_params(format!("{key} 必须是布尔值")))
}

fn numeric_id(args: &Value) -> Result<i64, ServiceError> {
    args["id"]
        .as_i64()
        .or_else(|| args["id"].as_str()?.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| ServiceError::invalid_params("无效消息或队列编号"))
}

fn http_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > 8
        && !url.contains(' ')
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split([',', '，', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn models(args: &Value) -> Result<Vec<String>, ServiceError> {
    match args.get("models") {
        None => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(split_list(s)),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ServiceError::invalid_params("models 只能包含模型名称"))
            })
            .collect(),
        _ => Err(ServiceError::invalid_params(
            "models 必须是名称列表或逗号分隔字符串",
        )),
    }
}

fn secret<'a>(secrets: &'a HashMap<String, String>, key: &str) -> Result<&'a str, ServiceError> {
    secrets
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ServiceError::invalid_params("请填写安全输入框"))
}

fn protect_self(ctx: &Context, plugin: &str) -> Result<(), ServiceError> {
    if plugin == ctx.name() {
        return Err(ServiceError::invalid_params(
            "操作插件不能在执行计划时停止或重启自身，请在插件设置中操作。",
        ));
    }
    Ok(())
}

pub(crate) async fn execute(
    ctx: &Context,
    name: &str,
    args: Value,
    secrets: HashMap<String, String>,
) -> Result<Value, ServiceError> {
    let admin = HubAdminClient(ctx.service("hub-admin"));
    let llm = LlmClient(ctx.service("llm"));
    let feeds = FeedAdminClient(ctx.service("feed-admin"));
    let x = XAdminClient(ctx.service("x-admin"));
    match name {
        "source_save" => {
            let name = target(&args, "name")?;
            let url = text(&args, "url")?;
            if !http_url(&url) {
                return Err(ServiceError::invalid_params(
                    "信源地址要以 http:// 或 https:// 开头",
                ));
            }
            let id = args["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            return to_result(&feeds.save(&FeedInput { id, name, url }).await?);
        }
        "source_pause" => {
            return to_result(
                &feeds
                    .set_paused(&target(&args, "id")?, boolean(&args, "paused")?)
                    .await?,
            )
        }
        "source_delete" => feeds.delete(&target(&args, "id")?).await?,
        "source_retry" => {
            SourceStatusClient(ctx.service("source-status"))
                .retry(&target(&args, "id")?)
                .await?
        }
        "x_add" => return to_result(&x.add(&target(&args, "handle")?).await?),
        "x_pause" => {
            return to_result(
                &x.set_paused(&target(&args, "handle")?, boolean(&args, "paused")?)
                    .await?,
            )
        }
        "x_delete" => x.delete(&target(&args, "handle")?).await?,
        "x_login" => return to_result(&x.login().await?),
        "x_open" => return ctx.service("x-admin").call("open", json!({})).await,
        "model_save" => {
            let name = target(&args, "name")?;
            let base_url = text(&args, "base_url")?;
            if !http_url(&base_url) {
                return Err(ServiceError::invalid_params(
                    "模型地址要以 http:// 或 https:// 开头",
                ));
            }
            let input = ProviderInput {
                id: args["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                name,
                kind: if args["kind"] == provider_kinds::ANTHROPIC {
                    provider_kinds::ANTHROPIC
                } else {
                    provider_kinds::OPENAI
                }
                .into(),
                base_url,
                models: models(&args)?,
                api_key: secrets
                    .get("api_key")
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty()),
            };
            return to_result(&llm.provider_save(&input).await?);
        }
        "model_assign" => {
            let provider_id = target(&args, "provider_id")?;
            let providers = llm.providers().await?;
            let provider = providers
                .providers
                .iter()
                .find(|p| p.id == provider_id)
                .ok_or_else(|| ServiceError::invalid_params("模型服务不存在"))?;
            let requested = args["model"].as_str().unwrap_or_default();
            let model = provider
                .models
                .iter()
                .find(|m| m.as_str() == requested)
                .or(provider.models.first())
                .cloned()
                .unwrap_or_default();
            llm.assign(&Assignment {
                purpose: target(&args, "purpose")?,
                provider_id,
                model,
            })
            .await?;
        }
        "model_default" => llm.set_default(&target(&args, "id")?).await?,
        "model_delete" => llm.provider_delete(&target(&args, "id")?).await?,
        "model_test" => {
            let result = llm
                .test(&TestRequest {
                    provider_id: Some(target(&args, "id")?),
                })
                .await?;
            if !result.ok {
                return Err(ServiceError::unavailable(
                    "模型连接测试失败，请检查模型设置。",
                ));
            }
            return to_result(&result);
        }
        "plugin_enabled" => {
            let plugin = target(&args, "name")?;
            let enabled = boolean(&args, "enabled")?;
            if !enabled {
                protect_self(ctx, &plugin)?;
            }
            admin.set_enabled(&plugin, enabled).await?;
        }
        "plugin_restart" => {
            let plugin = target(&args, "name")?;
            protect_self(ctx, &plugin)?;
            admin.restart(&plugin).await?;
        }
        "plugin_grant" => admin.grant(&target(&args, "name")?).await?,
        "plugin_config" => {
            let plugin = target(&args, "name")?;
            protect_self(ctx, &plugin)?;
            let fields = args["fields"]
                .as_object()
                .ok_or_else(|| ServiceError::invalid_params("fields 必须是配置对象"))?;
            // Hub validates declared keys/types and applies only the supplied overrides.
            admin
                .config_set(&plugin, Value::Object(fields.clone()))
                .await?;
        }
        "watch_assets" | "item_read" | "item_star" | "items_read_all" | "session_rename"
        | "session_delete" => {
            return ctx.service("workspace-state").call(name, args).await;
        }
        "item_level" => {
            let level = Level::parse(&text(&args, "level")?)
                .ok_or_else(|| ServiceError::invalid_params("无效的级别"))?;
            return to_result(
                &ScreeningClient(ctx.service("screening"))
                    .review(numeric_id(&args)?, level)
                    .await?,
            );
        }
        "push_settings" => {
            let settings: PushSettings = serde_json::from_value(args)?;
            settings.validate()?;
            return ctx
                .service("dispatch")
                .call("save_focus_settings", to_result(&settings)?)
                .await;
        }
        "channel_test" => {
            return to_result(
                &DispatchClient(ctx.service("dispatch"))
                    .test_send(&target(&args, "id")?)
                    .await?,
            )
        }
        "channel_bind" => {
            let id = target(&args, "id")?;
            let channel = ctx
                .services("channel")
                .into_iter()
                .find(|(p, _)| p == &id)
                .ok_or_else(|| ServiceError::invalid_params("渠道不存在"))?;
            return to_result(&ChannelClient(channel.1).bind().await?);
        }
        "webhook_save" => {
            SecretsClient(ctx.service("secrets"))
                .set(
                    "wecom-webhook.webhook_key",
                    secret(&secrets, "value")?.trim(),
                )
                .await?
        }
        "secret_save" => {
            SecretsClient(ctx.service("secrets"))
                .set(&target(&args, "key")?, secret(&secrets, "value")?)
                .await?
        }
        "secret_delete" => {
            SecretsClient(ctx.service("secrets"))
                .delete(&target(&args, "key")?)
                .await?
        }
        "outbox_retry" => {
            return to_result(
                &DispatchClient(ctx.service("dispatch"))
                    .retry(numeric_id(&args)?)
                    .await?,
            )
        }
        "rules_save" => {
            let text = args["text"]
                .as_str()
                .ok_or_else(|| ServiceError::invalid_params("text 必须是规则文本"))?;
            return to_result(&RulebookClient(ctx.service("rulebook")).set(text).await?);
        }
        "proxy_save" => {
            let mode = text(&args, "mode")?;
            let url = if mode == "direct" {
                String::new()
            } else {
                text(&args, "url")?
            };
            if mode != "direct"
                && (!["http://", "https://", "socks5://", "socks5h://"]
                    .iter()
                    .any(|p| url.starts_with(p) && url.len() > p.len())
                    || url.contains(' '))
            {
                return Err(ServiceError::invalid_params(
                    "代理地址要以 http://、https://、socks5:// 或 socks5h:// 开头",
                ));
            }
            admin.app_set(app_keys::PROXY, json!(url)).await?;
        }
        "proxy_test" => {
            let config = admin.config_get("web").await?;
            let url = config.config["proxy_test_url"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("https://www.gstatic.com/generate_204");
            let start = Instant::now();
            let response = ctx
                .http()
                .get(url)
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .map_err(|_| ServiceError::unavailable("无法连接，请检查当前生效的代理设置"))?;
            if !response.status().is_success() && !response.status().is_redirection() {
                return Err(ServiceError::unavailable(format!(
                    "网络测试目标返回 {}",
                    response.status().as_u16()
                )));
            }
            return Ok(json!({"ok":true,"latency_ms":start.elapsed().as_millis()}));
        }
        "app_settings" => {
            if admin.app_get(app_keys::SHELL).await?.as_str() != Some("desktop") {
                return Err(ServiceError::invalid_params(
                    "开机自启和关闭到托盘仅桌面版可用",
                ));
            }
            // Validate the complete patch before writing either setting.
            let mut changes = Vec::new();
            for key in [app_keys::AUTOSTART, app_keys::CLOSE_TO_TRAY] {
                if args.get(key).is_some() {
                    changes.push((key, boolean(&args, key)?));
                }
            }
            if changes.is_empty() {
                return Err(ServiceError::invalid_params("请提供需要修改的系统设置"));
            }
            for (key, value) in changes {
                admin.app_set(key, json!(value)).await?;
            }
        }
        "update_action" => {
            let action = text(&args, "action")?;
            if !matches!(action.as_str(), "check" | "install") {
                return Err(ServiceError::invalid_params("未知更新操作"));
            }
            admin
                .app_set(
                    app_keys::UPDATE_ACTION,
                    json!({"kind":action,"request_id":plugin_sdk::contracts::new_trace_id()}),
                )
                .await?;
        }
        "auto_update" => {
            admin
                .app_set(app_keys::AUTO_UPDATE, json!(boolean(&args, "enabled")?))
                .await?
        }
        _ => return Err(ServiceError::method_not_found(name)),
    }
    Ok(json!({"ok":true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn invalid_settings_and_self_restart_do_not_reach_mutation_services() {
        let hub = TestHub::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        hub.register_fn("hub", "hub-admin", move |method, params| {
            captured.lock().unwrap().push((method, params));
            async { Ok(json!({})) }
        });
        let ctx = hub.context("operations", json!({}), test_http_client());
        for (tool, args) in [
            (
                "plugin_enabled",
                json!({"name":"dispatch","enabled":"false"}),
            ),
            ("plugin_restart", json!({"name":"operations"})),
            ("source_save", json!({"name":"bad","url":"invalid"})),
            ("proxy_save", json!({"mode":"proxy","url":"file:///local"})),
            ("push_settings", json!({"scope":"followed","assets":[]})),
        ] {
            assert!(
                execute(&ctx, tool, args, HashMap::new()).await.is_err(),
                "{tool}"
            );
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn system_patch_changes_only_requested_boolean() {
        let hub = TestHub::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        hub.register_fn("hub", "hub-admin", move |method, params| {
            captured.lock().unwrap().push((method.clone(), params));
            async move {
                Ok(if method == "app_get" {
                    json!({"value":"desktop"})
                } else {
                    json!({"ok":true})
                })
            }
        });
        let ctx = hub.context("operations", json!({}), test_http_client());
        execute(
            &ctx,
            "app_settings",
            json!({"close_to_tray":false}),
            HashMap::new(),
        )
        .await
        .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1],
            (
                "app_set".into(),
                json!({"key":"close_to_tray","value":false})
            )
        );
    }

    #[tokio::test]
    async fn model_test_failure_stays_failure() {
        let hub = TestHub::new();
        hub.register_fn("llm", "llm", |_, _| async {
            Ok(json!({"ok":false,"latency_ms":20,"message":"upstream unavailable","model":null}))
        });
        let ctx = hub.context("operations", json!({}), test_http_client());
        assert!(
            execute(&ctx, "model_test", json!({"id":"test"}), HashMap::new())
                .await
                .is_err()
        );
    }
}

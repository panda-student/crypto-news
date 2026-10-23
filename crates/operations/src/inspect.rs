use crate::catalogue::contains_secret;
use plugin_sdk::contracts::hub_admin::HubAdminClient;
use plugin_sdk::contracts::llm::LlmClient;
use plugin_sdk::contracts::source::FeedAdminClient;
use plugin_sdk::contracts::x::XAdminClient;
use plugin_sdk::ServiceError;
use serde_json::{json, Value};
pub(crate) async fn inspect(ctx: &plugin_sdk::Context) -> Result<Value, ServiceError> {
    let admin = HubAdminClient(ctx.service("hub-admin"));
    let llm = LlmClient(ctx.service("llm"));
    let feed = FeedAdminClient(ctx.service("feed-admin"));
    let x_admin = XAdminClient(ctx.service("x-admin"));
    let dispatch = ctx.service("dispatch");
    let empty = json!({});
    let (models, feeds, x, plugins, push) = tokio::join!(
        llm.providers(),
        feed.list(),
        x_admin.list(),
        admin.plugins(),
        plugin_sdk::contracts::call_typed::<_, Value>(&*dispatch, "focus_settings", &empty)
    );
    let mut data = json!({"watch_assets":ctx.service("workspace-state").call("inspect",json!({})).await?["watch_assets"],"models":models.ok(),"feeds":feeds.ok(),"x_accounts":x.ok(),"plugins":plugins.ok().map(|v|v.into_iter().map(|p|json!({"name":p.name,"enabled":p.enabled,"status":p.status})).collect::<Vec<_>>()),"push":push.ok()});
    for key in ["autostart", "close_to_tray", "auto_update", "proxy"] {
        data["app_settings"][key] = admin.app_get(key).await.unwrap_or(Value::Null);
    }
    // Never return URL credentials or query-string keys to models/clients.
    scrub(&mut data);
    Ok(data)
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

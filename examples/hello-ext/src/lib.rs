//! 扩展插件示例：提供 `hello` 服务，订阅 `item.collected` 并在内存里计数。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde_json::{json, Value};

#[derive(Default)]
pub struct HelloExt {
    items_seen: Arc<AtomicU64>,
}

impl Plugin for HelloExt {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        // 每次 start 都重置计数，重启后从 0 开始
        let seen = self.items_seen.clone();
        seen.store(0, Ordering::Relaxed);
        Box::pin(async move {
            let counter = seen.clone();
            ctx.subscribe("item.collected", move |_e| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
            })?;
            ctx.provide_fn("hello", move |_caller, method, params| {
                let seen = seen.clone();
                async move { handle(&seen, &method, params) }
            })?;
            tracing::info!("hello-ext 已启动");
            Ok(())
        })
    }
}

fn handle(seen: &AtomicU64, method: &str, params: Value) -> Result<Value, ServiceError> {
    match method {
        "greet" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ServiceError::invalid_params("缺少 name"))?;
            Ok(json!({ "text": format!("你好，{name}") }))
        }
        "stats" => Ok(json!({ "items_seen": seen.load(Ordering::Relaxed) })),
        other => Err(ServiceError::method_not_found(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::{now_rfc3339, Caller, Event};
    use plugin_sdk::extension::serve;
    use plugin_sdk::rpc::{LineTransport, RpcPeer};
    use plugin_sdk::testing::{test_http_client, TestHub};
    use plugin_sdk::Manifest;

    #[test]
    fn manifest_valid() {
        let m = Manifest::parse(HelloExt::default().manifest()).unwrap();
        assert_eq!(m.name, "hello-ext");
        assert_eq!(m.provides.get("hello").map(String::as_str), Some("1.0"));
        assert_eq!(m.entry.as_deref(), Some("hello-ext.exe"));
    }

    #[tokio::test]
    async fn greet_and_stats_with_test_hub() {
        let hub = TestHub::new();
        let ctx = hub.context("hello-ext", json!({}), test_http_client());
        HelloExt::default().start(ctx).await.unwrap();

        let r = hub
            .call("hello", "greet", json!({ "name": "世界" }))
            .await
            .unwrap();
        assert_eq!(r, json!({ "text": "你好，世界" }));
        assert!(hub.call("hello", "greet", json!({})).await.is_err());

        let r = hub.call("hello", "stats", json!({})).await.unwrap();
        assert_eq!(r, json!({ "items_seen": 0 }));
        hub.deliver("item.collected", json!({})).await.unwrap();
        let r = hub.call("hello", "stats", json!({})).await.unwrap();
        assert_eq!(r, json!({ "items_seen": 1 }));
        hub.stop().await;
    }

    #[tokio::test]
    async fn full_rpc_flow_over_duplex() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let task = tokio::spawn(serve(HelloExt::default(), br, bw));
        let (hub, _inc) = RpcPeer::start(Arc::new(LineTransport::new(ar, aw)));

        let r = hub
            .request(
                "initialize",
                json!({ "protocol": 1, "name": "hello-ext", "config": {}, "proxy": "http://127.0.0.1:9" }),
            )
            .await
            .unwrap();
        assert_eq!(r, json!({ "ok": true }));

        let r = hub
            .request(
                "service.call",
                json!({ "service": "hello", "method": "greet", "params": { "name": "hub" }, "caller": Caller::owner("hub") }),
            )
            .await
            .unwrap();
        assert_eq!(r, json!({ "text": "你好，hub" }));

        let ev = Event {
            id: 1,
            kind: "item.collected".into(),
            trace_id: "t".into(),
            source: "x".into(),
            created_at: now_rfc3339(),
            payload: json!({}),
        };
        let r = hub
            .request("event.deliver", json!({ "event": ev }))
            .await
            .unwrap();
        assert_eq!(r, json!({ "ok": true }));

        let r = hub
            .request(
                "service.call",
                json!({ "service": "hello", "method": "stats", "params": {}, "caller": Caller::owner("hub") }),
            )
            .await
            .unwrap();
        assert_eq!(r, json!({ "items_seen": 1 }));

        hub.request("shutdown", json!({})).await.unwrap();
        task.await.unwrap().unwrap();
    }
}

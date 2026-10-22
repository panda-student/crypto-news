//! 扩展插件运行时：在单独进程里跑插件，经 stdio 上的 JSON-RPC 与 hub 通信。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use contracts::{BoxFuture, Caller, Event, Intercept, ServiceError, ServiceHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::context::{EventFn, HubLink, InterceptFn, StopFn};
use crate::rpc::{Incoming, LineTransport, RpcPeer};
use crate::{build_http_client, Context, Plugin, PluginError};

pub const PROTOCOL: u32 = 1;

#[derive(Deserialize)]
struct InitParams {
    protocol: u32,
    name: String,
    #[serde(default)]
    config: Value,
    proxy: String,
}

#[derive(Default)]
struct Registry {
    subscribers: HashMap<String, EventFn>,
    interceptors: HashMap<String, InterceptFn>,
    services: HashMap<String, Arc<dyn ServiceHandler>>,
    stops: Vec<StopFn>,
}

struct RpcLink {
    peer: Arc<RpcPeer>,
    reg: Arc<Mutex<Registry>>,
    name: String,
}

impl HubLink for RpcLink {
    fn call(
        &self,
        service: &str,
        provider: Option<&str>,
        method: &str,
        params: Value,
    ) -> BoxFuture<'static, Result<Value, ServiceError>> {
        let peer = self.peer.clone();
        let p =
            json!({ "service": service, "provider": provider, "method": method, "params": params });
        Box::pin(async move { peer.request("service.call", p).await })
    }

    /// 扩展插件拿不到实时的提供者列表，`all = true` 的依赖首版不支持。
    fn providers(&self, _service: &str) -> Vec<String> {
        Vec::new()
    }

    fn publish(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<i64, PluginError>> {
        let peer = self.peer.clone();
        let p = json!({ "kind": kind, "payload": payload, "trace_id": trace_id });
        Box::pin(async move {
            let r = peer.request("event.publish", p).await?;
            Ok(r.get("id").and_then(Value::as_i64).unwrap_or(0))
        })
    }

    fn subscribe(&self, kind: &str, f: EventFn) -> Result<(), PluginError> {
        self.reg
            .lock()
            .unwrap()
            .subscribers
            .insert(kind.to_string(), f);
        Ok(())
    }

    fn intercept(&self, kind: &str, f: InterceptFn) -> Result<(), PluginError> {
        self.reg
            .lock()
            .unwrap()
            .interceptors
            .insert(kind.to_string(), f);
        Ok(())
    }

    fn emit_intercept(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<Option<Value>, PluginError>> {
        let peer = self.peer.clone();
        let p = json!({ "kind": kind, "payload": payload, "trace_id": trace_id });
        Box::pin(async move {
            let r = peer.request("event.emit_intercept", p).await?;
            Ok(r.get("payload").cloned().filter(|v| !v.is_null()))
        })
    }

    fn provide(&self, service: &str, h: Arc<dyn ServiceHandler>) -> Result<(), PluginError> {
        self.reg
            .lock()
            .unwrap()
            .services
            .insert(service.to_string(), h);
        Ok(())
    }

    fn spawn(&self, name: &str, fut: BoxFuture<'static, Result<(), PluginError>>) {
        let task = name.to_string();
        let plugin = self.name.clone();
        tokio::spawn(async move {
            if let Err(e) = fut.await {
                // 后台任务失败算插件失败：退出进程，由 hub 退避后重启
                eprintln!("插件 {plugin} 的任务 {task} 失败：{e}");
                std::process::exit(1);
            }
        });
    }

    fn on_stop(&self, f: StopFn) {
        self.reg.lock().unwrap().stops.push(f);
    }
}

/// 扩展插件的 `main` 调用它。
pub fn run_extension<P: Plugin>(plugin: P) -> Result<(), PluginError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PluginError::new(format!("创建运行时失败：{e}")))?;
    rt.block_on(serve(plugin, tokio::io::stdin(), tokio::io::stdout()))
}

/// 在给定的读写流上跑插件，直到收到 `shutdown` 或连接断开。
pub async fn serve<P, R, W>(plugin: P, reader: R, writer: W) -> Result<(), PluginError>
where
    P: Plugin,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (peer, mut incoming) = RpcPeer::start(Arc::new(LineTransport::new(reader, writer)));
    let reg = Arc::new(Mutex::new(Registry::default()));

    // 等 initialize
    let ctx = loop {
        let Some(msg) = incoming.recv().await else {
            return Ok(());
        };
        let Incoming::Request { id, method, params } = msg else {
            continue;
        };
        if method != "initialize" {
            peer.respond(id, Err(ServiceError::unavailable("尚未初始化")))
                .await?;
            continue;
        }
        let p: InitParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                peer.respond(id, Err(ServiceError::invalid_params(e)))
                    .await?;
                continue;
            }
        };
        if p.protocol != PROTOCOL {
            peer.respond(id, Err(ServiceError::invalid_params("协议版本不支持")))
                .await?;
            return Err(PluginError::new("协议版本不支持"));
        }
        let http = build_http_client(&p.proxy)?;
        let link = Arc::new(RpcLink {
            peer: peer.clone(),
            reg: reg.clone(),
            name: p.name.clone(),
        });
        let data_dir = std::env::current_dir().unwrap_or_default().join("data");
        let ctx = Context::new(link, p.name, p.config, http, data_dir);
        peer.respond(id, Ok(json!({ "ok": true }))).await?;
        break ctx;
    };

    plugin.start(ctx).await?;

    while let Some(msg) = incoming.recv().await {
        let Incoming::Request { id, method, params } = msg else {
            continue;
        };
        match method.as_str() {
            "shutdown" => {
                let stops: Vec<StopFn> = std::mem::take(&mut reg.lock().unwrap().stops);
                for f in stops.into_iter().rev() {
                    f().await;
                }
                peer.respond(id, Ok(json!({ "ok": true }))).await?;
                return Ok(());
            }
            "health.ping" => {
                peer.respond(id, Ok(json!({ "ok": true }))).await?;
            }
            _ => {
                let peer = peer.clone();
                let reg = reg.clone();
                tokio::spawn(async move {
                    let r = handle(&reg, &method, params).await;
                    let _ = peer.respond(id, r).await;
                });
            }
        }
    }
    Ok(())
}

async fn handle(reg: &Mutex<Registry>, method: &str, params: Value) -> Result<Value, ServiceError> {
    match method {
        "event.deliver" => {
            #[derive(Deserialize)]
            struct P {
                event: Event,
                #[serde(default)]
                intercept: bool,
            }
            let p: P = contracts::parse_params(params)?;
            if p.intercept {
                let f = reg.lock().unwrap().interceptors.get(&p.event.kind).cloned();
                let f = f.ok_or_else(|| ServiceError::method_not_found(&p.event.kind))?;
                let r: Intercept = f(p.event).await.map_err(ServiceError::internal)?;
                contracts::to_result(&r)
            } else {
                let f = reg.lock().unwrap().subscribers.get(&p.event.kind).cloned();
                let f = f.ok_or_else(|| ServiceError::method_not_found(&p.event.kind))?;
                f(p.event).await.map_err(ServiceError::internal)?;
                Ok(json!({ "ok": true }))
            }
        }
        "service.call" => {
            #[derive(Deserialize)]
            struct P {
                service: String,
                method: String,
                #[serde(default)]
                params: Value,
                caller: Caller,
            }
            let p: P = contracts::parse_params(params)?;
            let h = reg.lock().unwrap().services.get(&p.service).cloned();
            let h =
                h.ok_or_else(|| ServiceError::unavailable(format!("未提供服务 {}", p.service)))?;
            h.call(p.caller, &p.method, p.params).await
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Plugin for Echo {
        fn manifest(&self) -> &str {
            "name = 'echo'\nversion = '0.1.0'"
        }
        fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
            Box::pin(async move {
                ctx.provide_fn(
                    "echo",
                    |_c, m, p| async move { Ok(json!({ "m": m, "p": p })) },
                )?;
                ctx.subscribe("demo", |_e| async { Ok(()) })?;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn initialize_call_deliver_shutdown() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let task = tokio::spawn(serve(Echo, br, bw));
        let (hub, _inc) = RpcPeer::start(Arc::new(LineTransport::new(ar, aw)));

        let r = hub
            .request(
                "initialize",
                json!({ "protocol": 1, "name": "echo", "config": {}, "proxy": "http://127.0.0.1:9" }),
            )
            .await
            .unwrap();
        assert_eq!(r, json!({ "ok": true }));

        let r = hub
            .request(
                "service.call",
                json!({ "service": "echo", "method": "hi", "params": 1, "caller": Caller::owner("hub") }),
            )
            .await
            .unwrap();
        assert_eq!(r, json!({ "m": "hi", "p": 1 }));

        let ev = Event {
            id: 1,
            kind: "demo".into(),
            trace_id: "t".into(),
            source: "x".into(),
            created_at: contracts::now_rfc3339(),
            payload: json!({}),
        };
        let r = hub
            .request("event.deliver", json!({ "event": ev }))
            .await
            .unwrap();
        assert_eq!(r, json!({ "ok": true }));

        assert_eq!(
            hub.request("health.ping", json!({})).await.unwrap(),
            json!({ "ok": true })
        );
        hub.request("shutdown", json!({})).await.unwrap();
        task.await.unwrap().unwrap();
    }
}

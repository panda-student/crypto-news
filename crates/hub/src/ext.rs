//! 扩展插件进程：启动子进程，经 stdio 上的 JSON-RPC 代理它的服务、订阅和拦截。

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use contracts::{BoxFuture, Caller, Event, Intercept, ServiceError, ServiceHandler};
use plugin_sdk::rpc::{Incoming, LineTransport, RpcPeer};
use plugin_sdk::{HubLink, PluginError};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

use crate::link::Link;
use crate::redact;

const PROTOCOL: u32 = 1;

/// 运行中的扩展插件进程。
pub(crate) struct ExtHandle {
    peer: Arc<RpcPeer>,
    kill: oneshot::Sender<()>,
    done: oneshot::Receiver<()>,
}

impl ExtHandle {
    /// 先请插件自己退出，再结束进程。
    pub(crate) async fn stop(self) {
        let _ = self
            .peer
            .request_timeout("shutdown", json!({}), Duration::from_secs(5))
            .await;
        let _ = self.kill.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.done).await;
    }
}

struct ProxyService {
    peer: Arc<RpcPeer>,
    service: String,
}

impl ServiceHandler for ProxyService {
    fn call<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            self.peer
                .request(
                    "service.call",
                    json!({ "service": self.service, "method": method, "params": params, "caller": caller }),
                )
                .await
        })
    }
}

pub(crate) async fn start(
    link: Arc<Link>,
    dir: &Path,
    exe: &Path,
    config: Value,
) -> Result<ExtHandle, String> {
    let proxy = link.inner.proxy.clone();
    let mut cmd = tokio::process::Command::new(exe);
    cmd.current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env(k, &proxy);
    }
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW：不弹控制台窗口
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败：{e}"))?;
    let stdin = child.stdin.take().ok_or("拿不到 stdin")?;
    let stdout = child.stdout.take().ok_or("拿不到 stdout")?;
    let stderr = child.stderr.take().ok_or("拿不到 stderr")?;

    // 监视进程：收到 kill（或句柄被丢弃）时结束进程；进程自己退出时报告失败。
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    let watch_link = Link {
        inner: link.inner.clone(),
        name: link.name.clone(),
        generation: link.generation,
    };
    tokio::spawn(async move {
        tokio::select! {
            r = child.wait() => {
                let msg = match r {
                    Ok(s) => format!("进程退出：{s}"),
                    Err(e) => format!("进程异常：{e}"),
                };
                watch_link.fail(msg);
            }
            _ = kill_rx => {
                let _ = child.kill().await;
            }
        }
        let _ = done_tx.send(());
    });

    let name = link.name.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::info!("[{name}] {}", redact(&l));
        }
    });
    link.add_task(err_task.abort_handle());

    let (peer, incoming) = RpcPeer::start(Arc::new(LineTransport::new(stdout, stdin)));
    let handle = ExtHandle {
        peer: peer.clone(),
        kill: kill_tx,
        done: done_rx,
    };

    let init = json!({
        "protocol": PROTOCOL,
        "name": link.name,
        "config": config,
        "proxy": proxy,
    });
    peer.request_timeout("initialize", init, Duration::from_secs(10))
        .await
        .map_err(|e| format!("初始化失败：{}", e.message))?;

    // 处理插件发来的请求
    let in_task = tokio::spawn(serve_incoming(link.clone(), peer.clone(), incoming));
    link.add_task(in_task.abort_handle());

    register_proxies(&link, &peer).map_err(|e| e.0)?;

    // 心跳
    let ping_peer = peer.clone();
    link.spawn(
        "health",
        Box::pin(async move {
            let mut misses = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                match ping_peer
                    .request_timeout("health.ping", json!({}), Duration::from_secs(5))
                    .await
                {
                    Ok(_) => misses = 0,
                    Err(_) => {
                        misses += 1;
                        if misses >= 3 {
                            return Err(PluginError::new("连续 3 次心跳无响应"));
                        }
                    }
                }
            }
        }),
    );
    Ok(handle)
}

/// 按清单把服务、订阅和拦截登记成转发到进程的代理。
fn register_proxies(link: &Arc<Link>, peer: &Arc<RpcPeer>) -> Result<(), PluginError> {
    let manifest = {
        let st = link.inner.state.lock().unwrap();
        st.plugins
            .get(&link.name)
            .map(|e| e.manifest.clone())
            .ok_or_else(|| PluginError::new("插件不存在"))?
    };
    for svc in manifest.provides.keys() {
        link.provide(
            svc,
            Arc::new(ProxyService {
                peer: peer.clone(),
                service: svc.clone(),
            }),
        )?;
    }
    for kind in &manifest.events.subscribe {
        let p = peer.clone();
        link.subscribe(
            kind,
            Arc::new(move |ev: Event| {
                let p = p.clone();
                Box::pin(async move {
                    p.request("event.deliver", json!({ "event": ev, "intercept": false }))
                        .await?;
                    Ok(())
                })
            }),
        )?;
    }
    for kind in &manifest.events.intercept {
        let p = peer.clone();
        link.intercept(
            kind,
            Arc::new(move |ev: Event| {
                let p = p.clone();
                Box::pin(async move {
                    let r = p
                        .request("event.deliver", json!({ "event": ev, "intercept": true }))
                        .await?;
                    let i: Intercept = serde_json::from_value(r)?;
                    Ok(i)
                })
            }),
        )?;
    }
    Ok(())
}

async fn serve_incoming(
    link: Arc<Link>,
    peer: Arc<RpcPeer>,
    mut incoming: tokio::sync::mpsc::UnboundedReceiver<Incoming>,
) {
    while let Some(msg) = incoming.recv().await {
        match msg {
            Incoming::Request { id, method, params } => {
                let link = link.clone();
                let peer = peer.clone();
                tokio::spawn(async move {
                    let r = handle(&link, &method, params).await;
                    let _ = peer.respond(id, r).await;
                });
            }
            Incoming::Notification { method, params } => {
                if method == "log" {
                    let msg = params.get("message").and_then(Value::as_str).unwrap_or("");
                    tracing::info!("[{}] {}", link.name, redact(msg));
                }
            }
        }
    }
}

async fn handle(link: &Link, method: &str, params: Value) -> Result<Value, ServiceError> {
    #[derive(Deserialize)]
    struct Call {
        service: String,
        #[serde(default)]
        provider: Option<String>,
        method: String,
        #[serde(default)]
        params: Value,
    }
    #[derive(Deserialize)]
    struct Pub {
        kind: String,
        #[serde(default)]
        payload: Value,
        #[serde(default)]
        trace_id: Option<String>,
    }
    let to_err = |e: PluginError| ServiceError::forbidden(e.0);
    match method {
        "service.call" => {
            let p: Call = contracts::parse_params(params)?;
            link.call(&p.service, p.provider.as_deref(), &p.method, p.params)
                .await
        }
        "event.publish" => {
            let p: Pub = contracts::parse_params(params)?;
            let id = link
                .publish(&p.kind, p.payload, p.trace_id)
                .await
                .map_err(to_err)?;
            Ok(json!({ "id": id }))
        }
        "event.emit_intercept" => {
            let p: Pub = contracts::parse_params(params)?;
            let r = link
                .emit_intercept(&p.kind, p.payload, p.trace_id)
                .await
                .map_err(to_err)?;
            Ok(json!({ "payload": r }))
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

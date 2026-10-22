//! JSON-RPC 2.0：每行一个 JSON 对象。传输层是 trait，首版只有按行读写的实现（stdio）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contracts::{BoxFuture, ServiceError};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// 传输层。
pub trait Transport: Send + Sync {
    fn send(&self, msg: Value) -> BoxFuture<'_, Result<(), ServiceError>>;
    /// 读下一条消息；连接关闭时返回 None。
    fn recv(&self) -> BoxFuture<'_, Option<Value>>;
}

/// 按行读写 JSON 的传输层，用于 stdio 和子进程管道。
pub struct LineTransport<R, W> {
    reader: tokio::sync::Mutex<BufReader<R>>,
    writer: tokio::sync::Mutex<W>,
}

impl<R, W> LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        LineTransport {
            reader: tokio::sync::Mutex::new(BufReader::new(reader)),
            writer: tokio::sync::Mutex::new(writer),
        }
    }
}

impl<R, W> Transport for LineTransport<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    fn send(&self, msg: Value) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let mut line = serde_json::to_string(&msg).map_err(ServiceError::internal)?;
            line.push('\n');
            let mut w = self.writer.lock().await;
            w.write_all(line.as_bytes())
                .await
                .map_err(|_| ServiceError::unavailable("连接已断开"))?;
            w.flush()
                .await
                .map_err(|_| ServiceError::unavailable("连接已断开"))
        })
    }

    fn recv(&self) -> BoxFuture<'_, Option<Value>> {
        Box::pin(async move {
            let mut r = self.reader.lock().await;
            loop {
                let mut line = String::new();
                match r.read_line(&mut line).await {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {}
                }
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(t) {
                    Ok(v) => return Some(v),
                    Err(_) => tracing::warn!("忽略一行无法解析的 JSON-RPC 消息"),
                }
            }
        })
    }
}

/// 对端发来的请求或通知。
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<Value, ServiceError>>>>;

/// JSON-RPC 对端：发请求等响应，并把对方的请求交给接收端处理。
pub struct RpcPeer {
    transport: Arc<dyn Transport>,
    pending: Pending,
    next_id: AtomicU64,
}

impl RpcPeer {
    /// 启动读取任务。返回的接收端在连接关闭时结束。
    pub fn start(
        transport: Arc<dyn Transport>,
    ) -> (Arc<RpcPeer>, mpsc::UnboundedReceiver<Incoming>) {
        let peer = Arc::new(RpcPeer {
            transport,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let p = peer.clone();
        tokio::spawn(async move {
            while let Some(msg) = p.transport.recv().await {
                p.handle(msg, &tx);
            }
            // 连接关闭：未完成的请求全部失败
            let pending: Vec<_> = p.pending.lock().unwrap().drain().collect();
            for (_, s) in pending {
                let _ = s.send(Err(ServiceError::unavailable("连接已断开")));
            }
        });
        (peer, rx)
    }

    fn handle(&self, msg: Value, tx: &mpsc::UnboundedSender<Incoming>) {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, msg.get("id").cloned()) {
            (Some(method), Some(id)) if !id.is_null() => {
                let _ = tx.send(Incoming::Request { id, method, params });
            }
            (Some(method), _) => {
                let _ = tx.send(Incoming::Notification { method, params });
            }
            (None, Some(id)) => {
                let Some(id) = id.as_u64() else { return };
                let Some(s) = self.pending.lock().unwrap().remove(&id) else {
                    return;
                };
                let r = if let Some(err) = msg.get("error") {
                    Err(ServiceError::new(
                        err.get("code")
                            .and_then(Value::as_i64)
                            .unwrap_or(ServiceError::INTERNAL),
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("未知错误"),
                    ))
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = s.send(r);
            }
            (None, None) => {}
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, ServiceError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (s, r) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, s);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.transport.send(msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        r.await
            .unwrap_or_else(|_| Err(ServiceError::unavailable("连接已断开")))
    }

    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ServiceError> {
        match tokio::time::timeout(timeout, self.request(method, params)).await {
            Ok(r) => r,
            Err(_) => Err(ServiceError::unavailable(format!("{method} 超时"))),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ServiceError> {
        self.transport
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    pub async fn respond(
        &self,
        id: Value,
        result: Result<Value, ServiceError>,
    ) -> Result<(), ServiceError> {
        let msg = match result {
            Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Err(e) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": e.code, "message": e.message } })
            }
        };
        self.transport.send(msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_response_over_duplex() {
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (pa, _ia) = RpcPeer::start(Arc::new(LineTransport::new(ar, aw)));
        let (pb, mut ib) = RpcPeer::start(Arc::new(LineTransport::new(br, bw)));
        tokio::spawn(async move {
            while let Some(m) = ib.recv().await {
                if let Incoming::Request { id, method, params } = m {
                    let r = if method == "echo" {
                        Ok(params)
                    } else {
                        Err(ServiceError::method_not_found(&method))
                    };
                    pb.respond(id, r).await.unwrap();
                }
            }
        });
        let v = pa.request("echo", json!({"x": 1})).await.unwrap();
        assert_eq!(v, json!({"x": 1}));
        let e = pa.request("nope", json!({})).await.unwrap_err();
        assert_eq!(e.code, ServiceError::METHOD_NOT_FOUND);
    }
}

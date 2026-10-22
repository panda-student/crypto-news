//! 测试用的内存 hub：插件单元测试用它代替真正的 hub。

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use contracts::store::{dispatch_store_call, SharedStore, TableAccess};
use contracts::{BoxFuture, Caller, Event, Intercept, ServiceError, ServiceHandler};
use serde_json::Value;

use crate::context::{EventFn, HubLink, InterceptFn, StopFn};
use crate::{Context, PluginError};

type Providers = BTreeMap<String, Arc<dyn ServiceHandler>>;

#[derive(Default)]
struct State {
    services: HashMap<String, Providers>,
    subscribers: HashMap<String, Vec<EventFn>>,
    interceptors: HashMap<String, Vec<InterceptFn>>,
    published: Vec<Event>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    stops: Vec<StopFn>,
    failures: Vec<String>,
}

/// 内存 hub。服务调用不做权限检查，事件同步投递给订阅者。
#[derive(Default)]
pub struct TestHub {
    state: Mutex<State>,
    next_id: AtomicI64,
}

impl TestHub {
    pub fn new() -> Arc<TestHub> {
        Arc::new(TestHub::default())
    }

    /// 以插件 `provider` 的名义登记服务。
    pub fn register(&self, provider: &str, service: &str, h: Arc<dyn ServiceHandler>) {
        self.state
            .lock()
            .unwrap()
            .services
            .entry(service.to_string())
            .or_default()
            .insert(provider.to_string(), h);
    }

    /// 用闭包登记服务：`f(method, params)`。
    pub fn register_fn<F, Fut>(&self, provider: &str, service: &str, f: F)
    where
        F: Fn(String, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ServiceError>> + Send + 'static,
    {
        self.register(provider, service, Arc::new(Closure(f)));
    }

    /// 登记 store 服务，访问不受限。
    pub fn with_store(&self, backend: SharedStore) {
        self.register(
            "store",
            contracts::store::SERVICE,
            Arc::new(StoreHandler(backend)),
        );
    }

    /// 给插件 `name` 建上下文。`http` 一般是指向 wiremock 的客户端。
    pub fn context(self: &Arc<Self>, name: &str, config: Value, http: reqwest::Client) -> Context {
        let link = Arc::new(TestLink {
            hub: self.clone(),
            name: name.to_string(),
        });
        Context::new(link, name, config, http, std::env::temp_dir().join(name))
    }

    /// 以主人身份调用服务。
    pub async fn call(
        &self,
        service: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        let h = self.handler(service, None)?;
        h.call(Caller::owner("test"), method, params).await
    }

    /// 已发出的某类事件。
    pub fn published(&self, kind: &str) -> Vec<Event> {
        self.state
            .lock()
            .unwrap()
            .published
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    /// 后台任务的失败说明。
    pub fn failures(&self) -> Vec<String> {
        self.state.lock().unwrap().failures.clone()
    }

    /// 把事件投递给全部订阅者，任一处理器失败即返回错误。
    pub async fn deliver(&self, kind: &str, payload: Value) -> Result<(), PluginError> {
        let ev = self.make_event(kind, payload, None, "test");
        let subs = self
            .state
            .lock()
            .unwrap()
            .subscribers
            .get(kind)
            .cloned()
            .unwrap_or_default();
        for f in subs {
            f(ev.clone()).await?;
        }
        Ok(())
    }

    /// 停止：执行 on_stop，再中止后台任务。
    pub async fn stop(&self) {
        let (stops, tasks) = {
            let mut s = self.state.lock().unwrap();
            (std::mem::take(&mut s.stops), std::mem::take(&mut s.tasks))
        };
        for f in stops.into_iter().rev() {
            f().await;
        }
        for t in tasks {
            t.abort();
        }
    }

    fn handler(
        &self,
        service: &str,
        provider: Option<&str>,
    ) -> Result<Arc<dyn ServiceHandler>, ServiceError> {
        let s = self.state.lock().unwrap();
        let ps = s
            .services
            .get(service)
            .ok_or_else(|| ServiceError::unavailable(format!("服务 {service} 不可用")))?;
        let h = match provider {
            Some(p) => ps.get(p),
            None => ps.values().next(),
        };
        h.cloned()
            .ok_or_else(|| ServiceError::unavailable(format!("服务 {service} 不可用")))
    }

    fn make_event(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
        source: &str,
    ) -> Event {
        Event {
            id: self.next_id.fetch_add(1, Ordering::Relaxed) + 1,
            kind: kind.to_string(),
            trace_id: trace_id.unwrap_or_else(contracts::new_trace_id),
            source: source.to_string(),
            created_at: contracts::now_rfc3339(),
            payload,
        }
    }
}

struct TestLink {
    hub: Arc<TestHub>,
    name: String,
}

impl HubLink for TestLink {
    fn call(
        &self,
        service: &str,
        provider: Option<&str>,
        method: &str,
        params: Value,
    ) -> BoxFuture<'static, Result<Value, ServiceError>> {
        let h = self.hub.handler(service, provider);
        let caller = Caller::owner(self.name.clone());
        let method = method.to_string();
        Box::pin(async move { h?.call(caller, &method, params).await })
    }

    fn providers(&self, service: &str) -> Vec<String> {
        self.hub
            .state
            .lock()
            .unwrap()
            .services
            .get(service)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn publish(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<i64, PluginError>> {
        let ev = self.hub.make_event(kind, payload, trace_id, &self.name);
        let id = ev.id;
        self.hub.state.lock().unwrap().published.push(ev);
        Box::pin(async move { Ok(id) })
    }

    fn subscribe(&self, kind: &str, f: EventFn) -> Result<(), PluginError> {
        self.hub
            .state
            .lock()
            .unwrap()
            .subscribers
            .entry(kind.to_string())
            .or_default()
            .push(f);
        Ok(())
    }

    fn intercept(&self, kind: &str, f: InterceptFn) -> Result<(), PluginError> {
        self.hub
            .state
            .lock()
            .unwrap()
            .interceptors
            .entry(kind.to_string())
            .or_default()
            .push(f);
        Ok(())
    }

    fn emit_intercept(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<Option<Value>, PluginError>> {
        let fs = self
            .hub
            .state
            .lock()
            .unwrap()
            .interceptors
            .get(kind)
            .cloned()
            .unwrap_or_default();
        let mut ev = self.hub.make_event(kind, payload, trace_id, &self.name);
        Box::pin(async move {
            for f in fs {
                match f(ev.clone()).await? {
                    Intercept::Continue(p) => ev.payload = p,
                    Intercept::Stop => return Ok(None),
                }
            }
            Ok(Some(ev.payload))
        })
    }

    fn provide(&self, service: &str, h: Arc<dyn ServiceHandler>) -> Result<(), PluginError> {
        self.hub.register(&self.name, service, h);
        Ok(())
    }

    fn spawn(&self, name: &str, fut: BoxFuture<'static, Result<(), PluginError>>) {
        let hub = self.hub.clone();
        let label = format!("{}/{}", self.name, name);
        let t = tokio::spawn(async move {
            if let Err(e) = fut.await {
                hub.state
                    .lock()
                    .unwrap()
                    .failures
                    .push(format!("{label}: {e}"));
            }
        });
        self.hub.state.lock().unwrap().tasks.push(t);
    }

    fn on_stop(&self, f: StopFn) {
        self.hub.state.lock().unwrap().stops.push(f);
    }
}

struct Closure<F>(F);

impl<F, Fut> ServiceHandler for Closure<F>
where
    F: Fn(String, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ServiceError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin((self.0)(method.to_string(), params))
    }
}

struct StoreHandler(SharedStore);

impl ServiceHandler for StoreHandler {
    fn call<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            dispatch_store_call(&*self.0, TableAccess::All, &caller.plugin, method, params).await
        })
    }
}

/// 不带代理的 HTTP 客户端，只给测试连 wiremock 用。
pub fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("构建测试 HTTP 客户端")
}

/// 测试用的数据目录。
pub fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "crypto-news-test-{name}-{}",
        contracts::new_trace_id()
    ));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn provide_call_publish_deliver() {
        let hub = TestHub::new();
        let ctx = hub.context("a", json!({}), test_http_client());
        ctx.provide_fn("svc", |c, m, _p| async move {
            Ok(json!({ "from": c.plugin, "m": m }))
        })
        .unwrap();
        let seen = Arc::new(Mutex::new(0));
        let s2 = seen.clone();
        ctx.subscribe("k", move |_e| {
            let s = s2.clone();
            async move {
                *s.lock().unwrap() += 1;
                Ok(())
            }
        })
        .unwrap();

        let other = hub.context("b", json!({}), test_http_client());
        let r = other.service("svc").call("x", json!({})).await.unwrap();
        assert_eq!(r, json!({ "from": "b", "m": "x" }));
        assert_eq!(other.services("svc").len(), 1);

        other.publish("k", &json!({ "n": 1 })).await.unwrap();
        assert_eq!(hub.published("k").len(), 1);
        hub.deliver("k", json!({})).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), 1);
        hub.stop().await;
    }
}

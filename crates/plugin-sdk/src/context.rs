use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use contracts::{
    BoxFuture, Caller, Event, Intercept, Service, ServiceCall, ServiceError, ServiceHandler,
};
use serde_json::Value;

use crate::PluginError;

pub type EventFn = Arc<dyn Fn(Event) -> BoxFuture<'static, Result<(), PluginError>> + Send + Sync>;
pub type InterceptFn =
    Arc<dyn Fn(Event) -> BoxFuture<'static, Result<Intercept, PluginError>> + Send + Sync>;
pub type StopFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// 插件与 hub 之间的连接。进程内由 hub 实现，扩展插件由 JSON-RPC 运行时实现，
/// 测试用 [`crate::testing::TestHub`]。插件不直接用它，用 [`Context`]。
pub trait HubLink: Send + Sync {
    /// 调用服务。`provider` 为空时取唯一的提供者（多个时按插件名取第一个）。
    fn call(
        &self,
        service: &str,
        provider: Option<&str>,
        method: &str,
        params: Value,
    ) -> BoxFuture<'static, Result<Value, ServiceError>>;
    /// 当前在运行的提供者插件名，按名字排序。
    fn providers(&self, service: &str) -> Vec<String>;
    fn publish(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<i64, PluginError>>;
    fn subscribe(&self, kind: &str, f: EventFn) -> Result<(), PluginError>;
    fn intercept(&self, kind: &str, f: InterceptFn) -> Result<(), PluginError>;
    /// 发出可拦截事件：返回最终的载荷，被拦下时为 None。
    fn emit_intercept(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<Option<Value>, PluginError>>;
    fn provide(&self, service: &str, h: Arc<dyn ServiceHandler>) -> Result<(), PluginError>;
    fn spawn(&self, name: &str, fut: BoxFuture<'static, Result<(), PluginError>>);
    fn on_stop(&self, f: StopFn);
}

/// 插件上下文：每次 `start` 都会拿到一个新的。登记的服务、订阅和任务在插件停止时由 hub 清理。
#[derive(Clone)]
pub struct Context {
    link: Arc<dyn HubLink>,
    name: String,
    config: Value,
    http: reqwest::Client,
    data_dir: PathBuf,
}

impl Context {
    pub fn new(
        link: Arc<dyn HubLink>,
        name: impl Into<String>,
        config: Value,
        http: reqwest::Client,
        data_dir: PathBuf,
    ) -> Self {
        Context {
            link,
            name: name.into(),
            config,
            http,
            data_dir,
        }
    }

    /// 插件名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 本插件的配置（`config.toml` 中 `[plugins.<名字>]` 的内容）。
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// 把配置解析成具体类型。
    pub fn config_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, PluginError> {
        serde_json::from_value(self.config.clone())
            .map_err(|e| PluginError::new(format!("插件 {} 配置错误：{e}", self.name)))
    }

    /// hub 构建的唯一 HTTP 客户端，强制走代理。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn link(&self) -> &Arc<dyn HubLink> {
        &self.link
    }

    /// 唯一的提供者（有多个时按插件名取第一个）。服务不可用时，调用返回 -32000。
    pub fn service(&self, service: &str) -> Service {
        Arc::new(BoundService {
            link: self.link.clone(),
            service: service.to_string(),
            provider: None,
        })
    }

    /// 全部在运行的提供者：`(插件名, 服务)`。依赖里写了 `all = true` 时用。每次调用都重新查询。
    pub fn services(&self, service: &str) -> Vec<(String, Service)> {
        self.link
            .providers(service)
            .into_iter()
            .map(|p| {
                let s: Service = Arc::new(BoundService {
                    link: self.link.clone(),
                    service: service.to_string(),
                    provider: Some(p.clone()),
                });
                (p, s)
            })
            .collect()
    }

    pub fn store(&self) -> contracts::store::StoreClient {
        contracts::store::StoreClient(self.service(contracts::store::SERVICE))
    }

    /// 发出事件，返回事件 ID（ephemeral 事件为 0）。
    pub async fn publish<T: serde::Serialize>(
        &self,
        kind: &str,
        payload: &T,
    ) -> Result<i64, PluginError> {
        let v = serde_json::to_value(payload)?;
        self.link.publish(kind, v, None).await
    }

    /// 沿用已有的 trace ID 发出事件。
    pub async fn publish_traced<T: serde::Serialize>(
        &self,
        kind: &str,
        payload: &T,
        trace_id: &str,
    ) -> Result<i64, PluginError> {
        let v = serde_json::to_value(payload)?;
        self.link.publish(kind, v, Some(trace_id.to_string())).await
    }

    /// 订阅事件。处理器返回 Err 时 hub 退避重试，所以处理器要幂等。
    pub fn subscribe<F, Fut>(&self, kind: &str, f: F) -> Result<(), PluginError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), PluginError>> + Send + 'static,
    {
        self.link.subscribe(
            kind,
            Arc::new(move |e| Box::pin(f(e)) as BoxFuture<'static, _>),
        )
    }

    /// 订阅可拦截事件。
    pub fn intercept<F, Fut>(&self, kind: &str, f: F) -> Result<(), PluginError>
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Intercept, PluginError>> + Send + 'static,
    {
        self.link.intercept(
            kind,
            Arc::new(move |e| Box::pin(f(e)) as BoxFuture<'static, _>),
        )
    }

    /// 发出可拦截事件，返回最终载荷；被拦下时为 None。
    pub async fn emit_intercept(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> Result<Option<Value>, PluginError> {
        self.link.emit_intercept(kind, payload, trace_id).await
    }

    /// 登记服务。
    pub fn provide(&self, service: &str, h: Arc<dyn ServiceHandler>) -> Result<(), PluginError> {
        self.link.provide(service, h)
    }

    /// 用闭包登记服务：`f(caller, method, params)`。
    pub fn provide_fn<F, Fut>(&self, service: &str, f: F) -> Result<(), PluginError>
    where
        F: Fn(Caller, String, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ServiceError>> + Send + 'static,
    {
        self.link.provide(service, Arc::new(FnService(f)))
    }

    /// 启动后台任务。任务返回 Err 或 panic 算插件失败，hub 会清理并重启插件。
    pub fn spawn<Fut>(&self, name: &str, fut: Fut)
    where
        Fut: Future<Output = Result<(), PluginError>> + Send + 'static,
    {
        self.link.spawn(name, Box::pin(fut));
    }

    /// 插件停止时执行（在中止后台任务之前）。
    pub fn on_stop<F, Fut>(&self, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.link
            .on_stop(Box::new(move || Box::pin(f()) as BoxFuture<'static, ()>));
    }
}

struct BoundService {
    link: Arc<dyn HubLink>,
    service: String,
    provider: Option<String>,
}

impl ServiceCall for BoundService {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        self.link
            .call(&self.service, self.provider.as_deref(), method, params)
    }
}

struct FnService<F>(F);

impl<F, Fut> ServiceHandler for FnService<F>
where
    F: Fn(Caller, String, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ServiceError>> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin((self.0)(caller, method.to_string(), params))
    }
}

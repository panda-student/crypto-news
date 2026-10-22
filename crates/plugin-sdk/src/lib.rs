//! 编写插件的统一接口。官方插件和扩展插件用同一套写法：实现 [`Plugin`]，
//! 在 `start` 里通过 [`Context`] 登记服务、订阅事件、启动后台任务。

mod context;
pub mod extension;
mod http;
pub mod manifest;
pub mod rpc;
pub mod testing;

pub use context::{Context, EventFn, HubLink, InterceptFn, StopFn};
pub use contracts;
pub use contracts::{BoxFuture, Caller, Event, Intercept, ServiceError, ServiceHandler};
pub use extension::run_extension;
pub use http::build_http_client;
pub use manifest::Manifest;

/// 插件错误：只带一条说明，不能包含密钥。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PluginError(pub String);

impl PluginError {
    pub fn new(msg: impl std::fmt::Display) -> Self {
        PluginError(msg.to_string())
    }
}

impl From<ServiceError> for PluginError {
    fn from(e: ServiceError) -> Self {
        PluginError(e.to_string())
    }
}
impl From<serde_json::Error> for PluginError {
    fn from(e: serde_json::Error) -> Self {
        PluginError(format!("JSON：{e}"))
    }
}
impl From<reqwest::Error> for PluginError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest 的错误可能带完整 URL（含查询参数里的密钥），只保留类别和状态码
        PluginError(http::describe_error(&e))
    }
}
impl From<String> for PluginError {
    fn from(s: String) -> Self {
        PluginError(s)
    }
}
impl From<&str> for PluginError {
    fn from(s: &str) -> Self {
        PluginError(s.to_string())
    }
}

/// 插件。官方插件编进 app；扩展插件在自己的 `main` 里调用 [`run_extension`]。
pub trait Plugin: Send + Sync + 'static {
    /// `plugin.toml` 的内容，官方插件用 `include_str!`。
    fn manifest(&self) -> &str;

    /// 启动：登记服务、订阅和任务后返回。失败或登记的任务出错时，hub 会清理并退避重启，
    /// 重启时会再次调用 `start`，所以这里不要只做一次性的初始化。
    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>>;
}

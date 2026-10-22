//! 官方表结构、事件与服务接口。只增不删，改动由负责人完成。
//!
//! 所有服务调用都是"服务名 + 方法名 + JSON 参数 → JSON 结果"；本 crate 为官方服务提供
//! 类型化客户端，把调用包成普通的 Rust 方法。

pub mod agent;
pub mod auth;
pub mod channel;
pub mod dispatch;
pub mod events;
pub mod llm;
pub mod rulebook;
pub mod screen;
pub mod secrets;
pub mod source;
pub mod store;
pub mod x;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 主人的用户 ID，首版只有这一个用户。
pub const OWNER: &str = "owner";

/// 服务调用方，由 hub 填写，插件无法伪造。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub plugin: String,
    pub user_id: String,
    pub trace_id: String,
}

impl Caller {
    pub fn owner(plugin: impl Into<String>) -> Self {
        Caller {
            plugin: plugin.into(),
            user_id: OWNER.to_string(),
            trace_id: new_trace_id(),
        }
    }
}

/// 服务错误，错误码与 JSON-RPC 共用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: i64,
    pub message: String,
}

impl ServiceError {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const UNAVAILABLE: i64 = -32000;
    pub const FORBIDDEN: i64 = -32001;
    pub const INTERNAL: i64 = -32002;
    /// 被限流，稍后可重试
    pub const RATE_LIMITED: i64 = -32003;
    /// 凭证无效或过期
    pub const CREDENTIAL_EXPIRED: i64 = -32004;
    /// 额度用完
    pub const QUOTA_EXHAUSTED: i64 = -32005;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        ServiceError {
            code,
            message: message.into(),
        }
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(Self::METHOD_NOT_FOUND, format!("方法不存在：{method}"))
    }
    pub fn invalid_params(msg: impl std::fmt::Display) -> Self {
        Self::new(Self::INVALID_PARAMS, format!("参数错误：{msg}"))
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(Self::UNAVAILABLE, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(Self::FORBIDDEN, msg)
    }
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::new(Self::INTERNAL, msg.to_string())
    }
    pub fn rate_limited(msg: impl Into<String>) -> Self {
        Self::new(Self::RATE_LIMITED, msg)
    }
    pub fn credential_expired(msg: impl Into<String>) -> Self {
        Self::new(Self::CREDENTIAL_EXPIRED, msg)
    }
    pub fn quota_exhausted(msg: impl Into<String>) -> Self {
        Self::new(Self::QUOTA_EXHAUSTED, msg)
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ServiceError {}

impl From<serde_json::Error> for ServiceError {
    fn from(e: serde_json::Error) -> Self {
        ServiceError::invalid_params(e)
    }
}

/// 服务提供方实现的接口。
pub trait ServiceHandler: Send + Sync {
    fn call<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>>;
}

/// 调用方拿到的服务句柄：调用方身份已由 hub 绑定。
pub trait ServiceCall: Send + Sync {
    fn call<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>>;
}

pub type Service = Arc<dyn ServiceCall>;

/// 调用服务并把参数和结果按类型转换。
pub async fn call_typed<P: Serialize, R: serde::de::DeserializeOwned>(
    svc: &dyn ServiceCall,
    method: &str,
    params: &P,
) -> Result<R, ServiceError> {
    let params = serde_json::to_value(params).map_err(ServiceError::internal)?;
    let out = svc.call(method, params).await?;
    serde_json::from_value(out)
        .map_err(|e| ServiceError::internal(format!("结果格式不符（{method}）：{e}")))
}

/// 解析服务参数；失败时返回 -32602。
pub fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ServiceError> {
    serde_json::from_value(params).map_err(ServiceError::invalid_params)
}

/// 把结果转换成 JSON。
pub fn to_result<T: Serialize>(v: &T) -> Result<Value, ServiceError> {
    serde_json::to_value(v).map_err(ServiceError::internal)
}

/// 事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// event_log 的自增 ID，通知事件才有；ephemeral 事件为 0
    pub id: i64,
    pub kind: String,
    /// 32 位十六进制
    pub trace_id: String,
    /// 发出插件，hub 填写
    pub source: String,
    /// RFC3339 UTC
    pub created_at: String,
    pub payload: Value,
}

impl Event {
    /// 把载荷解析成具体类型。
    pub fn parse<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.payload.clone())
    }
}

/// 可拦截事件处理器的返回。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "payload", rename_all = "snake_case")]
pub enum Intercept {
    Continue(Value),
    Stop,
}

/// 新建 trace ID：32 位十六进制。
pub fn new_trace_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut a = RandomState::new().build_hasher();
    a.write_u128(nanos);
    a.write_u64(n);
    let mut b = RandomState::new().build_hasher();
    b.write_u64(n);
    b.write_u128(nanos.rotate_left(17));
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

/// 当前 UTC 时间，RFC3339。
pub fn now_rfc3339() -> String {
    format_rfc3339(time::OffsetDateTime::now_utc())
}

/// 格式化为 RFC3339 UTC（秒精度）。
pub fn format_rfc3339(t: time::OffsetDateTime) -> String {
    let t = t
        .to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .unwrap_or(t);
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// 解析 RFC3339。
pub fn parse_rfc3339(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// hub 自己提供的管理服务 `hub-admin` 1.2。
pub mod hub_admin {
    use super::*;

    pub const SERVICE: &str = "hub-admin";

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PluginInfo {
        pub name: String,
        pub version: String,
        /// official / extension
        pub kind: String,
        /// 待授权 / 缺依赖 / 启动中 / 运行中 / 失败退避 / 已停止 / 停止中 / 已停用
        pub status: String,
        pub missing: Vec<String>,
        pub permissions: Vec<String>,
        pub granted: bool,
        pub enabled: bool,
        pub last_error: Option<String>,
        /// 清单中的配置项说明，原样转成 JSON
        #[serde(default)]
        pub config_fields: Value,
        /// 清单里的功能说明
        #[serde(default)]
        pub description: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PluginList {
        pub plugins: Vec<PluginInfo>,
    }

    /// 插件当前生效的配置。`overrides` 是在界面里改过、覆盖 config.toml 的部分。
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct PluginConfig {
        pub name: String,
        pub config: Value,
        #[serde(default)]
        pub overrides: Value,
    }

    /// 应用级设置的键。值在下次启动时生效。
    pub mod app_keys {
        /// 代理地址；空字符串表示直连
        pub const PROXY: &str = "proxy";
        /// 1.2：开机自启（布尔）。只有桌面外壳会读取并应用它
        pub const AUTOSTART: &str = "autostart";
        /// 1.2：关闭窗口时缩到托盘（布尔，缺省 true）。只有桌面外壳会读取
        pub const CLOSE_TO_TRAY: &str = "close_to_tray";
        /// 1.2：只读的运行时信息，当前外壳："desktop" 或 null（`app serve` 单独运行）
        pub const SHELL: &str = "shell";
        pub const AUTO_UPDATE: &str = "auto_update";
        pub const UPDATE_ACTION: &str = "update_action";
        pub const UPDATE_STATUS: &str = "update_status";
    }

    #[derive(Clone)]
    pub struct HubAdminClient(pub Service);

    impl HubAdminClient {
        pub async fn plugins(&self) -> Result<Vec<PluginInfo>, ServiceError> {
            let r: PluginList = call_typed(&*self.0, "plugins", &serde_json::json!({})).await?;
            Ok(r.plugins)
        }
        pub async fn grant(&self, name: &str) -> Result<(), ServiceError> {
            self.0
                .call("grant", serde_json::json!({ "name": name }))
                .await?;
            Ok(())
        }
        pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), ServiceError> {
            self.0
                .call(
                    "set_enabled",
                    serde_json::json!({ "name": name, "enabled": enabled }),
                )
                .await?;
            Ok(())
        }
        /// 1.1：读插件配置。
        pub async fn config_get(&self, name: &str) -> Result<PluginConfig, ServiceError> {
            call_typed(&*self.0, "config_get", &serde_json::json!({ "name": name })).await
        }
        /// 1.1：改插件配置并重启该插件。`values` 是 {字段: 值}，值为 null 表示恢复默认。
        /// 只能改清单里声明过的非密钥字段。
        pub async fn config_set(&self, name: &str, values: Value) -> Result<(), ServiceError> {
            self.0
                .call(
                    "config_set",
                    serde_json::json!({ "name": name, "values": values }),
                )
                .await?;
            Ok(())
        }
        /// 1.1：重启一个插件。
        pub async fn restart(&self, name: &str) -> Result<(), ServiceError> {
            self.0
                .call("restart", serde_json::json!({ "name": name }))
                .await?;
            Ok(())
        }
        /// 1.1：读应用级设置，没设过时返回当前生效的值（没有则 null）。
        pub async fn app_get(&self, key: &str) -> Result<Value, ServiceError> {
            let v = self
                .0
                .call("app_get", serde_json::json!({ "key": key }))
                .await?;
            Ok(v.get("value").cloned().unwrap_or(Value::Null))
        }
        /// 1.1：写应用级设置，下次启动生效。
        pub async fn app_set(&self, key: &str, value: Value) -> Result<(), ServiceError> {
            self.0
                .call("app_set", serde_json::json!({ "key": key, "value": value }))
                .await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_is_32_hex_and_unique() {
        let a = new_trace_id();
        let b = new_trace_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn rfc3339_roundtrip() {
        let s = now_rfc3339();
        assert!(s.ends_with('Z'));
        assert!(parse_rfc3339(&s).is_some());
    }

    #[test]
    fn level_serde_and_parse() {
        use crate::screen::Level;
        assert_eq!(serde_json::to_value(Level::Urgent).unwrap(), "urgent");
        assert_eq!(Level::parse("重要"), Some(Level::Important));
        assert_eq!(Level::parse("archive"), Some(Level::Archive));
        assert!(Level::Urgent < Level::Archive);
        let s: crate::channel::ChannelState =
            serde_json::from_value("quota_exhausted".into()).unwrap();
        assert_eq!(s, crate::channel::ChannelState::QuotaExhausted);
    }

    #[test]
    fn intercept_serde() {
        let v = serde_json::to_value(Intercept::Continue(serde_json::json!({"a":1}))).unwrap();
        assert_eq!(v["action"], "continue");
        let s: Intercept = serde_json::from_value(serde_json::json!({"action":"stop"})).unwrap();
        assert_eq!(s, Intercept::Stop);
    }
}

pub mod focus;

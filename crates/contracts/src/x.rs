//! X 采集管理服务 `x-admin` 1.0，由 x-browser 插件提供。
//!
//! x-browser 同时提供 `source` 服务（信源 ID 为 `x:<账号>`），本文件只定义管理接口。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const ADMIN_SERVICE: &str = "x-admin";

/// 采集器状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XStatus {
    /// 找到的浏览器名称（Chrome / Edge / Chromium），找不到时为 None，X 信源停用
    #[serde(default)]
    pub browser: Option<String>,
    /// 浏览器配置目录里是否有有效的 X 登录状态
    pub logged_in: bool,
    /// 登录窗口是否正开着
    #[serde(default)]
    pub login_window_open: bool,
    /// 最近一次错误，给用户看的中文说明，不含密钥或 Cookie
    #[serde(default)]
    pub last_error: Option<String>,
}

/// 关注的一个 X 账号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XAccount {
    /// 不带 @ 的用户名，小写比较
    pub handle: String,
    /// 暂停的账号不出现在 `source.list` 里，也不抓取
    pub paused: bool,
    /// 内置账号（首次启动时写入的默认清单）
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XAccountList {
    pub accounts: Vec<XAccount>,
}

/// `x-admin` 1.0：`status` → XStatus；`login` → XStatus（打开有界面的浏览器窗口，
/// 用户自己登录，本服务不接收账号密码）；`list` → `{accounts}`；`add {handle}` → XAccount；
/// `delete {handle}` → `{ok}`；`set_paused {handle, paused}` → XAccount。
#[derive(Clone)]
pub struct XAdminClient(pub Service);

impl XAdminClient {
    pub async fn status(&self) -> Result<XStatus, ServiceError> {
        call_typed(&*self.0, "status", &json!({})).await
    }
    pub async fn login(&self) -> Result<XStatus, ServiceError> {
        call_typed(&*self.0, "login", &json!({})).await
    }
    pub async fn list(&self) -> Result<Vec<XAccount>, ServiceError> {
        let r: XAccountList = call_typed(&*self.0, "list", &json!({})).await?;
        Ok(r.accounts)
    }
    pub async fn add(&self, handle: &str) -> Result<XAccount, ServiceError> {
        call_typed(&*self.0, "add", &json!({ "handle": handle })).await
    }
    pub async fn delete(&self, handle: &str) -> Result<(), ServiceError> {
        self.0.call("delete", json!({ "handle": handle })).await?;
        Ok(())
    }
    pub async fn set_paused(&self, handle: &str, paused: bool) -> Result<XAccount, ServiceError> {
        call_typed(
            &*self.0,
            "set_paused",
            &json!({ "handle": handle, "paused": paused }),
        )
        .await
    }
}

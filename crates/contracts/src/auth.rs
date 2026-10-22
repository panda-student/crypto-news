//! `auth` 服务 1.0：认证与权限的统一入口，首版恒为主人。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "auth";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentUser {
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub allowed: bool,
}

#[derive(Clone)]
pub struct AuthClient(pub Service);

impl AuthClient {
    pub async fn current(&self) -> Result<CurrentUser, ServiceError> {
        call_typed(&*self.0, "current", &json!({})).await
    }
    pub async fn check(&self, action: &str) -> Result<bool, ServiceError> {
        let r: CheckResult = call_typed(&*self.0, "check", &json!({ "action": action })).await?;
        Ok(r.allowed)
    }
}

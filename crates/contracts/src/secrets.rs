//! `secrets` 服务 1.0：加密保存的密钥，secrets 插件提供。
//!
//! 键名格式 `<插件名>.<字段名>`，对应插件清单里 type = "secret" 的配置项。
//! 权限：插件只能读写自己前缀下的键；主人身份的 web / operations 能写、删任意键、列出全部，但读不到明文。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "secrets";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretValue {
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub key: String,
    pub configured: bool,
    /// RFC3339 UTC
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretList {
    pub secrets: Vec<SecretEntry>,
}

#[derive(Clone)]
pub struct SecretsClient(pub Service);

impl SecretsClient {
    /// 未配置时返回 None。
    pub async fn get(&self, key: &str) -> Result<Option<String>, ServiceError> {
        let r: SecretValue = call_typed(&*self.0, "get", &json!({ "key": key })).await?;
        Ok(r.value)
    }
    pub async fn set(&self, key: &str, value: &str) -> Result<(), ServiceError> {
        self.0
            .call("set", json!({ "key": key, "value": value }))
            .await?;
        Ok(())
    }
    pub async fn delete(&self, key: &str) -> Result<(), ServiceError> {
        self.0.call("delete", json!({ "key": key })).await?;
        Ok(())
    }
    /// 主人身份的 web / operations 得到全部键；其他插件只得到自己前缀下的键。
    pub async fn list(&self) -> Result<Vec<SecretEntry>, ServiceError> {
        let r: SecretList = call_typed(&*self.0, "list", &json!({})).await?;
        Ok(r.secrets)
    }
}

//! `rulebook` 服务 1.0：一份规则文本，界面可编辑；分级与简析的判断标准写在这里。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "rulebook";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rulebook {
    pub text: String,
    /// RFC3339 UTC；从未保存过时为空，此时 text 是内置默认规则
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Clone)]
pub struct RulebookClient(pub Service);

impl RulebookClient {
    pub async fn get(&self) -> Result<Rulebook, ServiceError> {
        call_typed(&*self.0, "get", &json!({})).await
    }
    /// 只允许 web 调用。
    pub async fn set(&self, text: &str) -> Result<Rulebook, ServiceError> {
        call_typed(&*self.0, "set", &json!({ "text": text })).await
    }
}

//! app-operations 1.0: AI may prepare plans; only web's confirmation UI may execute.
use crate::{call_typed, Service, ServiceError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
pub const SERVICE: &str = "app-operations";
#[derive(Clone, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub tool: String,
    pub arguments: Value,
    #[serde(default)]
    pub session_id: Option<i64>,
}
#[derive(Default, Deserialize, Serialize)]
pub struct Confirmation {
    #[serde(default)]
    pub secrets: HashMap<String, String>,
}
#[derive(Clone)]
pub struct OperationsClient(pub Service);
impl OperationsClient {
    pub async fn catalogue(&self) -> Result<Vec<Value>, ServiceError> {
        call_typed(&*self.0, "catalogue", &json!({})).await
    }
    pub async fn inspect(&self) -> Result<Value, ServiceError> {
        self.0.call("inspect", json!({})).await
    }
    pub async fn prepare(&self, req: &PrepareRequest) -> Result<Value, ServiceError> {
        call_typed(&*self.0, "prepare", req).await
    }
    pub async fn plans(&self) -> Result<Value, ServiceError> {
        self.0.call("plans", json!({})).await
    }
    pub async fn status(&self, id: &str) -> Result<Value, ServiceError> {
        self.0.call("status", json!({"id":id})).await
    }
    /// Not available as an MCP/AI tool. Service checks authenticated caller == web.
    pub async fn confirm(&self, id: &str, input: Confirmation) -> Result<Value, ServiceError> {
        self.0
            .call("confirm", json!({"id":id,"secrets":input.secrets}))
            .await
    }
    pub async fn cancel(&self, id: &str) -> Result<Value, ServiceError> {
        self.0.call("cancel", json!({"id":id})).await
    }
}
/// Reject common credential forms before a question or a plan can be persisted.
pub fn contains_secret(text: &str) -> bool {
    text.contains("sk-") || text.contains("key=") || text.contains("Bearer ")
}

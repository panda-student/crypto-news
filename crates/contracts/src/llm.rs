//! `llm` 服务 1.2：模型调用与用量统计，llm 插件提供。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "llm";

/// 模型档位：小模型做初筛与重点简析，大模型做追问；保留旧版简析用途兼容用量记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Small,
    Large,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// system / user / assistant
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tier: Tier,
    /// 要求模型只输出 JSON 对象
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 用途标签，记入用量，如 `screen`、`brief`、`agent`
    #[serde(default)]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
    /// 实际使用的模型名
    pub model: String,
    #[serde(default)]
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageQuery {
    /// RFC3339 UTC，含
    #[serde(default)]
    pub since: Option<String>,
    /// RFC3339 UTC，不含
    #[serde(default)]
    pub until: Option<String>,
}

/// 按日期（UTC）、模型、用途汇总的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRow {
    /// YYYY-MM-DD
    pub day: String,
    pub model: String,
    pub purpose: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageList {
    pub rows: Vec<UsageRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ready {
    pub ready: bool,
}

#[derive(Clone)]
pub struct LlmClient(pub Service);

impl LlmClient {
    /// 错误码：模型服务限流 RATE_LIMITED，密钥无效 CREDENTIAL_EXPIRED，
    /// 额度用完 QUOTA_EXHAUSTED，未配置 UNAVAILABLE，其他失败 INTERNAL。
    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, ServiceError> {
        call_typed(&*self.0, "chat", req).await
    }
    pub async fn usage(&self, q: &UsageQuery) -> Result<Vec<UsageRow>, ServiceError> {
        let r: UsageList = call_typed(&*self.0, "usage", q).await?;
        Ok(r.rows)
    }
    /// 是否已配置可用的模型（不发网络请求）。
    pub async fn ready(&self) -> Result<bool, ServiceError> {
        let r: Ready = call_typed(&*self.0, "ready", &json!({})).await?;
        Ok(r.ready)
    }
}

// ---- 1.1：流式输出 ----

/// 流式片段（ephemeral）：llm 每收到一段输出就发一次。
pub const LLM_DELTA: &str = "llm.delta";
/// 取消流式调用（ephemeral）：llm 收到后停止该流，`chat_stream` 返回已生成的部分。
pub const LLM_CANCEL: &str = "llm.cancel";

/// `chat_stream` 的参数：普通请求加一个由调用方生成的流 ID。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChatRequest {
    #[serde(flatten)]
    pub request: ChatRequest,
    /// 调用方生成、全局唯一，用来对应 `llm.delta` 和 `llm.cancel`
    pub stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChatResponse {
    #[serde(flatten)]
    pub response: ChatResponse,
    /// 被 `llm.cancel` 停止时为 true，content 是停止前已生成的部分
    #[serde(default)]
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmDelta {
    pub stream_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCancel {
    pub stream_id: String,
}

/// 测试连接的参数：用指定的接口与模型发一个极短的请求，不记用量。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TestRequest {
    /// 不填则测当前配置
    #[serde(default)]
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub ok: bool,
    /// 失败原因（不含密钥）；成功时为实际模型名
    pub message: String,
    pub latency_ms: u64,
}

impl LlmClient {
    /// 流式调用：输出片段以 `llm.delta` 事件发出，返回完整结果。
    /// 不支持流式的接口退化为一次性输出，整段作为一个片段发出。
    pub async fn chat_stream(
        &self,
        req: &StreamChatRequest,
    ) -> Result<StreamChatResponse, ServiceError> {
        call_typed(&*self.0, "chat_stream", req).await
    }
    /// 测试连接。网络或密钥错误放在 `TestResult.message`，不作为服务错误返回。
    pub async fn test(&self, req: &TestRequest) -> Result<TestResult, ServiceError> {
        call_typed(&*self.0, "test", req).await
    }
}

// ---- 1.2：多个模型服务与用途分配 ----

/// 用途标签，同时也是 `ChatRequest.purpose` 的取值。
pub mod purposes {
    /// 初筛：给新消息打级别
    pub const SCREEN: &str = "screen";
    /// 简析：重要消息的影响分析
    pub const ANALYZE: &str = "analyze";
    /// 追问
    pub const AGENT: &str = "agent";
    /// 日报
    pub const DIGEST: &str = "digest";
    /// 界面里可分配的用途及中文名，按显示顺序
    pub const ALL: &[(&str, &str)] = &[
        (SCREEN, "初筛与重点简析"),
        (ANALYZE, "旧版简析（已停用自动调用）"),
        (AGENT, "追问"),
        (DIGEST, "日报"),
    ];
}

/// 接口类型。
pub mod provider_kinds {
    pub const OPENAI: &str = "openai";
    pub const ANTHROPIC: &str = "anthropic";
}

/// 一个模型服务。密钥只给出是否已设置，从不返回明文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// `provider_kinds` 之一
    pub kind: String,
    pub base_url: String,
    /// 可用模型，第一个是这个服务的默认模型
    pub models: Vec<String>,
    pub has_key: bool,
    pub is_default: bool,
    /// 最近一次测试连接的结果；没测过为 None
    #[serde(default)]
    pub last_test: Option<TestResult>,
}

/// 某个用途用哪个服务的哪个模型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// `purposes` 之一
    pub purpose: String,
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderList {
    pub providers: Vec<ProviderInfo>,
    /// 只列出显式分配过的用途；没分配的用途走默认服务
    pub assignments: Vec<Assignment>,
}

/// 新建（id 为 None）或修改一个模型服务。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderInput {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub models: Vec<String>,
    /// None 表示不修改；新建时必须提供
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderId {
    pub id: String,
}

impl LlmClient {
    /// 列出全部模型服务和用途分配。
    pub async fn providers(&self) -> Result<ProviderList, ServiceError> {
        call_typed(&*self.0, "providers", &json!({})).await
    }
    /// 保存模型服务，返回保存后的信息。第一个服务自动成为默认。
    /// 密钥写入 secrets（键 `llm.key.<id>`），不写日志。
    pub async fn provider_save(&self, input: &ProviderInput) -> Result<ProviderInfo, ServiceError> {
        call_typed(&*self.0, "provider_save", input).await
    }
    /// 删除模型服务及其密钥；不能删除默认服务。指向它的用途分配一并删除。
    pub async fn provider_delete(&self, id: &str) -> Result<(), ServiceError> {
        let _: Value =
            call_typed(&*self.0, "provider_delete", &ProviderId { id: id.into() }).await?;
        Ok(())
    }
    pub async fn set_default(&self, id: &str) -> Result<(), ServiceError> {
        let _: Value = call_typed(&*self.0, "set_default", &ProviderId { id: id.into() }).await?;
        Ok(())
    }
    /// 设置用途分配；model 必须在该服务的模型列表里。
    pub async fn assign(&self, a: &Assignment) -> Result<(), ServiceError> {
        let _: Value = call_typed(&*self.0, "assign", a).await?;
        Ok(())
    }
}

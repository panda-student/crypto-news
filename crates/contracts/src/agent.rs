//! `agent` 服务 1.0：界面里的追问，agent 插件提供。
//!
//! 回答只依据系统里已采集的消息，分"结论 / 偏利好 / 偏利空 / 待确认"四段，
//! 不做价格预测，不给投资建议。

use serde::{Deserialize, Serialize};

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "agent";

/// 同一会话里之前的一轮问答，用来理解追问。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub question: String,
    /// 上一轮回答的结论段
    pub answer: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskRequest {
    /// 最多 500 字，超出部分截掉
    pub question: String,
    /// 针对某条消息追问时填它的 ID，这条消息一定会放进资料
    #[serde(default)]
    pub item_id: Option<i64>,
    /// Explicit selection, including an empty selection that clears legacy context.
    #[serde(default)]
    pub item_ids: Option<Vec<i64>>,
    /// 同一会话之前的问答，按时间顺序，最多取最后 5 轮
    #[serde(default)]
    pub history: Vec<ChatTurn>,
}

/// 回答引用的一条消息，序号即正文里的 `[n]`（从 1 开始）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub item_id: i64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskReply {
    /// false 表示资料里没有相关消息，此时只有 conclusion 有内容
    pub found: bool,
    /// 一两句话的结论
    pub conclusion: String,
    /// 偏利好的要点，每条末尾带 `[n]`
    #[serde(default)]
    pub bullish: Vec<String>,
    /// 偏利空的要点
    #[serde(default)]
    pub bearish: Vec<String>,
    /// 还没有证实、需要继续关注的点
    #[serde(default)]
    pub unconfirmed: Vec<String>,
    #[serde(default)]
    pub citations: Vec<Citation>,
}

#[derive(Clone)]
pub struct AgentClient(pub Service);

impl AgentClient {
    /// 一次性返回完整回答。模型不可用时返回 `UNAVAILABLE`。
    pub async fn ask(&self, req: &AskRequest) -> Result<AskReply, ServiceError> {
        call_typed(&*self.0, "ask", req).await
    }
}

// ---- 1.1：流式追问 ----

/// `ask_stream` 的参数。回答正文以 `llm.delta` 事件（同一 stream_id）逐段发出。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskStreamRequest {
    #[serde(flatten)]
    pub request: AskRequest,
    /// 调用方生成、全局唯一；取消时发 `llm.cancel`
    pub stream_id: String,
}

/// 正文里 `[n]` 对应的消息，`n` 与流式输出里的编号一致，不重新编号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NumberedCitation {
    pub n: u32,
    pub item_id: i64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AskStreamReply {
    /// false 表示资料里没有相关消息（此时没有发出任何片段，answer 是提示语）
    pub found: bool,
    /// 完整回答正文（纯文本，可含 `[n]` 和换行）
    pub answer: String,
    /// 正文里实际出现的编号，按出现顺序
    #[serde(default)]
    pub citations: Vec<NumberedCitation>,
    #[serde(default)]
    pub cancelled: bool,
}

impl AgentClient {
    /// 流式追问；只允许 web 调用。模型不可用时返回 `UNAVAILABLE` 等错误。
    pub async fn ask_stream(&self, req: &AskStreamRequest) -> Result<AskStreamReply, ServiceError> {
        call_typed(&*self.0, "ask_stream", req).await
    }
}

/// Durable channel conversation record. Desktop continuations use the same conversation key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub id: i64,
    pub turn_key: String,
    pub conversation_key: String,
    pub channel: String,
    pub origin: String,
    pub question: String,
    pub reply_json: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConversationTurn {
    pub turn_key: String,
    pub conversation_key: String,
    pub channel: String,
    pub origin: String,
    pub question: String,
    pub reply_json: String,
}
impl AgentClient {
    pub async fn conversation_turns(
        &self,
        after: i64,
    ) -> Result<Vec<ConversationTurn>, ServiceError> {
        call_typed(
            &*self.0,
            "conversation_turns",
            &serde_json::json!({"after":after}),
        )
        .await
    }
    pub async fn record_conversation_turn(
        &self,
        turn: &RecordConversationTurn,
    ) -> Result<i64, ServiceError> {
        call_typed(&*self.0, "record_conversation_turn", turn).await
    }
}

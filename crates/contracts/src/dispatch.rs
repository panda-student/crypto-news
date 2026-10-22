//! `dispatch` 服务 1.0：发送队列与日报记录的查询和操作，dispatch 插件提供，供 web 使用。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "dispatch";

/// 发送任务种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxKind {
    /// 快讯
    Flash,
    /// 日报
    Digest,
    /// 系统提醒（信源异常、渠道过期等）
    Alert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    /// 等待发送（含退避中、免打扰延后）
    Pending,
    Sent,
    /// 失败 5 次或无可用渠道，转人工
    Manual,
    /// 被拦截丢弃或用户取消
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: i64,
    pub kind: OutboxKind,
    /// 目标渠道 ID；改走备用渠道时会更新
    pub channel: String,
    pub status: OutboxStatus,
    pub attempts: i64,
    /// 同一条日报被分批时的序号，从 1 开始
    pub batch: i64,
    pub text: String,
    #[serde(default)]
    pub item_ids: Vec<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// RFC3339 UTC
    pub created_at: String,
    /// 下次尝试时间
    #[serde(default)]
    pub next_at: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OutboxQuery {
    #[serde(default)]
    pub status: Option<OutboxStatus>,
    #[serde(default)]
    pub kind: Option<OutboxKind>,
    /// 缺省 50，最大 500
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxList {
    pub entries: Vec<OutboxEntry>,
}

/// 队列中通知的完整统计，不受列表分页影响。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueCounts {
    pub channel: String,
    pub pending: i64,
    pub ready: i64,
    pub delayed: i64,
    pub manual: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueSummary {
    pub total: QueueCounts,
    pub channels: Vec<QueueCounts>,
    /// 尚未合并成通知的事件数，与待发通知分开计数。
    pub merging_events: i64,
}

/// 日报记录：按"日期 + 时段"记账，保证每个时段只发一次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestRecord {
    pub id: i64,
    /// 本地日期 YYYY-MM-DD
    pub date: String,
    /// 时段，如 `08:00`
    pub slot: String,
    pub item_count: i64,
    pub important_count: i64,
    #[serde(default)]
    pub outbox_ids: Vec<i64>,
    /// RFC3339 UTC
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestList {
    pub digests: Vec<DigestRecord>,
}

/// 日报预览：两种格式的渲染结果（已分批）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestPreview {
    pub markdown: Vec<String>,
    pub plain: Vec<String>,
    pub item_count: i64,
}

#[derive(Clone)]
pub struct DispatchClient(pub Service);

impl DispatchClient {
    pub async fn queue_summary(&self) -> Result<QueueSummary, ServiceError> {
        call_typed(&*self.0, "queue_summary", &json!({})).await
    }

    pub async fn outbox(&self, q: &OutboxQuery) -> Result<Vec<OutboxEntry>, ServiceError> {
        let r: OutboxList = call_typed(&*self.0, "outbox", q).await?;
        Ok(r.entries)
    }
    /// 把转人工或丢弃的任务重新排队。
    pub async fn retry(&self, id: i64) -> Result<OutboxEntry, ServiceError> {
        call_typed(&*self.0, "retry", &json!({ "id": id })).await
    }
    pub async fn digests(&self, limit: i64) -> Result<Vec<DigestRecord>, ServiceError> {
        let r: DigestList = call_typed(&*self.0, "digests", &json!({ "limit": limit })).await?;
        Ok(r.digests)
    }
    /// 按当前素材渲染日报预览，不入队、不记账。
    pub async fn preview_digest(&self) -> Result<DigestPreview, ServiceError> {
        call_typed(&*self.0, "preview_digest", &json!({})).await
    }
    /// 发一条测试消息到指定渠道（经 outbox）。
    pub async fn test_send(&self, channel: &str) -> Result<OutboxEntry, ServiceError> {
        call_typed(&*self.0, "test_send", &json!({ "channel": channel })).await
    }
}

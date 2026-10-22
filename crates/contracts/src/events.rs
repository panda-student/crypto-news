//! 官方事件的种类与载荷。

use serde::{Deserialize, Serialize};

/// 定时事件（ephemeral，不写日志）。
pub const TIMER_TICK: &str = "timer.tick";
/// 消息入库。
pub const ITEM_COLLECTED: &str = "item.collected";
/// 信源连续失败到 3 次时发一次。
pub const SOURCE_FAILED: &str = "source.failed";
/// 信源失败后第一次成功时发。
pub const SOURCE_RECOVERED: &str = "source.recovered";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerTick {
    /// 定时器名，如 `collect`、`daily-digest`
    pub name: String,
    /// RFC3339 UTC
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemCollected {
    pub item_id: i64,
    pub source_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceFailed {
    pub source_id: String,
    pub consecutive_failures: i64,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRecovered {
    pub source_id: String,
}

/// 消息已分级（含人工改级）。
pub const ITEM_SCREENED: &str = "item.screened";
/// 待推送内容：screen 对紧急消息简析后发（以后 merge、debate 也发）。
pub const PUSH_PENDING: &str = "push.pending";
/// 推送发出前（可拦截）：免打扰延后、合并快讯在这里处理。
pub const PUSH_BEFORE: &str = "push.before";
/// 用户从渠道发来消息（持久）。
pub const USER_MESSAGE: &str = "user.message";
/// 渠道状态变化：绑定、过期、掉线、恢复、额度。
pub const CHANNEL_STATUS: &str = "channel.status";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemScreened {
    pub item_id: i64,
    pub source_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub published_at: Option<String>,
    pub level: crate::screen::Level,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub impact: Option<String>,
    pub method: crate::screen::ScreenMethod,
    /// 开机补抓且发布时间早于 2 小时前：不推快讯，进日报并标"补抓"
    #[serde(default)]
    pub backfill: bool,
    /// 涉及的币种代码，见 `Screening.assets`
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub direction: Option<crate::screen::Direction>,
}

/// 推送里的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushItem {
    pub item_id: i64,
    pub level: crate::screen::Level,
    pub title: String,
    #[serde(default)]
    pub impact: Option<String>,
    pub url: String,
    pub source_id: String,
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushPending {
    /// 目前只有 `flash`
    pub kind: String,
    pub items: Vec<PushItem>,
}

/// `push.before` 的载荷。拦截者可以改 items、设置 defer_until 延后，或返回 Stop 丢弃。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushBefore {
    pub kind: String,
    pub items: Vec<PushItem>,
    /// RFC3339 UTC；设置后到点再发
    #[serde(default)]
    pub defer_until: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 渠道 ID（插件名）
    pub channel: String,
    pub user_id: String,
    /// 回复时作为 `channel.send` 的 reply_to
    pub conversation: String,
    pub text: String,
    pub message_id: String,
    /// RFC3339 UTC
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelStatus {
    pub channel: String,
    pub state: crate::channel::ChannelState,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub quota_left: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

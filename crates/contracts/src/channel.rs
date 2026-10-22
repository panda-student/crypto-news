//! `channel` 服务 1.0：推送渠道，由各渠道插件提供（wechat-ilink、wecom-webhook），
//! 依赖方用 `all = true` 拿到全部提供者，按 `info` 选择。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "channel";

/// 渠道接受的文本格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    /// 企业微信 markdown
    Markdown,
    /// 纯文本（微信）
    Plain,
}

/// 渠道状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    /// 可发送
    Ok,
    /// 未绑定或未配置
    Unbound,
    /// 凭证过期，需要用户操作续期
    Expired,
    /// 额度用完
    QuotaExhausted,
    /// 网络或服务异常，会自行重试
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// 渠道 ID，等于插件名
    pub id: String,
    pub name: String,
    pub format: TextFormat,
    /// 单条消息的字节上限
    pub max_bytes: usize,
    /// 备用渠道：主渠道不可用时紧急消息才走它
    pub backup: bool,
    pub state: ChannelState,
    /// 剩余可主动发送条数；无限制时为空
    #[serde(default)]
    pub quota_left: Option<i64>,
    /// 渠道的额度容量：微信为会话恢复额度，企业微信为每分钟发送上限。
    #[serde(default)]
    pub quota_limit: Option<i64>,
    /// 最近收到用户消息的时间，RFC3339 UTC。
    #[serde(default)]
    pub last_received_at: Option<String>,
    /// 凭证到期时间，RFC3339 UTC
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SendRequest {
    /// 已按 `format` 渲染、不超过 `max_bytes` 的正文
    pub text: String,
    /// 回复某条用户消息时填 `user.message` 里的 conversation
    #[serde(default)]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SendResult {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub quota_left: Option<i64>,
}

/// 发起绑定的结果（扫码登录等）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindInfo {
    pub state: ChannelState,
    /// 需要用户扫码时给出二维码内容（URL 或文本），界面自行渲染
    #[serde(default)]
    pub qrcode: Option<String>,
    /// 二维码图片的 data URL（渠道直接给图片时）
    #[serde(default)]
    pub qrcode_image: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone)]
pub struct ChannelClient(pub Service);

impl ChannelClient {
    pub async fn info(&self) -> Result<ChannelInfo, ServiceError> {
        call_typed(&*self.0, "info", &json!({})).await
    }
    /// 错误码：RATE_LIMITED 限流（可重试）、CREDENTIAL_EXPIRED 凭证过期、
    /// QUOTA_EXHAUSTED 额度用完、UNAVAILABLE 未绑定或网络失败、INVALID_PARAMS 超长。
    pub async fn send(&self, req: &SendRequest) -> Result<SendResult, ServiceError> {
        call_typed(&*self.0, "send", req).await
    }
    /// 发起绑定；不需要绑定的渠道返回 METHOD_NOT_FOUND。只允许 web 调用。
    pub async fn bind(&self) -> Result<BindInfo, ServiceError> {
        call_typed(&*self.0, "bind", &json!({})).await
    }
    /// 查询绑定进度。
    pub async fn bind_status(&self) -> Result<BindInfo, ServiceError> {
        call_typed(&*self.0, "bind_status", &json!({})).await
    }
}

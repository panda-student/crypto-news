//! 渠道状态计算：纯函数，不碰 IO。

use plugin_sdk::contracts::channel::ChannelState;
use plugin_sdk::contracts::format_rfc3339;
use time::{Duration, OffsetDateTime};

/// 收消息循环连续失败达到这个次数算 offline。
pub const OFFLINE_AFTER_FAILURES: u32 = 3;

pub const DETAIL_NO_CREDENTIALS: &str = "请扫码绑定微信";
pub const DETAIL_NO_CONTEXT: &str = "请在微信里给机器人发一条消息";
pub const DETAIL_EXPIRED: &str = "会话已过期，在微信里给机器人发一条消息即可恢复";
pub const DETAIL_QUOTA: &str = "推送额度已用完，在微信里给机器人回复任意内容恢复";
pub const DETAIL_OFFLINE: &str = "收消息连续失败，正在重试";

#[derive(Debug, Clone, Copy)]
pub struct StateInput {
    pub has_credentials: bool,
    pub has_context: bool,
    pub quota_left: i64,
    pub last_user_at: Option<OffsetDateTime>,
    pub now: OffsetDateTime,
    pub session_hours: i64,
    /// 收消息循环是否连续失败
    pub receive_failing: bool,
    /// 服务端明确拒绝了凭证或会话凭证（-14），要等用户重新绑定或发消息才恢复
    pub rejected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Computed {
    pub state: ChannelState,
    pub detail: Option<String>,
    /// RFC3339 UTC
    pub expires_at: Option<String>,
}

pub fn expires_at(last_user_at: OffsetDateTime, session_hours: i64) -> OffsetDateTime {
    last_user_at + Duration::hours(session_hours)
}

pub fn compute_state(i: &StateInput) -> Computed {
    let expires = i
        .last_user_at
        .map(|t| format_rfc3339(expires_at(t, i.session_hours)));
    let make = |state, detail: Option<&str>, expires_at| Computed {
        state,
        detail: detail.map(str::to_string),
        expires_at,
    };
    if !i.has_credentials {
        return make(ChannelState::Unbound, Some(DETAIL_NO_CREDENTIALS), None);
    }
    if !i.has_context {
        return make(ChannelState::Unbound, Some(DETAIL_NO_CONTEXT), None);
    }
    let timed_out = match i.last_user_at {
        Some(t) => i.now >= expires_at(t, i.session_hours),
        None => true,
    };
    if i.rejected || timed_out {
        return make(ChannelState::Expired, Some(DETAIL_EXPIRED), expires);
    }
    if i.quota_left <= 0 {
        return make(ChannelState::QuotaExhausted, Some(DETAIL_QUOTA), expires);
    }
    if i.receive_failing {
        return make(ChannelState::Offline, Some(DETAIL_OFFLINE), expires);
    }
    make(ChannelState::Ok, None, expires)
}

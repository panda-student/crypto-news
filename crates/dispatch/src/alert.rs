//! 系统提醒：信源异常与恢复、渠道过期与额度用完。同一件事 6 小时内只提醒一次。

use plugin_sdk::contracts::channel::{ChannelInfo, ChannelState};
use plugin_sdk::contracts::events::{ChannelStatus, SourceFailed, SourceRecovered};
use plugin_sdk::contracts::ServiceError;
use time::Duration;

use crate::outbox::NewOutbox;
use crate::render::render_alert;
use crate::{select, Dispatch};

const DEDUP: Duration = Duration::hours(6);
/// 提醒里带的错误说明最多这么多字符。
const ERROR_CHARS: usize = 100;

impl Dispatch {
    /// 这件事现在该不该提醒；该提醒时顺便记下时间。
    fn alert_due(&self, key: &str) -> bool {
        let now = self.now();
        let mut map = self.alerts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = map.get(key) {
            if now - *last < DEDUP {
                return false;
            }
        }
        map.insert(key.to_string(), now);
        true
    }

    /// 给每个目标渠道各排一条提醒；没有目标时留一条转人工的记录。
    async fn queue_alert(&self, targets: &[&ChannelInfo], text: &str) -> Result<(), ServiceError> {
        if targets.is_empty() {
            let mut n = NewOutbox::alert(String::new(), text.to_string());
            n.status = plugin_sdk::contracts::dispatch::OutboxStatus::Manual;
            n.last_error = Some("无可用渠道".into());
            self.insert_outbox(n).await?;
            return Ok(());
        }
        let now_s = self.now_s();
        for t in targets {
            let mut n = NewOutbox::alert(t.id.clone(), render_alert(text, t.max_bytes));
            if t.id == "wechat-ilink" {
                n.status = plugin_sdk::contracts::dispatch::OutboxStatus::Manual;
                n.last_error = Some("系统提醒保留在应用内，不占用微信互动额度".to_string());
            } else {
                n.next_at = Some(now_s.clone());
            }
            self.insert_outbox(n).await?;
        }
        Ok(())
    }

    pub(crate) async fn on_source_failed(&self, p: SourceFailed) -> Result<(), ServiceError> {
        if !self.alert_due(&format!("source.failed:{}", p.source_id)) {
            return Ok(());
        }
        let err: String = p.error.chars().take(ERROR_CHARS).collect();
        let text = format!(
            "⚠️ 信源异常：{} 连续失败 {} 次\n{}",
            p.source_id, p.consecutive_failures, err
        );
        let chans = self.channels().await;
        let infos: Vec<ChannelInfo> = chans.into_iter().map(|c| c.info).collect();
        self.queue_alert(&select::primary_channels(&infos), &text)
            .await
    }

    pub(crate) async fn on_source_recovered(&self, p: SourceRecovered) -> Result<(), ServiceError> {
        if !self.alert_due(&format!("source.recovered:{}", p.source_id)) {
            return Ok(());
        }
        let text = format!("✅ 信源恢复：{}", p.source_id);
        let chans = self.channels().await;
        let infos: Vec<ChannelInfo> = chans.into_iter().map(|c| c.info).collect();
        self.queue_alert(&select::primary_channels(&infos), &text)
            .await
    }

    /// 渠道恢复：待发提前。主渠道过期或额度用完：从其他渠道提醒用户。
    pub(crate) async fn on_channel_status(&self, p: ChannelStatus) -> Result<(), ServiceError> {
        match p.state {
            ChannelState::Ok => self.pull_forward(&p.channel).await,
            ChannelState::Expired | ChannelState::QuotaExhausted => {
                let chans = self.channels().await;
                let infos: Vec<ChannelInfo> = chans.into_iter().map(|c| c.info).collect();
                let this = infos.iter().find(|i| i.id == p.channel);
                if this.is_some_and(|i| i.backup) {
                    return Ok(());
                }
                let state = if p.state == ChannelState::Expired {
                    "expired"
                } else {
                    "quota_exhausted"
                };
                if !self.alert_due(&format!("channel:{}:{state}", p.channel)) {
                    return Ok(());
                }
                let name = this.map_or(p.channel.as_str(), |i| i.name.as_str());
                let text = if p.state == ChannelState::Expired {
                    if p.channel == "wecom-webhook" {
                        format!("⚠️ 渠道链接无效：请核对 {name} 的 Webhook 地址与机器人状态")
                    } else {
                        format!("⚠️ 渠道需要续期：{name} 的凭证已过期，请重新绑定")
                    }
                } else {
                    format!("⚠️ 渠道额度用完：{name} 暂时无法主动推送，请回复消息续期")
                };
                let targets = select::ok_channels(&infos, &p.channel);
                if targets.is_empty() {
                    tracing::info!("渠道 {} 状态异常，但没有其他可用渠道发提醒", p.channel);
                    return Ok(());
                }
                self.queue_alert(&targets, &text).await
            }
            _ => Ok(()),
        }
    }
}

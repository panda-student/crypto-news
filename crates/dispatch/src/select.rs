//! 渠道选择：纯函数，输入渠道信息，输出目标渠道。

use plugin_sdk::contracts::channel::{ChannelInfo, ChannelState};

/// 主渠道：非备用且已绑定；没有时把已绑定的备用渠道当主渠道。
/// 日报、提醒发给它们（不论当前状态，状态不好就在 outbox 里排队）。
pub fn primary_channels(infos: &[ChannelInfo]) -> Vec<&ChannelInfo> {
    let bound = |i: &&ChannelInfo| i.state != ChannelState::Unbound;
    let main: Vec<&ChannelInfo> = infos.iter().filter(|i| !i.backup).filter(bound).collect();
    if main.is_empty() {
        infos.iter().filter(bound).collect()
    } else {
        main
    }
}

/// 快讯目标：状态为 ok 的主渠道；没有则状态为 ok 的备用渠道；都没有则仍排给主渠道排队。
pub fn flash_channels(infos: &[ChannelInfo]) -> Vec<&ChannelInfo> {
    let primary = primary_channels(infos);
    let ok: Vec<&ChannelInfo> = primary
        .iter()
        .copied()
        .filter(|i| i.state == ChannelState::Ok)
        .collect();
    if !ok.is_empty() {
        return ok;
    }
    let backups: Vec<&ChannelInfo> = infos
        .iter()
        .filter(|i| i.backup && i.state == ChannelState::Ok)
        .collect();
    if !backups.is_empty() {
        return backups;
    }
    primary
}

/// 提醒目标：除 `except` 外所有状态为 ok 的渠道（含备用）。
pub fn ok_channels<'a>(infos: &'a [ChannelInfo], except: &str) -> Vec<&'a ChannelInfo> {
    infos
        .iter()
        .filter(|i| i.id != except && i.state == ChannelState::Ok)
        .collect()
}

/// 改走备用渠道：状态为 ok、不是当前渠道的备用渠道。
pub fn failover_channel<'a>(infos: &'a [ChannelInfo], current: &str) -> Option<&'a ChannelInfo> {
    infos
        .iter()
        .find(|i| i.backup && i.state == ChannelState::Ok && i.id != current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::channel::TextFormat;

    fn ch(id: &str, backup: bool, state: ChannelState) -> ChannelInfo {
        ChannelInfo {
            id: id.into(),
            name: id.into(),
            format: TextFormat::Plain,
            max_bytes: 2000,
            backup,
            state,
            quota_left: None,
            last_received_at: None,
            quota_limit: None,
            expires_at: None,
            detail: None,
        }
    }
    fn ids(v: Vec<&ChannelInfo>) -> Vec<&str> {
        v.into_iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn primary_prefers_non_backup_and_skips_unbound() {
        let infos = vec![
            ch("wechat", false, ChannelState::Expired),
            ch("wecom", true, ChannelState::Ok),
            ch("other", false, ChannelState::Unbound),
        ];
        assert_eq!(ids(primary_channels(&infos)), vec!["wechat"]);
    }

    #[test]
    fn backup_acts_as_primary_when_alone() {
        let infos = vec![
            ch("wechat", false, ChannelState::Unbound),
            ch("wecom", true, ChannelState::Ok),
        ];
        assert_eq!(ids(primary_channels(&infos)), vec!["wecom"]);
        assert_eq!(ids(flash_channels(&infos)), vec!["wecom"]);
        assert!(primary_channels(&[ch("x", true, ChannelState::Unbound)]).is_empty());
    }

    #[test]
    fn flash_uses_ok_primary_else_ok_backup_else_queues_on_primary() {
        let infos = vec![
            ch("wechat", false, ChannelState::Ok),
            ch("wecom", true, ChannelState::Ok),
        ];
        assert_eq!(ids(flash_channels(&infos)), vec!["wechat"]);

        let infos = vec![
            ch("wechat", false, ChannelState::QuotaExhausted),
            ch("wecom", true, ChannelState::Ok),
        ];
        assert_eq!(ids(flash_channels(&infos)), vec!["wecom"]);

        let infos = vec![
            ch("wechat", false, ChannelState::Expired),
            ch("wecom", true, ChannelState::Offline),
        ];
        assert_eq!(ids(flash_channels(&infos)), vec!["wechat"]);
    }

    #[test]
    fn alert_and_failover_targets() {
        let infos = vec![
            ch("wechat", false, ChannelState::Expired),
            ch("wecom", true, ChannelState::Ok),
        ];
        assert_eq!(ids(ok_channels(&infos, "wechat")), vec!["wecom"]);
        assert!(ok_channels(&infos, "wecom").is_empty());
        assert_eq!(failover_channel(&infos, "wechat").unwrap().id, "wecom");
        assert!(failover_channel(&infos, "wecom").is_none());
    }
}

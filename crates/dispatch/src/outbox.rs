//! outbox：入队、发送一轮、错误处理。

use std::collections::{HashSet, VecDeque};

use plugin_sdk::contracts::channel::{ChannelInfo, SendRequest};
use plugin_sdk::contracts::dispatch::{OutboxEntry, OutboxKind, OutboxStatus};
use plugin_sdk::contracts::events::PushItem;
use plugin_sdk::contracts::{format_rfc3339, ServiceError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::Duration;

use crate::render::{render_flash, truncate_at_boundary};
use crate::{parse_ids, select, Chan, Dispatch};

/// 连续失败这么多次转人工。
pub(crate) const MAX_ATTEMPTS: i64 = 5;
/// 额度用完、凭证过期且没有备用渠道时，多久后再试（不计次数）。
const WAIT_RETRY: Duration = Duration::minutes(10);
const WECHAT: &str = "wechat-ilink";

/// 枚举转成存储用的字符串。
pub(crate) fn enum_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 退避：30 秒 × 2^(第几次失败−1)，最长 30 分钟。
pub(crate) fn backoff(attempts: i64) -> Duration {
    let exp = (attempts - 1).clamp(0, 10) as u32;
    Duration::seconds((30i64 << exp).min(30 * 60))
}

/// 只留错误码和一句短说明。
pub(crate) fn err_text(e: &ServiceError) -> String {
    format!("[{}] {}", e.code, truncate_at_boundary(&e.message, 200))
}

pub(crate) struct NewOutbox {
    pub user_requested: bool,
    pub kind: OutboxKind,
    pub channel: String,
    pub status: OutboxStatus,
    pub batch: i64,
    pub text: String,
    /// 快讯保留原始条目，改走备用渠道时重新渲染
    pub items: Option<Vec<PushItem>>,
    pub item_ids: Vec<i64>,
    pub next_at: Option<String>,
    pub last_error: Option<String>,
}

impl NewOutbox {
    /// 提醒、测试消息。
    pub(crate) fn alert(channel: String, text: String) -> NewOutbox {
        NewOutbox {
            user_requested: false,
            kind: OutboxKind::Alert,
            channel,
            status: OutboxStatus::Pending,
            batch: 1,
            text,
            items: None,
            item_ids: Vec::new(),
            next_at: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OutRow {
    #[serde(default)]
    pub user_requested: i64,
    pub id: i64,
    pub kind: OutboxKind,
    pub channel: String,
    pub status: OutboxStatus,
    #[serde(default)]
    pub attempts: Option<i64>,
    #[serde(default)]
    pub batch: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub items: Option<String>,
    #[serde(default)]
    pub item_ids: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub next_at: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
}

impl OutRow {
    pub(crate) fn entry(&self) -> OutboxEntry {
        OutboxEntry {
            id: self.id,
            kind: self.kind,
            channel: self.channel.clone(),
            status: self.status,
            attempts: self.attempts.unwrap_or(0),
            batch: self.batch.unwrap_or(1),
            text: self.text.clone().unwrap_or_default(),
            item_ids: parse_ids(self.item_ids.as_deref()),
            last_error: self.last_error.clone(),
            created_at: self.created_at.clone().unwrap_or_default(),
            next_at: self.next_at.clone(),
            sent_at: self.sent_at.clone(),
        }
    }
}

impl Dispatch {
    pub(crate) async fn insert_outbox(&self, n: NewOutbox) -> Result<i64, ServiceError> {
        let items = match &n.items {
            Some(v) => Value::String(serde_json::to_string(v)?),
            None => Value::Null,
        };
        let r = self
            .store
            .execute(
                "INSERT INTO outbox(kind, channel, status, attempts, batch, text, items, item_ids, \
                 last_error, created_at, next_at, sent_at, user_requested) VALUES(?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
                vec![
                    json!(enum_str(&n.kind)),
                    json!(n.channel),
                    json!(enum_str(&n.status)),
                    json!(n.batch),
                    json!(n.text),
                    items,
                    json!(serde_json::to_string(&n.item_ids)?),
                    json!(n.last_error),
                    json!(self.now_s()),
                    json!(n.next_at),
                    json!(i64::from(n.user_requested)),
                ],
            )
            .await?;
        Ok(r.last_insert_id)
    }

    pub(crate) async fn get_outbox(&self, id: i64) -> Result<Option<OutRow>, ServiceError> {
        let mut v: Vec<OutRow> = self
            .store
            .query_as("SELECT * FROM outbox WHERE id = ?", vec![json!(id)])
            .await?;
        Ok(v.pop())
    }

    /// 发送一轮：到点的待发任务按 id 顺序依次发送；某渠道失败后，本轮跳过它剩下的任务。
    pub(crate) async fn send_round(&self) -> Result<(), ServiceError> {
        let _guard = self.send_lock.lock().await;
        if self.cfg.focus_mode {
            self.store.execute("UPDATE outbox SET status='dropped',next_at=NULL,last_error='重点推送模式不补发旧日报' WHERE status='pending' AND kind='digest' AND user_requested=0",vec![]).await?;
            self.flush_focus().await?;
        }
        self.store.execute("UPDATE outbox SET status='manual',next_at=NULL,last_error='系统提醒保留在应用内，不占用微信互动额度' WHERE status='pending' AND channel=? AND kind='alert' AND user_requested=0",vec![json!(WECHAT)]).await?;
        self.store.execute("UPDATE outbox SET status='dropped',next_at=NULL,last_error='微信自动推送仅发送紧急事件，不补发日报' WHERE status='pending' AND channel=? AND kind='digest' AND user_requested=0",vec![json!(WECHAT)]).await?;
        let now_s = self.now_s();
        let mut rows: Vec<OutRow> = self
            .store
            .query_as(
                "SELECT * FROM outbox WHERE status = 'pending' AND channel <> '' \
                 AND (next_at IS NULL OR next_at <= ?) ORDER BY id",
                vec![json!(now_s)],
            )
            .await?;
        // 新紧急事件优先，不能让旧的一般通知挡在前面。
        rows.sort_by_key(|row| {
            let urgent = row.kind == OutboxKind::Flash
                && row
                    .items
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<Vec<PushItem>>(value).ok())
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item.level == plugin_sdk::contracts::screen::Level::Urgent)
                    });
            (!urgent, row.id)
        });
        if rows.is_empty() {
            return Ok(());
        }
        let chans = self.channels().await;
        let infos: Vec<ChannelInfo> = chans.iter().map(|c| c.info.clone()).collect();
        let mut blocked: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(OutRow, bool)> = rows.into_iter().map(|r| (r, false)).collect();
        while let Some((mut row, failed_over)) = queue.pop_front() {
            if blocked.contains(&row.channel) {
                continue;
            }
            let Some(chan) = chans.iter().find(|c| c.info.id == row.channel) else {
                let e = ServiceError::unavailable("渠道不可用");
                self.fail_attempt(&row, &e).await?;
                blocked.insert(row.channel.clone());
                continue;
            };
            let urgent_only = row.channel == WECHAT && row.user_requested == 0;
            if row.kind == OutboxKind::Flash && (self.cfg.focus_mode || urgent_only) {
                let original: Vec<PushItem> = row
                    .items
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let mut valid = if self.cfg.focus_mode {
                    self.fresh_flash(&original).await?
                } else {
                    original.clone()
                };
                if urgent_only {
                    use plugin_sdk::contracts::{
                        focus,
                        screen::{Level, ScreeningClient},
                    };
                    let screens = ScreeningClient(self.ctx.service("screening"))
                        .get(&valid.iter().map(|i| i.item_id).collect::<Vec<_>>())
                        .await?;
                    valid.retain(|item| {
                        item.level == Level::Urgent
                            && screens.iter().any(|s| {
                                s.item_id == item.item_id
                                    && s.level == Some(Level::Urgent)
                                    && focus::in_window(
                                        Level::Urgent,
                                        s.direction,
                                        item.published_at.as_deref(),
                                        self.now(),
                                    )
                            })
                    });
                }
                if valid.is_empty() {
                    self.store.execute("UPDATE outbox SET status='dropped',last_error='重点消息已过时或不再符合范围' WHERE id=?",vec![json!(row.id)]).await?;
                    continue;
                }
                if valid.len() != original.len() {
                    let text = render_flash(&valid, chan.info.format, chan.info.max_bytes)
                        .into_iter()
                        .map(|b| b.text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let items = serde_json::to_string(&valid)?;
                    let ids = serde_json::to_string(
                        &valid.iter().map(|i| i.item_id).collect::<Vec<_>>(),
                    )?;
                    self.store
                        .execute(
                            "UPDATE outbox SET text=?,items=?,item_ids=? WHERE id=?",
                            vec![json!(text), json!(items), json!(ids), json!(row.id)],
                        )
                        .await?;
                    row.text = Some(text);
                    row.items = Some(items);
                    row.item_ids = Some(ids);
                }
            }
            let req = SendRequest {
                text: row.text.clone().unwrap_or_default(),
                reply_to: None,
            };
            let err = match chan.client.send(&req).await {
                Ok(_) => {
                    self.mark_sent(row.id).await?;
                    continue;
                }
                Err(e) => e,
            };
            match err.code {
                ServiceError::INVALID_PARAMS => {
                    self.set_manual(row.id, &err_text(&err)).await?;
                }
                ServiceError::CREDENTIAL_EXPIRED | ServiceError::QUOTA_EXHAUSTED => {
                    let target = if row.kind == OutboxKind::Flash && !failed_over {
                        select::failover_channel(&infos, &row.channel)
                            .and_then(|i| chans.iter().find(|c| c.info.id == i.id))
                    } else {
                        None
                    };
                    match target {
                        Some(t) => {
                            let moved = self.reroute(&row, t, &err).await?;
                            queue.push_front((moved, true));
                        }
                        None => {
                            self.wait_retry(&row, &err).await?;
                            blocked.insert(row.channel.clone());
                        }
                    }
                }
                _ => {
                    self.fail_attempt(&row, &err).await?;
                    blocked.insert(row.channel.clone());
                }
            }
        }
        Ok(())
    }

    async fn mark_sent(&self, id: i64) -> Result<(), ServiceError> {
        self.store
            .execute(
                "UPDATE outbox SET status = 'sent', sent_at = ?, next_at = NULL, last_error = NULL \
                 WHERE id = ?",
                vec![json!(self.now_s()), json!(id)],
            )
            .await?;
        Ok(())
    }

    async fn set_manual(&self, id: i64, last_error: &str) -> Result<(), ServiceError> {
        self.store
            .execute(
                "UPDATE outbox SET status = 'manual', next_at = NULL, last_error = ? WHERE id = ?",
                vec![json!(last_error), json!(id)],
            )
            .await?;
        Ok(())
    }

    /// 计一次失败：到 5 次转人工，否则退避。
    async fn fail_attempt(&self, row: &OutRow, err: &ServiceError) -> Result<(), ServiceError> {
        let attempts = row.attempts.unwrap_or(0) + 1;
        if attempts >= MAX_ATTEMPTS {
            self.store
                .execute(
                    "UPDATE outbox SET status = 'manual', attempts = ?, next_at = NULL, \
                     last_error = ? WHERE id = ?",
                    vec![json!(attempts), json!(err_text(err)), json!(row.id)],
                )
                .await?;
        } else {
            let next = format_rfc3339(self.now() + backoff(attempts));
            self.store
                .execute(
                    "UPDATE outbox SET attempts = ?, next_at = ?, last_error = ? WHERE id = ?",
                    vec![
                        json!(attempts),
                        json!(next),
                        json!(err_text(err)),
                        json!(row.id),
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// 凭证过期、额度用完：不计次数，10 分钟后再试（渠道恢复时会被提前）。
    async fn wait_retry(&self, row: &OutRow, err: &ServiceError) -> Result<(), ServiceError> {
        let next = format_rfc3339(self.now() + WAIT_RETRY);
        self.store
            .execute(
                "UPDATE outbox SET next_at = ?, last_error = ? WHERE id = ?",
                vec![json!(next), json!(err_text(err)), json!(row.id)],
            )
            .await?;
        Ok(())
    }

    /// 快讯改走备用渠道：按备用渠道的格式和字节上限重新渲染。
    /// 本行承接第一批，多出的批次作为新任务入队。
    async fn reroute(
        &self,
        row: &OutRow,
        target: &Chan,
        err: &ServiceError,
    ) -> Result<OutRow, ServiceError> {
        let items: Vec<PushItem> = row
            .items
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let now_s = self.now_s();
        let mut moved = row.clone();
        moved.channel = target.info.id.clone();
        moved.next_at = Some(now_s.clone());
        moved.last_error = Some(err_text(err));
        let batches = if items.is_empty() {
            Vec::new()
        } else {
            render_flash(&items, target.info.format, target.info.max_bytes)
        };
        if let Some(first) = batches.first() {
            let part = &items[first.items.clone()];
            moved.text = Some(first.text.clone());
            moved.batch = Some(1);
            moved.items = Some(serde_json::to_string(part)?);
            moved.item_ids = Some(serde_json::to_string(
                &part.iter().map(|i| i.item_id).collect::<Vec<_>>(),
            )?);
        }
        self.store
            .execute(
                "UPDATE outbox SET channel = ?, text = ?, batch = ?, items = ?, item_ids = ?, \
                 last_error = ?, next_at = ? WHERE id = ?",
                vec![
                    json!(moved.channel),
                    json!(moved.text),
                    json!(moved.batch),
                    json!(moved.items),
                    json!(moved.item_ids),
                    json!(moved.last_error),
                    json!(now_s),
                    json!(row.id),
                ],
            )
            .await?;
        for (i, b) in batches.iter().enumerate().skip(1) {
            let part = &items[b.items.clone()];
            self.insert_outbox(NewOutbox {
                user_requested: false,
                kind: OutboxKind::Flash,
                channel: target.info.id.clone(),
                status: OutboxStatus::Pending,
                batch: i as i64 + 1,
                text: b.text.clone(),
                items: Some(part.to_vec()),
                item_ids: part.iter().map(|i| i.item_id).collect(),
                next_at: Some(now_s.clone()),
                last_error: None,
            })
            .await?;
        }
        Ok(moved)
    }

    /// 渠道恢复：这个渠道上待发的任务提前到现在。
    pub(crate) async fn pull_forward(&self, channel: &str) -> Result<(), ServiceError> {
        self.store
            .execute(
                "UPDATE outbox SET next_at = ? WHERE channel = ? AND status = 'pending' AND (channel <> 'wechat-ilink' OR kind = 'flash' OR user_requested=1)",
                vec![json!(self.now_s()), json!(channel)],
            )
            .await?;
        Ok(())
    }
}

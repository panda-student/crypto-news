//! `dispatch` 服务 1.0 的方法。

use plugin_sdk::contracts::dispatch::{
    DigestList, DigestRecord, OutboxList, OutboxQuery, OutboxStatus, QueueCounts, QueueSummary,
};
use plugin_sdk::contracts::{parse_params, to_result, Caller, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::outbox::{enum_str, NewOutbox, OutRow};
use crate::{parse_ids, Dispatch};

/// 只有这个插件能调用 retry 和 test_send。
const WEB: &str = "web";

#[derive(Debug, Deserialize)]
struct DigestRow {
    id: i64,
    date: Option<String>,
    slot: Option<String>,
    item_count: Option<i64>,
    important_count: Option<i64>,
    outbox_ids: Option<String>,
    created_at: Option<String>,
}

fn require_web(caller: &Caller, method: &str) -> Result<(), ServiceError> {
    if caller.plugin == WEB {
        Ok(())
    } else {
        Err(ServiceError::forbidden(format!(
            "只有 web 能调用 dispatch.{method}"
        )))
    }
}

impl Dispatch {
    pub(crate) async fn handle(
        &self,
        caller: Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        match method {
            "focus_settings" => {
                require_web(&caller, method)?;
                to_result(&self.focus_settings().await?)
            }
            "save_focus_settings" => {
                require_web(&caller, method)?;
                let p = plugin_sdk::contracts::parse_params(params)?;
                to_result(&self.save_focus_settings(p).await?)
            }
            "queue_summary" => to_result(&self.queue_summary().await?),
            "outbox" => {
                let q: OutboxQuery = parse_or_default(params)?;
                to_result(&OutboxList {
                    entries: self.list_outbox(&q).await?,
                })
            }
            "digests" => {
                #[derive(Deserialize, Default)]
                struct P {
                    #[serde(default)]
                    limit: Option<i64>,
                }
                let p: P = parse_or_default(params)?;
                let limit = p.limit.unwrap_or(30).clamp(1, 365);
                to_result(&DigestList {
                    digests: self.list_digests(limit).await?,
                })
            }
            "preview_digest" => to_result(&self.preview_digest().await?),
            "retry" => {
                require_web(&caller, method)?;
                #[derive(Deserialize)]
                struct P {
                    id: i64,
                }
                let p: P = parse_params(params)?;
                to_result(&self.retry(p.id).await?)
            }
            "test_send" => {
                require_web(&caller, method)?;
                #[derive(Deserialize)]
                struct P {
                    channel: String,
                }
                let p: P = parse_params(params)?;
                to_result(&self.test_send(&p.channel).await?)
            }
            other => Err(ServiceError::method_not_found(other)),
        }
    }

    async fn queue_summary(&self) -> Result<QueueSummary, ServiceError> {
        let now = self.now_s();
        let channels: Vec<QueueCounts> = self.store.query_as(
            "SELECT channel, SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END) AS pending,
             SUM(CASE WHEN status='pending' AND (next_at IS NULL OR next_at<=?) THEN 1 ELSE 0 END) AS ready,
             SUM(CASE WHEN status='pending' AND next_at>? THEN 1 ELSE 0 END) AS delayed,
             SUM(CASE WHEN status='manual' THEN 1 ELSE 0 END) AS manual
             FROM outbox WHERE status IN ('pending','manual') GROUP BY channel ORDER BY channel",
            vec![json!(now),json!(now)]
        ).await?;
        let total = channels
            .iter()
            .fold(QueueCounts::default(), |mut total, c| {
                total.pending += c.pending;
                total.ready += c.ready;
                total.delayed += c.delayed;
                total.manual += c.manual;
                total
            });
        let events = self
            .store
            .query("SELECT COUNT(*) AS n FROM focus_pending", vec![])
            .await?;
        Ok(QueueSummary {
            total,
            channels,
            merging_events: events[0]["n"].as_i64().unwrap_or(0),
        })
    }

    async fn list_outbox(
        &self,
        q: &OutboxQuery,
    ) -> Result<Vec<plugin_sdk::contracts::dispatch::OutboxEntry>, ServiceError> {
        let mut sql = String::from("SELECT * FROM outbox WHERE 1 = 1");
        let mut params: Vec<Value> = Vec::new();
        if let Some(s) = q.status {
            sql.push_str(" AND status = ?");
            params.push(json!(enum_str(&s)));
        }
        if let Some(k) = q.kind {
            sql.push_str(" AND kind = ?");
            params.push(json!(enum_str(&k)));
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        params.push(json!(q.limit.unwrap_or(50).clamp(1, 500)));
        let rows: Vec<OutRow> = self.store.query_as(&sql, params).await?;
        Ok(rows.iter().map(OutRow::entry).collect())
    }

    async fn list_digests(&self, limit: i64) -> Result<Vec<DigestRecord>, ServiceError> {
        let rows: Vec<DigestRow> = self
            .store
            .query_as(
                "SELECT * FROM digests ORDER BY id DESC LIMIT ?",
                vec![json!(limit)],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| DigestRecord {
                id: r.id,
                date: r.date.unwrap_or_default(),
                slot: r.slot.unwrap_or_default(),
                item_count: r.item_count.unwrap_or(0),
                important_count: r.important_count.unwrap_or(0),
                outbox_ids: parse_ids(r.outbox_ids.as_deref()),
                created_at: r.created_at.unwrap_or_default(),
            })
            .collect())
    }

    /// 转人工或丢弃的任务重新排队，次数清零。
    async fn retry(
        &self,
        id: i64,
    ) -> Result<plugin_sdk::contracts::dispatch::OutboxEntry, ServiceError> {
        let Some(row) = self.get_outbox(id).await? else {
            return Err(ServiceError::invalid_params(format!("任务 {id} 不存在")));
        };
        if !matches!(row.status, OutboxStatus::Manual | OutboxStatus::Dropped) {
            return Err(ServiceError::invalid_params(format!(
                "任务 {id} 的状态是 {}，不能重试",
                enum_str(&row.status)
            )));
        }
        self.store
            .execute(
                "UPDATE outbox SET status = 'pending', attempts = 0, next_at = ?, user_requested=1 WHERE id = ?",
                vec![json!(self.now_s()), json!(id)],
            )
            .await?;
        let row = self
            .get_outbox(id)
            .await?
            .ok_or_else(|| ServiceError::internal("任务消失了"))?;
        Ok(row.entry())
    }

    /// 经 outbox 向指定渠道发一条测试消息。
    async fn test_send(
        &self,
        channel: &str,
    ) -> Result<plugin_sdk::contracts::dispatch::OutboxEntry, ServiceError> {
        let chans = self.channels().await;
        let Some(c) = chans.iter().find(|c| c.info.id == channel) else {
            return Err(ServiceError::invalid_params(format!(
                "渠道 {channel} 不存在"
            )));
        };
        let text = crate::render::render_alert(
            &format!("✅ 测试消息：{} 推送正常（{}）", c.info.name, self.now_s()),
            c.info.max_bytes,
        );
        let mut n = NewOutbox::alert(c.info.id.clone(), text);
        n.user_requested = true;
        n.next_at = Some(self.now_s());
        let id = self.insert_outbox(n).await?;
        let row = self
            .get_outbox(id)
            .await?
            .ok_or_else(|| ServiceError::internal("任务消失了"))?;
        Ok(row.entry())
    }
}

/// 参数缺省（null）时用默认值。
fn parse_or_default<T: serde::de::DeserializeOwned + Default>(
    params: Value,
) -> Result<T, ServiceError> {
    if params.is_null() {
        Ok(T::default())
    } else {
        parse_params(params)
    }
}

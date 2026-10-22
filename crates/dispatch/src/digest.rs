//! 日报：素材累积、定时生成、补发、预览。

use plugin_sdk::contracts::channel::{ChannelInfo, TextFormat};
use plugin_sdk::contracts::dispatch::{DigestPreview, OutboxKind, OutboxStatus};
use plugin_sdk::contracts::events::ItemScreened;
use plugin_sdk::contracts::screen::Level;
use plugin_sdk::contracts::ServiceError;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::outbox::NewOutbox;
use crate::render::{self, Entry};
use crate::{clock, select, Dispatch};

/// 没有对应格式的渠道时，预览用的字节上限。
const PREVIEW_MARKDOWN_MAX: usize = 4000;
const PREVIEW_PLAIN_MAX: usize = 2000;

#[derive(Debug, Deserialize)]
struct MaterialRow {
    item_id: i64,
    title: Option<String>,
    url: Option<String>,
    level: Option<String>,
    impact: Option<String>,
    backfill: Option<i64>,
}

/// 未进日报的素材：按级别、再按消息 ID 排序。
struct Material {
    ids: Vec<i64>,
    entries: Vec<Entry>,
}

impl Dispatch {
    /// `item.screened`：记入日报素材。重复事件（含人工改级）更新内容，保留已有的 digest_id。
    pub(crate) async fn on_item_screened(&self, p: ItemScreened) -> Result<(), ServiceError> {
        self.store
            .execute(
                "INSERT INTO digest_items(item_id, source_id, title, url, level, impact, reason, \
                 published_at, backfill, screened_at, digest_id) \
                 VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL) \
                 ON CONFLICT(item_id) DO UPDATE SET source_id = excluded.source_id, \
                 title = excluded.title, url = excluded.url, level = excluded.level, \
                 impact = excluded.impact, reason = excluded.reason, \
                 published_at = excluded.published_at, backfill = excluded.backfill, \
                 screened_at = excluded.screened_at",
                vec![
                    json!(p.item_id),
                    json!(p.source_id),
                    json!(p.title),
                    json!(p.url),
                    json!(p.level.as_str()),
                    json!(p.impact),
                    json!(p.reason),
                    json!(p.published_at),
                    json!(p.backfill as i64),
                    json!(self.now_s()),
                ],
            )
            .await?;
        Ok(())
    }

    async fn material(&self) -> Result<Material, ServiceError> {
        let rows: Vec<MaterialRow> = self
            .store
            .query_as(
                "SELECT item_id, title, url, level, impact, backfill FROM digest_items \
                 WHERE digest_id IS NULL ORDER BY item_id",
                vec![],
            )
            .await?;
        let mut rows: Vec<(Level, MaterialRow)> = rows
            .into_iter()
            .map(|r| {
                (
                    r.level
                        .as_deref()
                        .and_then(Level::parse)
                        .unwrap_or(Level::Normal),
                    r,
                )
            })
            .collect();
        // 稳定排序：同级别保持消息 ID 顺序
        rows.sort_by_key(|(l, _)| *l);
        let mut ids = Vec::new();
        let mut entries = Vec::new();
        for (level, r) in rows {
            ids.push(r.item_id);
            entries.push(Entry {
                level,
                title: r.title.unwrap_or_default(),
                url: r.url.unwrap_or_default(),
                impact: r.impact.filter(|s| !s.trim().is_empty()),
                backfill: r.backfill.unwrap_or(0) != 0,
            });
        }
        Ok(Material { ids, entries })
    }

    /// 生成今天的日报。`force` 为真（`daily-report` 定时事件）时不看时间；
    /// 否则（`minute` 事件、启动检查）只在已过今天的日报时刻后补发。
    /// 同一个"日期 + 时段"只记账一次，重复调用不会重复生成。返回本次是否生成了。
    pub(crate) async fn maybe_generate(&self, force: bool) -> Result<bool, ServiceError> {
        if !self.cfg.digest_enabled || self.cfg.focus_mode {
            return Ok(false);
        }
        let _guard = self.digest_lock.lock().await;
        let now = self.now();
        if !force && !clock::past_digest_time(now, self.cfg.offset, self.cfg.slot) {
            return Ok(false);
        }
        let date = clock::local_date(now, self.cfg.offset);
        let slot = self.cfg.slot_text.clone();
        let exists = self
            .store
            .query(
                "SELECT id FROM digests WHERE date = ? AND slot = ?",
                vec![json!(date), json!(slot)],
            )
            .await?;
        if !exists.is_empty() {
            return Ok(false);
        }
        let chans = self.channels().await;
        let infos: Vec<ChannelInfo> = chans.iter().map(|c| c.info.clone()).collect();
        let targets = select::primary_channels(&infos);
        if targets.is_empty() {
            tracing::info!("没有可用渠道，日报稍后再试");
            return Ok(false);
        }
        let mat = self.material().await?;
        let (total, important, _, _) = render::digest_stats(&mat.entries);
        // 先入队，再记账：中途失败时宁可下次重复生成，也不丢日报。
        let now_s = self.now_s();
        let mut outbox_ids = Vec::new();
        for t in targets {
            let parts = render::render_digest(
                &date,
                &mat.entries,
                t.format,
                t.max_bytes,
                &self.cfg.renew_hint,
            );
            for (i, text) in parts.into_iter().enumerate() {
                let id = self
                    .insert_outbox(NewOutbox {
                        user_requested: false,
                        kind: OutboxKind::Digest,
                        channel: t.id.clone(),
                        status: OutboxStatus::Pending,
                        batch: i as i64 + 1,
                        text,
                        items: None,
                        item_ids: mat.ids.clone(),
                        next_at: Some(now_s.clone()),
                        last_error: None,
                    })
                    .await?;
                outbox_ids.push(id);
            }
        }
        let inserted = self
            .store
            .execute(
                "INSERT OR IGNORE INTO digests(date, slot, item_count, important_count, outbox_ids, \
                 created_at) VALUES(?, ?, ?, ?, ?, ?)",
                vec![
                    json!(date),
                    json!(slot),
                    json!(total as i64),
                    json!(important as i64),
                    json!(serde_json::to_string(&outbox_ids)?),
                    json!(self.now_s()),
                ],
            )
            .await?;
        if inserted.rows_affected == 0 {
            return Ok(false);
        }
        let digest_id = inserted.last_insert_id;
        if !mat.ids.is_empty() {
            let marks = vec!["?"; mat.ids.len()].join(", ");
            let mut params: Vec<Value> = vec![json!(digest_id)];
            params.extend(mat.ids.iter().map(|i| json!(i)));
            self.store
                .execute(
                    &format!(
                        "UPDATE digest_items SET digest_id = ? WHERE digest_id IS NULL \
                         AND item_id IN ({marks})"
                    ),
                    params,
                )
                .await?;
        }
        Ok(true)
    }

    /// 按当前素材渲染日报预览：不入队、不记账、不改数据库。
    pub(crate) async fn preview_digest(&self) -> Result<DigestPreview, ServiceError> {
        let mat = self.material().await?;
        let chans = self.channels().await;
        let max_of = |fmt: TextFormat, default: usize| {
            chans
                .iter()
                .find(|c| c.info.format == fmt)
                .map_or(default, |c| c.info.max_bytes)
        };
        let date = clock::local_date(self.now(), self.cfg.offset);
        let render_as = |fmt, default| {
            render::render_digest(
                &date,
                &mat.entries,
                fmt,
                max_of(fmt, default),
                &self.cfg.renew_hint,
            )
        };
        let (total, _, _, _) = render::digest_stats(&mat.entries);
        Ok(DigestPreview {
            markdown: render_as(TextFormat::Markdown, PREVIEW_MARKDOWN_MAX),
            plain: render_as(TextFormat::Plain, PREVIEW_PLAIN_MAX),
            item_count: total as i64,
        })
    }
}

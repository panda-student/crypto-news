//! screen 插件：订阅 `item.collected`，按规则手册用小模型批量分四级，
//! 同次初筛返回紧急和重要级简析，发 `item.screened` 与 `push.pending`；提供 `screening` 服务。

mod round;
mod rules;
mod service;

use std::sync::Arc;
use std::time::Duration;

use plugin_sdk::contracts::events::{ItemCollected, ITEM_COLLECTED};
use plugin_sdk::contracts::screen::SERVICE;
use plugin_sdk::contracts::{now_rfc3339, parse_rfc3339};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Notify;

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "
CREATE TABLE screenings (
    item_id INTEGER PRIMARY KEY,
    source_id TEXT,
    url TEXT,
    title TEXT,
    published_at TEXT,
    level TEXT,
    score REAL,
    reason TEXT,
    impact TEXT,
    status TEXT NOT NULL,
    method TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    backfill INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    screened_at TEXT,
    next_at TEXT
);
CREATE INDEX idx_screenings_status_next ON screenings (status, next_at);
CREATE INDEX idx_screenings_screened_at ON screenings (screened_at);
CREATE INDEX idx_screenings_level ON screenings (level);
";

const MIGRATION_2: &str = "
ALTER TABLE screenings ADD COLUMN assets TEXT;
ALTER TABLE screenings ADD COLUMN direction TEXT;
";

const MIGRATION_3: &str = "
ALTER TABLE screenings ADD COLUMN title_zh TEXT;
ALTER TABLE screenings ADD COLUMN summary_zh TEXT;
";

const MIGRATION_4: &str = "
ALTER TABLE screenings ADD COLUMN zh_attempts INTEGER NOT NULL DEFAULT 0;
";

const MIGRATION_5: &str = "
ALTER TABLE screenings ADD COLUMN analyzed INTEGER NOT NULL DEFAULT 0;
";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    pub batch_size: usize,
    pub batch_interval_secs: u64,
    pub backfill_hours: i64,
    pub max_attempts: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            batch_size: 10,
            batch_interval_secs: 20,
            backfill_hours: 2,
            max_attempts: 3,
        }
    }
}

pub(crate) struct Inner {
    pub ctx: Context,
    pub cfg: Config,
    /// 待筛数量达到批大小时唤醒初筛循环
    pub wake: Notify,
}

#[derive(Default)]
pub struct ScreenPlugin;

impl Plugin for ScreenPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let inner = init(&ctx).await?;
            ctx.spawn("loop", run_loop(inner));
            Ok(())
        })
    }
}

/// 建表、登记服务、订阅事件；不启动循环（测试直接调 `round::run_round`）。
pub(crate) async fn init(ctx: &Context) -> Result<Arc<Inner>, PluginError> {
    let cfg: Config = if ctx.config().is_null() {
        Config::default()
    } else {
        ctx.config_as()?
    };
    let cfg = Config {
        batch_size: cfg.batch_size.clamp(1, 10),
        batch_interval_secs: cfg.batch_interval_secs.max(1),
        max_attempts: cfg.max_attempts.max(1),
        ..cfg
    };
    ctx.store().migrate(1, SCHEMA).await?;
    ctx.store().migrate(2, MIGRATION_2).await?;
    ctx.store().migrate(3, MIGRATION_3).await?;
    ctx.store().migrate(4, MIGRATION_4).await?;
    ctx.store().migrate(5, MIGRATION_5).await?;
    ctx.store()
        .migrate(6, "ALTER TABLE screenings ADD COLUMN focus TEXT;")
        .await?;
    // 保留迁移编号，升级不再主动重置任何已完成的历史分析。
    ctx.store().migrate(7, "SELECT 1;").await?;
    ctx.store()
        .migrate(
            8,
            "ALTER TABLE screenings ADD COLUMN input_hash TEXT;
        CREATE INDEX idx_screenings_input_hash ON screenings(input_hash);",
        )
        .await?;
    let inner = Arc::new(Inner {
        ctx: ctx.clone(),
        cfg,
        wake: Notify::new(),
    });

    let i = inner.clone();
    ctx.provide_fn(SERVICE, move |caller, method, params| {
        let i = i.clone();
        async move { service::call(&i, caller, &method, params).await }
    })?;
    let i = inner.clone();
    ctx.subscribe(ITEM_COLLECTED, move |ev| {
        let i = i.clone();
        async move {
            let item: ItemCollected = ev.parse()?;
            on_collected(&i, item).await
        }
    })?;
    Ok(inner)
}

async fn run_loop(inner: Arc<Inner>) -> Result<(), PluginError> {
    let interval = Duration::from_secs(inner.cfg.batch_interval_secs);
    loop {
        let n = round::run_round(&inner).await;
        if n >= inner.cfg.batch_size {
            continue;
        }
        let _ = tokio::time::timeout(interval, inner.wake.notified()).await;
    }
}

/// 新消息入库为 pending 行；重复事件忽略。
async fn on_collected(inner: &Inner, item: ItemCollected) -> Result<(), PluginError> {
    let now = time::OffsetDateTime::now_utc();
    let cutoff = now - time::Duration::hours(inner.cfg.backfill_hours);
    let backfill = item
        .published_at
        .as_deref()
        .and_then(parse_rfc3339)
        .is_some_and(|t| t < cutoff);
    let store = inner.ctx.store();
    store
        .execute(
            "INSERT OR IGNORE INTO screenings \
             (item_id, source_id, url, title, published_at, status, attempts, backfill, created_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', 0, ?, ?)",
            vec![
                json!(item.item_id),
                json!(item.source_id),
                json!(item.url),
                json!(item.title),
                json!(item.published_at),
                json!(i64::from(backfill)),
                json!(now_rfc3339()),
            ],
        )
        .await?;
    let rows = store
        .query(
            "SELECT COUNT(*) AS n FROM screenings WHERE status = 'pending'",
            vec![],
        )
        .await?;
    let n = rows
        .first()
        .and_then(|r| r.get("n"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if n >= inner.cfg.batch_size as u64 {
        inner.wake.notify_one();
    }
    Ok(())
}

#[cfg(test)]
mod tests;

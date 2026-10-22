//! collect 插件：定期调用所有 `source` 提供者抓取，去重入库，发 `item.collected`，
//! 维护信源状态并在连续失败时报警；提供 `source-status` 与 `item-query` 服务。

mod query;
mod round;

use std::sync::Arc;
use std::time::Duration;

use plugin_sdk::contracts::source::{ITEM_QUERY_SERVICE, STATUS_SERVICE};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError};
use serde::Deserialize;
use tokio::sync::Notify;

pub use round::is_due;

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    interval_secs INTEGER NOT NULL,
    cursor TEXT,
    last_attempt_at TEXT,
    last_success_at TEXT,
    last_error TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    total_items INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE raw_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT NOT NULL,
    dedup_key TEXT NOT NULL,
    external_id TEXT,
    url TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT,
    published_at TEXT,
    collected_at TEXT NOT NULL,
    raw TEXT NOT NULL DEFAULT '',
    UNIQUE (source_id, dedup_key)
);
CREATE INDEX idx_raw_items_collected_at ON raw_items (collected_at);
";

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    tick_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config { tick_secs: 30 }
    }
}

#[derive(Default)]
pub struct CollectPlugin;

impl Plugin for CollectPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = if ctx.config().is_null() {
                Config::default()
            } else {
                ctx.config_as()?
            };
            let tick = Duration::from_secs(cfg.tick_secs.max(1));
            let wake = init(&ctx).await?;
            let c = ctx.clone();
            ctx.spawn("loop", run_loop(c, tick, wake));
            Ok(())
        })
    }
}

async fn run_loop(ctx: Context, tick: Duration, wake: Arc<Notify>) -> Result<(), PluginError> {
    loop {
        round::run_round(&ctx).await;
        // 到点或被 retry 唤醒都立刻再跑一轮
        let _ = tokio::time::timeout(tick, wake.notified()).await;
    }
}

/// 建表并登记服务，返回用来唤醒调度循环的 Notify。
async fn init(ctx: &Context) -> Result<Arc<Notify>, PluginError> {
    ctx.store().migrate(1, SCHEMA).await?;

    let wake = Arc::new(Notify::new());
    let c = ctx.clone();
    let w = wake.clone();
    ctx.provide_fn(STATUS_SERVICE, move |_caller, method, params| {
        let c = c.clone();
        let w = w.clone();
        async move { query::status_call(&c, &w, &method, params).await }
    })?;
    let c = ctx.clone();
    ctx.provide_fn(ITEM_QUERY_SERVICE, move |caller, method, params| {
        let c = c.clone();
        async move { query::item_call(&c, &caller, &method, params).await }
    })?;
    Ok(wake)
}

#[cfg(test)]
mod tests;

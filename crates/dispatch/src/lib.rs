//! 推送调度插件：紧急快讯即时推，日报定时推（休眠错过开机补发），信源和渠道异常时提醒。
//! 所有推送经 outbox 排队，按错误类型重试或改走备用渠道。提供 `dispatch` 1.0。

mod alert;
mod clock;
mod digest;
mod focus;
mod outbox;
mod render;
mod select;
mod service;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plugin_sdk::contracts::channel::{ChannelClient, ChannelInfo};
use plugin_sdk::contracts::dispatch::{OutboxKind, OutboxStatus, SERVICE};
use plugin_sdk::contracts::events::{
    ChannelStatus, ItemScreened, PushBefore, PushItem, PushPending, SourceFailed, SourceRecovered,
    TimerTick, CHANNEL_STATUS, ITEM_SCREENED, PUSH_BEFORE, PUSH_PENDING, SOURCE_FAILED,
    SOURCE_RECOVERED, TIMER_TICK,
};

use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::contracts::{channel, format_rfc3339, parse_rfc3339, BoxFuture, Event, Intercept};
use plugin_sdk::{Context, Plugin, PluginError};
use serde::Deserialize;
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

use clock::QuietHours;
use outbox::NewOutbox;

const SCHEMA: &str = "
CREATE TABLE digest_items(
  item_id INTEGER PRIMARY KEY, source_id TEXT, title TEXT, url TEXT, level TEXT,
  impact TEXT, reason TEXT, published_at TEXT, backfill INTEGER, screened_at TEXT, digest_id INTEGER);
CREATE TABLE digests(
  id INTEGER PRIMARY KEY AUTOINCREMENT, date TEXT, slot TEXT, item_count INTEGER,
  important_count INTEGER, outbox_ids TEXT, created_at TEXT, UNIQUE(date, slot));
CREATE TABLE outbox(
  id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT, channel TEXT, status TEXT,
  attempts INTEGER DEFAULT 0, batch INTEGER DEFAULT 1, text TEXT, items TEXT, item_ids TEXT,
  last_error TEXT, created_at TEXT, next_at TEXT, sent_at TEXT);
CREATE INDEX outbox_status_next ON outbox(status, next_at);
";

/// 发送循环的周期。
const SEND_EVERY: Duration = Duration::from_secs(5);

pub struct DispatchPlugin;

impl Plugin for DispatchPlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let clock: Clock = Arc::new(OffsetDateTime::now_utc);
            let d = Dispatch::setup(ctx.clone(), clock).await?;
            let sender = d.clone();
            ctx.spawn("sender", async move {
                let mut iv = tokio::time::interval(SEND_EVERY);
                loop {
                    iv.tick().await;
                    if let Err(e) = sender.send_round().await {
                        tracing::warn!("发送一轮失败：{e}");
                    }
                }
            });
            // 启动时检查：休眠错过日报时间的，开机补发
            if let Err(e) = d.maybe_generate(false).await {
                tracing::warn!("启动补发日报失败：{e}");
            }
            Ok(())
        })
    }
}

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default = "default_true")]
    focus_mode: bool,
    #[serde(default = "default_offset")]
    utc_offset: String,
    #[serde(default = "default_digest_time")]
    digest_time: String,
    #[serde(default)]
    quiet_hours: String,
    #[serde(default = "default_renew_hint")]
    renew_hint: String,
    #[serde(default = "default_true")]
    push_urgent: bool,
    #[serde(default = "default_true")]
    push_important: bool,
    #[serde(default = "default_true")]
    digest_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_offset() -> String {
    "+08:00".into()
}
fn default_digest_time() -> String {
    "08:00".into()
}
fn default_renew_hint() -> String {
    "💬 回复任意内容，保持微信推送畅通".into()
}

/// 解析后的配置。
pub(crate) struct Settings {
    pub focus_mode: bool,
    pub offset: UtcOffset,
    pub slot: (u8, u8),
    /// 日报时段的文本形式，如 `08:00`，记账用
    pub slot_text: String,
    pub quiet: Option<QuietHours>,
    pub renew_hint: String,
    /// 紧急消息是否立即推送（关闭后只进日报）
    pub push_urgent: bool,
    /// 重要消息是否立即推送（关闭后只进日报）
    pub push_important: bool,
    /// 是否生成并发送定时日报
    pub digest_enabled: bool,
}

impl Settings {
    pub(crate) fn from_config(v: &Value) -> Result<Settings, PluginError> {
        let raw: RawConfig = if v.is_null() {
            serde_json::from_value(serde_json::json!({}))?
        } else {
            serde_json::from_value(v.clone())
                .map_err(|e| PluginError::new(format!("dispatch 配置错误：{e}")))?
        };
        let offset = clock::parse_offset(&raw.utc_offset).map_err(PluginError::new)?;
        let slot = clock::parse_hhmm(&raw.digest_time).map_err(PluginError::new)?;
        let quiet = clock::parse_quiet(&raw.quiet_hours).map_err(PluginError::new)?;
        Ok(Settings {
            focus_mode: raw.focus_mode,
            offset,
            slot,
            slot_text: format!("{:02}:{:02}", slot.0, slot.1),
            quiet,
            renew_hint: raw.renew_hint,
            push_urgent: raw.push_urgent,
            push_important: raw.push_important,
            digest_enabled: raw.digest_enabled,
        })
    }
}

/// 一个可用的渠道：信息 + 调用句柄。
#[derive(Clone)]
pub(crate) struct Chan {
    pub info: ChannelInfo,
    pub client: ChannelClient,
}

pub(crate) struct Dispatch {
    pub ctx: Context,
    pub store: StoreClient,
    pub cfg: Settings,
    clock: Clock,
    /// 提醒去重：`渠道:状态` → 上次提醒时间
    pub alerts: Mutex<HashMap<String, OffsetDateTime>>,
    /// 同一时刻只跑一轮发送
    pub send_lock: tokio::sync::Mutex<()>,
    /// 同一时刻只生成一份日报
    pub digest_lock: tokio::sync::Mutex<()>,
}

impl Dispatch {
    /// 建表、登记订阅、拦截器和服务。
    pub(crate) async fn setup(ctx: Context, clock: Clock) -> Result<Arc<Dispatch>, PluginError> {
        let cfg = Settings::from_config(ctx.config())?;
        let store = ctx.store();
        store.migrate(1, SCHEMA).await?;
        store.migrate(2, focus::SCHEMA).await?;
        store
            .migrate(
                3,
                "ALTER TABLE outbox ADD COLUMN user_requested INTEGER NOT NULL DEFAULT 0;",
            )
            .await?;
        let d = Arc::new(Dispatch {
            store,
            cfg,
            clock,
            alerts: Mutex::new(HashMap::new()),
            send_lock: tokio::sync::Mutex::new(()),
            digest_lock: tokio::sync::Mutex::new(()),
            ctx,
        });
        d.install()?;
        Ok(d)
    }

    fn install(self: &Arc<Self>) -> Result<(), PluginError> {
        let d = self.clone();
        self.ctx.subscribe(ITEM_SCREENED, move |ev| {
            let d = d.clone();
            async move {
                let Some(p) = parse::<ItemScreened>(&ev) else {
                    return Ok(());
                };
                d.on_item_screened(p).await?;
                Ok(())
            }
        })?;

        let d = self.clone();
        self.ctx.subscribe(PUSH_PENDING, move |ev| {
            let d = d.clone();
            async move {
                let Some(p) = parse::<PushPending>(&ev) else {
                    return Ok(());
                };
                d.on_push_pending(p, &ev.trace_id).await
            }
        })?;

        let d = self.clone();
        self.ctx.subscribe(TIMER_TICK, move |ev| {
            let d = d.clone();
            async move {
                let Some(t) = parse::<TimerTick>(&ev) else {
                    return Ok(());
                };
                let r = match t.name.as_str() {
                    "daily-report" => d.maybe_generate(true).await.map(|_| ()),
                    "minute" => d.maybe_generate(false).await.map(|_| ()),
                    _ => Ok(()),
                };
                r.map_err(PluginError::from)
            }
        })?;

        let d = self.clone();
        self.ctx.subscribe(SOURCE_FAILED, move |ev| {
            let d = d.clone();
            async move {
                let Some(p) = parse::<SourceFailed>(&ev) else {
                    return Ok(());
                };
                d.on_source_failed(p).await?;
                Ok(())
            }
        })?;

        let d = self.clone();
        self.ctx.subscribe(SOURCE_RECOVERED, move |ev| {
            let d = d.clone();
            async move {
                let Some(p) = parse::<SourceRecovered>(&ev) else {
                    return Ok(());
                };
                d.on_source_recovered(p).await?;
                Ok(())
            }
        })?;

        let d = self.clone();
        self.ctx.subscribe(CHANNEL_STATUS, move |ev| {
            let d = d.clone();
            async move {
                let Some(p) = parse::<ChannelStatus>(&ev) else {
                    return Ok(());
                };
                d.on_channel_status(p).await?;
                Ok(())
            }
        })?;

        let d = self.clone();
        self.ctx.intercept(PUSH_BEFORE, move |ev| {
            let d = d.clone();
            async move { Ok(d.quiet_intercept(ev)) }
        })?;

        let d = self.clone();
        self.ctx
            .provide_fn(SERVICE, move |caller, method, params| {
                let d = d.clone();
                async move { d.handle(caller, &method, params).await }
            })?;
        Ok(())
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    pub(crate) fn now_s(&self) -> String {
        format_rfc3339(self.now())
    }

    /// 全部在运行的渠道及其信息；`info` 调用失败的跳过。
    pub(crate) async fn channels(&self) -> Vec<Chan> {
        let mut out = Vec::new();
        for (name, svc) in self.ctx.services(channel::SERVICE) {
            let client = ChannelClient(svc);
            match client.info().await {
                Ok(mut info) => {
                    info.id = name;
                    out.push(Chan { info, client });
                }
                Err(e) => tracing::warn!("读取渠道 {name} 信息失败：{e}"),
            }
        }
        out
    }

    /// `push.pending`：经 `push.before` 拦截后，逐渠道渲染并入队。
    async fn on_push_pending(&self, p: PushPending, trace_id: &str) -> Result<(), PluginError> {
        if p.kind != "flash" {
            tracing::debug!("忽略未知推送类型：{}", p.kind);
            return Ok(());
        }
        let items = p
            .items
            .into_iter()
            .filter(|i| {
                self.cfg.focus_mode
                    || match i.level {
                        plugin_sdk::contracts::screen::Level::Urgent => self.cfg.push_urgent,
                        plugin_sdk::contracts::screen::Level::Important => self.cfg.push_important,
                        _ => true,
                    }
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Ok(());
        }
        let before = PushBefore {
            kind: p.kind,
            items,
            defer_until: None,
        };
        let out = self
            .ctx
            .emit_intercept(
                PUSH_BEFORE,
                serde_json::to_value(&before)?,
                Some(trace_id.to_string()),
            )
            .await?;
        let Some(v) = out else {
            tracing::info!("快讯被 push.before 拦截，已丢弃");
            return Ok(());
        };
        let before: PushBefore = match serde_json::from_value(v) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("push.before 载荷无法解析：{e}");
                return Ok(());
            }
        };
        if before.items.is_empty() {
            return Ok(());
        }
        let next_at = before
            .defer_until
            .as_deref()
            .and_then(parse_rfc3339)
            .map(format_rfc3339)
            .unwrap_or_else(|| self.now_s());
        if self.cfg.focus_mode {
            self.receive_focus(before.items, Some(next_at)).await?;
        } else {
            self.enqueue_flash(&before.items, next_at).await?;
        }
        Ok(())
    }

    /// 快讯入队：每个目标渠道按其格式分批，各批一条 outbox。
    #[allow(dead_code)]
    async fn enqueue_flash(
        &self,
        items: &[PushItem],
        next_at: String,
    ) -> Result<(), plugin_sdk::ServiceError> {
        let chans = self.channels().await;
        let infos: Vec<ChannelInfo> = chans.iter().map(|c| c.info.clone()).collect();
        let targets = select::flash_channels(&infos);
        if targets.is_empty() {
            let item_ids = items.iter().map(|i| i.item_id).collect();
            self.insert_outbox(NewOutbox {
                user_requested: false,
                kind: OutboxKind::Flash,
                channel: String::new(),
                batch: 1,
                text: render::render_flash(items, channel::TextFormat::Plain, 2000)
                    .into_iter()
                    .map(|b| b.text)
                    .collect::<Vec<_>>()
                    .join("\n"),
                items: Some(items.to_vec()),
                item_ids,
                next_at: None,
                status: OutboxStatus::Manual,
                last_error: Some("无可用渠道".into()),
            })
            .await?;
            return Ok(());
        }
        for t in targets {
            for (i, b) in render::render_flash(items, t.format, t.max_bytes)
                .into_iter()
                .enumerate()
            {
                let part = &items[b.items.clone()];
                self.insert_outbox(NewOutbox {
                    user_requested: false,
                    kind: OutboxKind::Flash,
                    channel: t.id.clone(),
                    batch: i as i64 + 1,
                    text: b.text,
                    items: Some(part.to_vec()),
                    item_ids: part.iter().map(|i| i.item_id).collect(),
                    next_at: Some(next_at.clone()),
                    status: OutboxStatus::Pending,
                    last_error: None,
                })
                .await?;
            }
        }
        Ok(())
    }

    /// `push.before` 拦截器：免打扰时段内的快讯延后到时段结束。
    fn quiet_intercept(&self, ev: Event) -> Intercept {
        let Some(mut b) = parse::<PushBefore>(&ev) else {
            return Intercept::Continue(ev.payload);
        };
        let Some(q) = &self.cfg.quiet else {
            return Intercept::Continue(ev.payload);
        };
        if b.kind != "flash" {
            return Intercept::Continue(ev.payload);
        }
        let Some(end) = clock::quiet_end(self.now(), self.cfg.offset, q) else {
            return Intercept::Continue(ev.payload);
        };
        let existing = b.defer_until.as_deref().and_then(parse_rfc3339);
        let chosen = existing.map_or(end, |e| e.max(end));
        b.defer_until = Some(format_rfc3339(chosen));
        match serde_json::to_value(&b) {
            Ok(v) => Intercept::Continue(v),
            Err(_) => Intercept::Continue(ev.payload),
        }
    }
}

/// 解析事件载荷；失败只记日志（重试也不会成功）。
fn parse<T: serde::de::DeserializeOwned>(ev: &Event) -> Option<T> {
    match ev.parse::<T>() {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("事件 {} 载荷无法解析：{e}", ev.kind);
            None
        }
    }
}

/// 解析 JSON 数组形式的 id 列表。
pub(crate) fn parse_ids(s: Option<&str>) -> Vec<i64> {
    s.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

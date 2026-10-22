//! 集成测试：内存 hub + 内存 SQLite + 假渠道 + 固定时钟。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use plugin_sdk::contracts::channel::{ChannelInfo, ChannelState, SendRequest, TextFormat};
use plugin_sdk::contracts::dispatch::{OutboxKind, OutboxStatus};
use plugin_sdk::contracts::events::{CHANNEL_STATUS, ITEM_SCREENED, PUSH_PENDING, SOURCE_FAILED};
use plugin_sdk::contracts::events::{SOURCE_RECOVERED, TIMER_TICK};
use plugin_sdk::contracts::{parse_rfc3339, Intercept, ServiceError};
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Manifest, Plugin};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::outbox::{backoff, OutRow};
use crate::{Dispatch, DispatchPlugin};

const START: &str = "2026-10-16T02:00:00Z"; // 本地 10:00（+08:00），已过 08:00
const HINT: &str = "💬 回复任意内容，保持微信推送畅通";

fn t(s: &str) -> OffsetDateTime {
    parse_rfc3339(s).unwrap()
}

/// 假渠道：记录发出的正文，发送结果按脚本依次返回，脚本用完后一律成功。
struct Fake {
    info: ChannelInfo,
    script: Mutex<VecDeque<Result<(), ServiceError>>>,
    sent: Mutex<Vec<String>>,
}

impl Fake {
    fn script(&self, r: Result<(), ServiceError>) {
        self.script.lock().unwrap().push_back(r);
    }
    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

struct World {
    hub: Arc<TestHub>,
    d: Arc<Dispatch>,
    now: Arc<Mutex<OffsetDateTime>>,
    fakes: HashMap<String, Arc<Fake>>,
}

impl World {
    fn set(&self, s: &str) {
        *self.now.lock().unwrap() = t(s);
    }
    fn advance(&self, secs: i64) {
        let mut n = self.now.lock().unwrap();
        *n += time::Duration::seconds(secs);
    }
    fn fake(&self, name: &str) -> &Arc<Fake> {
        &self.fakes[name]
    }
    async fn rows(&self) -> Vec<OutRow> {
        self.d
            .store
            .query_as("SELECT * FROM outbox ORDER BY id", vec![])
            .await
            .unwrap()
    }
    async fn count(&self, sql: &str) -> usize {
        self.d.store.query(sql, vec![]).await.unwrap().len()
    }
    fn web(&self) -> plugin_sdk::contracts::Service {
        self.hub
            .context("web", json!({}), test_http_client())
            .service("dispatch")
    }
}

type Spec = (&'static str, bool, ChannelState, TextFormat);

const WECHAT: Spec = ("wechat", false, ChannelState::Ok, TextFormat::Plain);
const WECOM_BACKUP: Spec = ("wecom", true, ChannelState::Ok, TextFormat::Markdown);

async fn setup(mut cfg: Value, specs: &[Spec]) -> World {
    // Existing tests retain explicit legacy mode to cover the compatibility sender/digest.
    // New focus-policy integration cases below opt into the production default.
    if cfg.get("focus_mode").is_none() {
        cfg["focus_mode"] = json!(false);
    }

    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let personal = specs.iter().any(|s| s.0 == "wechat-ilink");
    hub.register_fn("screen","screening",move |method,params|async move {if method!="get"{return Err(ServiceError::method_not_found(&method))}Ok(json!({"screenings":params["ids"].as_array().unwrap().iter().map(|id|json!({"item_id":id,"level":if personal {"urgent"}else{"important"},"status":"done","attempts":0,"assets":["BTC"],"direction":"up"})).collect::<Vec<_>>()}))});
    let mut fakes = HashMap::new();
    for (name, backup, state, format) in specs {
        let fake = Arc::new(Fake {
            info: ChannelInfo {
                id: name.to_string(),
                name: name.to_string(),
                format: *format,
                max_bytes: if *format == TextFormat::Plain {
                    2000
                } else {
                    4000
                },
                backup: *backup,
                state: *state,
                quota_left: None,
                last_received_at: None,
                quota_limit: None,
                expires_at: None,
                detail: None,
            },
            script: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
        });
        let f = fake.clone();
        hub.register_fn(name, "channel", move |method, params| {
            let f = f.clone();
            async move {
                match method.as_str() {
                    "info" => Ok(serde_json::to_value(&f.info).unwrap()),
                    "send" => {
                        let req: SendRequest = serde_json::from_value(params).unwrap();
                        let r = f.script.lock().unwrap().pop_front().unwrap_or(Ok(()));
                        r?;
                        f.sent.lock().unwrap().push(req.text);
                        Ok(json!({ "message_id": "m1" }))
                    }
                    m => Err(ServiceError::method_not_found(m)),
                }
            }
        });
        fakes.insert(name.to_string(), fake);
    }
    let now = Arc::new(Mutex::new(t(START)));
    let n2 = now.clone();
    let ctx = hub.context("dispatch", cfg, test_http_client());
    let d = Dispatch::setup(ctx, Arc::new(move || *n2.lock().unwrap()))
        .await
        .unwrap();
    World { hub, d, now, fakes }
}

fn flash_payload(id: i64, title: &str) -> Value {
    json!({
        "kind": "flash",
        "items": [{
            "item_id": id, "level": "urgent", "title": title, "impact": "影响判断",
            "url": "https://x/1", "source_id": "okx"
        }]
    })
}

async fn push_flash(w: &World, id: i64, title: &str) {
    w.hub
        .deliver(PUSH_PENDING, flash_payload(id, title))
        .await
        .unwrap();
}

async fn screened(w: &World, id: i64, level: &str, title: &str) {
    w.hub
        .deliver(
            ITEM_SCREENED,
            json!({
                "item_id": id, "source_id": "okx", "url": format!("https://x/{id}"),
                "title": title, "level": level, "method": "llm", "impact": "影响",
                "backfill": false
            }),
        )
        .await
        .unwrap();
}

async fn tick(w: &World, name: &str) {
    w.hub
        .deliver(TIMER_TICK, json!({ "name": name, "at": START }))
        .await
        .unwrap();
}

// ---------- 插件与配置 ----------

#[test]
fn manifest_is_valid() {
    Manifest::parse(DispatchPlugin.manifest()).unwrap();
}

#[test]
fn settings_defaults_and_errors() {
    let s = crate::Settings::from_config(&Value::Null).unwrap();
    assert_eq!(s.slot_text, "08:00");
    assert!(s.quiet.is_none());
    assert_eq!(s.renew_hint, HINT);
    assert!(crate::Settings::from_config(&json!({ "utc_offset": "8" })).is_err());
    assert!(crate::Settings::from_config(&json!({ "digest_time": "25:00" })).is_err());
    assert!(crate::Settings::from_config(&json!({ "quiet_hours": "bad" })).is_err());
}

#[tokio::test]
async fn start_provides_service() {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let ctx = hub.context("dispatch", json!({}), test_http_client());
    DispatchPlugin.start(ctx).await.unwrap();
    let r = hub.call("dispatch", "outbox", json!({})).await.unwrap();
    assert_eq!(r["entries"], json!([]));
    hub.stop().await;
}

#[test]
fn backoff_doubles_and_caps() {
    assert_eq!(backoff(1).whole_seconds(), 30);
    assert_eq!(backoff(2).whole_seconds(), 60);
    assert_eq!(backoff(3).whole_seconds(), 120);
    assert_eq!(backoff(7).whole_seconds(), 1800);
    assert_eq!(backoff(50).whole_seconds(), 1800);
}

// ---------- 快讯入队 ----------

#[tokio::test]
async fn flash_enqueues_one_row_per_channel_and_sends() {
    let other = ("wecom", false, ChannelState::Ok, TextFormat::Markdown);
    let w = setup(json!({}), &[WECHAT, other]).await;
    push_flash(&w, 7, "OKX 上线新币").await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.kind == OutboxKind::Flash));
    assert!(rows.iter().all(|r| r.status == OutboxStatus::Pending));
    let mut channels: Vec<_> = rows.iter().map(|r| r.channel.clone()).collect();
    channels.sort();
    assert_eq!(channels, vec!["wechat", "wecom"]);
    let md = rows.iter().find(|r| r.channel == "wecom").unwrap();
    assert!(md.text.as_ref().unwrap().contains("##"));
    assert_eq!(md.entry().item_ids, vec![7]);

    w.d.send_round().await.unwrap();
    assert!(w
        .rows()
        .await
        .iter()
        .all(|r| r.status == OutboxStatus::Sent));
    assert_eq!(w.fake("wechat").sent().len(), 1);
    assert!(w.fake("wechat").sent()[0].contains("OKX 上线新币"));
    assert_eq!(w.fake("wecom").sent().len(), 1);
}

#[tokio::test]
async fn flash_stopped_by_interceptor_is_not_enqueued() {
    let w = setup(json!({}), &[WECHAT]).await;
    let other = w.hub.context("other", json!({}), test_http_client());
    other
        .intercept(plugin_sdk::contracts::events::PUSH_BEFORE, |_ev| async {
            Ok(Intercept::Stop)
        })
        .unwrap();
    push_flash(&w, 1, "被拦截").await;
    assert!(w.rows().await.is_empty());
}

#[tokio::test]
async fn flash_outside_quiet_hours_is_immediate() {
    let w = setup(json!({ "quiet_hours": "23:00-07:00" }), &[WECHAT]).await;
    push_flash(&w, 1, "白天快讯").await;
    let rows = w.rows().await;
    assert_eq!(rows[0].next_at.as_deref(), Some(START));
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 1);
}

#[tokio::test]
async fn flash_in_quiet_hours_is_deferred_to_the_end() {
    let w = setup(json!({ "quiet_hours": "23:00-07:00" }), &[WECHAT]).await;
    w.set("2026-10-16T15:30:00Z"); // 本地 23:30
    push_flash(&w, 1, "夜间快讯").await;
    let rows = w.rows().await;
    assert_eq!(rows[0].status, OutboxStatus::Pending);
    assert_eq!(rows[0].next_at.as_deref(), Some("2026-10-16T23:00:00Z"));
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    w.set("2026-10-16T23:00:00Z");
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 1);
}

#[tokio::test]
async fn flash_goes_to_backup_when_no_primary_is_ok() {
    let expired = ("wechat", false, ChannelState::Expired, TextFormat::Plain);
    let w = setup(json!({}), &[expired, WECOM_BACKUP]).await;
    push_flash(&w, 1, "走备用").await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "wecom");
}

#[tokio::test]
async fn flash_without_any_channel_becomes_manual() {
    let w = setup(json!({}), &[]).await;
    push_flash(&w, 1, "没渠道").await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, OutboxStatus::Manual);
    assert_eq!(rows[0].channel, "");
    assert_eq!(rows[0].last_error.as_deref(), Some("无可用渠道"));
}

// ---------- 日报 ----------

#[tokio::test]
async fn minute_tick_catches_up_once_per_day_and_uses_unsent_items() {
    let w = setup(json!({}), &[WECHAT]).await;
    screened(&w, 1, "important", "重要消息").await;
    screened(&w, 2, "normal", "一般消息").await;
    screened(&w, 3, "archive", "归档消息").await;

    // 还没到日报时间：不生成
    w.set("2026-10-15T23:00:00Z"); // 本地 07:00
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 0);

    w.set(START);
    tick(&w, "minute").await;
    let digests =
        w.d.store
            .query("SELECT * FROM digests", vec![])
            .await
            .unwrap();
    assert_eq!(digests.len(), 1);
    assert_eq!(digests[0]["date"], "2026-10-16");
    assert_eq!(digests[0]["slot"], "08:00");
    assert_eq!(digests[0]["item_count"], 2);
    assert_eq!(digests[0]["important_count"], 1);
    let did = digests[0]["id"].as_i64().unwrap();

    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, OutboxKind::Digest);
    assert_eq!(rows[0].channel, "wechat");
    assert_eq!(rows[0].entry().item_ids, vec![1, 2, 3]);
    let text = rows[0].text.clone().unwrap();
    assert!(text.contains("重要消息") && text.contains("一般消息"));
    assert!(text.ends_with(HINT));
    assert_eq!(digests[0]["outbox_ids"], json!(format!("[{}]", rows[0].id)));

    // 素材回写 digest_id
    let items =
        w.d.store
            .query("SELECT digest_id FROM digest_items", vec![])
            .await
            .unwrap();
    assert!(items.iter().all(|r| r["digest_id"].as_i64() == Some(did)));

    // 同一天再 tick / 再收到 daily-report：不重复
    tick(&w, "minute").await;
    tick(&w, "daily-report").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 1);
    assert_eq!(w.rows().await.len(), 1);

    // 之后的新消息不进今天的日报，进明天的
    screened(&w, 4, "normal", "明天的消息").await;
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 1);
    w.set("2026-10-17T02:00:00Z");
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 2);
    let rows = w.rows().await;
    assert_eq!(rows[1].entry().item_ids, vec![4]);
    assert!(!rows[1].text.clone().unwrap().contains("重要消息"));
}

#[tokio::test]
async fn daily_report_tick_generates_before_digest_time_check() {
    let w = setup(json!({}), &[WECHAT]).await;
    screened(&w, 1, "normal", "一般消息").await;
    w.set("2026-10-15T23:00:00Z"); // 本地 07:00，minute 不会触发
    tick(&w, "daily-report").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 1);
}

#[tokio::test]
async fn empty_digest_says_no_news() {
    let w = setup(json!({}), &[WECHAT]).await;
    tick(&w, "daily-report").await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].text.clone().unwrap().contains("今日暂无新消息"));
}

#[tokio::test]
async fn digest_without_channel_is_retried_later() {
    let w = setup(json!({}), &[]).await;
    screened(&w, 1, "normal", "一般消息").await;
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 0);
}

#[tokio::test]
async fn digest_uses_backup_when_it_is_the_only_channel() {
    let w = setup(json!({}), &[WECOM_BACKUP]).await;
    tick(&w, "daily-report").await;
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "wecom");
}

// ---------- outbox 一轮 ----------

#[tokio::test]
async fn rate_limited_backs_off_then_succeeds() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::rate_limited("慢点")));
    push_flash(&w, 1, "限流").await;
    w.d.send_round().await.unwrap();
    let r = &w.rows().await[0];
    assert_eq!(r.status, OutboxStatus::Pending);
    assert_eq!(r.attempts, Some(1));
    assert_eq!(r.next_at.as_deref(), Some("2026-10-16T02:00:30Z"));
    assert!(r.last_error.as_ref().unwrap().starts_with("[-32003]"));
    // 没到点不发
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    w.advance(31);
    w.d.send_round().await.unwrap();
    let r = &w.rows().await[0];
    assert_eq!(r.status, OutboxStatus::Sent);
    assert!(r.sent_at.is_some());
    assert_eq!(w.fake("wechat").sent().len(), 1);
}

#[tokio::test]
async fn five_failures_turn_manual() {
    let w = setup(json!({}), &[WECHAT]).await;
    for _ in 0..5 {
        w.fake("wechat")
            .script(Err(ServiceError::rate_limited("慢点")));
    }
    push_flash(&w, 1, "总是失败").await;
    for i in 1..=5 {
        w.d.send_round().await.unwrap();
        let r = &w.rows().await[0];
        assert_eq!(r.attempts, Some(i));
        w.advance(31 * 60);
    }
    let r = &w.rows().await[0];
    assert_eq!(r.status, OutboxStatus::Manual);
    assert!(w.fake("wechat").sent().is_empty());
    // 转人工后不再发送
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
}

#[tokio::test]
async fn invalid_params_turns_manual_at_once() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::invalid_params("超长")));
    push_flash(&w, 1, "超长").await;
    w.d.send_round().await.unwrap();
    let r = &w.rows().await[0];
    assert_eq!(r.status, OutboxStatus::Manual);
    assert!(r.last_error.as_ref().unwrap().starts_with("[-32602]"));
}

#[tokio::test]
async fn credential_expired_flash_fails_over_to_backup_and_rerenders() {
    let w = setup(json!({}), &[WECHAT, WECOM_BACKUP]).await;
    w.fake("wechat")
        .script(Err(ServiceError::credential_expired("过期")));
    push_flash(&w, 1, "改走备用").await;
    assert_eq!(w.rows().await[0].channel, "wechat");
    w.d.send_round().await.unwrap();
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "wecom");
    assert_eq!(rows[0].status, OutboxStatus::Sent);
    assert!(w.fake("wechat").sent().is_empty());
    let sent = w.fake("wecom").sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("##") && sent[0].contains("改走备用"));
}

#[tokio::test]
async fn quota_exhausted_without_backup_waits_ten_minutes_without_counting() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::quota_exhausted("额度用完")));
    push_flash(&w, 1, "额度").await;
    w.d.send_round().await.unwrap();
    let r = &w.rows().await[0];
    assert_eq!(r.status, OutboxStatus::Pending);
    assert_eq!(r.attempts.unwrap_or(0), 0);
    assert_eq!(r.next_at.as_deref(), Some("2026-10-16T02:10:00Z"));
    w.advance(5 * 60);
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    w.advance(6 * 60);
    w.d.send_round().await.unwrap();
    assert_eq!(w.rows().await[0].status, OutboxStatus::Sent);
}

#[tokio::test]
async fn failed_channel_does_not_block_other_channels() {
    let w = setup(
        json!({}),
        &[
            WECHAT,
            ("wecom", false, ChannelState::Ok, TextFormat::Markdown),
        ],
    )
    .await;
    w.fake("wechat")
        .script(Err(ServiceError::rate_limited("慢")));
    push_flash(&w, 1, "甲").await;
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    assert_eq!(w.fake("wecom").sent().len(), 1);
}

#[tokio::test]
async fn channel_status_ok_pulls_pending_forward() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::rate_limited("慢")));
    push_flash(&w, 1, "提前").await;
    w.d.send_round().await.unwrap();
    assert_eq!(
        w.rows().await[0].next_at.as_deref(),
        Some("2026-10-16T02:00:30Z")
    );
    w.hub
        .deliver(
            CHANNEL_STATUS,
            json!({ "channel": "wechat", "state": "ok" }),
        )
        .await
        .unwrap();
    assert_eq!(w.rows().await[0].next_at.as_deref(), Some(START));
    w.d.send_round().await.unwrap();
    assert_eq!(w.rows().await[0].status, OutboxStatus::Sent);
}

// ---------- 提醒 ----------

#[tokio::test]
async fn source_alerts_go_to_primary_and_dedup_for_six_hours() {
    let w = setup(json!({}), &[WECHAT, WECOM_BACKUP]).await;
    let failed = json!({ "source_id": "okx", "consecutive_failures": 3, "error": "超时" });
    w.hub.deliver(SOURCE_FAILED, failed.clone()).await.unwrap();
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, OutboxKind::Alert);
    assert_eq!(rows[0].channel, "wechat");
    let text = rows[0].text.clone().unwrap();
    assert!(text.contains("信源异常") && text.contains("okx") && text.contains("超时"));

    w.hub.deliver(SOURCE_FAILED, failed.clone()).await.unwrap();
    assert_eq!(w.rows().await.len(), 1);
    w.advance(5 * 3600);
    w.hub.deliver(SOURCE_FAILED, failed.clone()).await.unwrap();
    assert_eq!(w.rows().await.len(), 1);
    w.advance(3600 + 1);
    w.hub.deliver(SOURCE_FAILED, failed).await.unwrap();
    assert_eq!(w.rows().await.len(), 2);

    w.hub
        .deliver(SOURCE_RECOVERED, json!({ "source_id": "okx" }))
        .await
        .unwrap();
    let rows = w.rows().await;
    assert_eq!(rows.len(), 3);
    assert!(rows[2].text.clone().unwrap().contains("信源恢复"));

    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 3);
    assert!(w.fake("wecom").sent().is_empty());
}

#[tokio::test]
async fn expired_primary_alerts_through_other_channel_with_dedup() {
    let w = setup(json!({}), &[WECHAT, WECOM_BACKUP]).await;
    let ev = json!({ "channel": "wechat", "state": "expired" });
    w.hub.deliver(CHANNEL_STATUS, ev.clone()).await.unwrap();
    let rows = w.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "wecom");
    assert!(rows[0].text.clone().unwrap().contains("wechat"));
    w.hub.deliver(CHANNEL_STATUS, ev).await.unwrap();
    assert_eq!(w.rows().await.len(), 1);

    // 备用渠道过期不提醒
    w.hub
        .deliver(
            CHANNEL_STATUS,
            json!({ "channel": "wecom", "state": "expired" }),
        )
        .await
        .unwrap();
    assert_eq!(w.rows().await.len(), 1);
}

// ---------- 服务 ----------

#[tokio::test]
async fn outbox_and_digests_services_filter() {
    let w = setup(json!({}), &[WECHAT]).await;
    push_flash(&w, 1, "甲").await;
    push_flash(&w, 2, "乙").await;
    tick(&w, "minute").await;
    w.d.send_round().await.unwrap();

    let all = w.hub.call("dispatch", "outbox", json!({})).await.unwrap();
    let entries = all["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert!(entries[0]["id"].as_i64() > entries[2]["id"].as_i64());
    let digest = w
        .hub
        .call("dispatch", "outbox", json!({ "kind": "digest" }))
        .await
        .unwrap();
    assert_eq!(digest["entries"].as_array().unwrap().len(), 1);
    let pending = w
        .hub
        .call(
            "dispatch",
            "outbox",
            json!({ "status": "pending", "limit": 1 }),
        )
        .await
        .unwrap();
    assert!(pending["entries"].as_array().unwrap().is_empty());
    let limited = w
        .hub
        .call("dispatch", "outbox", json!({ "limit": 2 }))
        .await
        .unwrap();
    assert_eq!(limited["entries"].as_array().unwrap().len(), 2);

    let ds = w.hub.call("dispatch", "digests", json!({})).await.unwrap();
    let list = ds["digests"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["date"], "2026-10-16");
    assert_eq!(list[0]["outbox_ids"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn preview_digest_does_not_touch_the_database() {
    let w = setup(json!({}), &[WECHAT, WECOM_BACKUP]).await;
    screened(&w, 1, "important", "重要消息").await;
    screened(&w, 2, "normal", "一般消息").await;
    let r = w
        .hub
        .call("dispatch", "preview_digest", json!({}))
        .await
        .unwrap();
    assert_eq!(r["item_count"], 2);
    assert!(r["markdown"][0].as_str().unwrap().contains("重要消息"));
    assert!(r["plain"][0].as_str().unwrap().contains("一般消息"));
    assert_eq!(w.count("SELECT id FROM digests").await, 0);
    assert_eq!(w.count("SELECT id FROM outbox").await, 0);
    assert_eq!(
        w.count("SELECT item_id FROM digest_items WHERE digest_id IS NULL")
            .await,
        2
    );
}

#[tokio::test]
async fn retry_requeues_manual_rows_for_web_only() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::invalid_params("超长")));
    push_flash(&w, 1, "转人工").await;
    w.d.send_round().await.unwrap();
    let id = w.rows().await[0].id;
    assert_eq!(w.rows().await[0].status, OutboxStatus::Manual);

    // 其他插件：拒绝
    let denied = w.hub.call("dispatch", "retry", json!({ "id": id })).await;
    assert_eq!(denied.unwrap_err().code, ServiceError::FORBIDDEN);
    assert_eq!(w.rows().await[0].status, OutboxStatus::Manual);

    // web：成功，次数清零
    let r = w.web().call("retry", json!({ "id": id })).await.unwrap();
    assert_eq!(r["status"], "pending");
    assert_eq!(r["attempts"], 0);
    w.d.send_round().await.unwrap();
    assert_eq!(w.rows().await[0].status, OutboxStatus::Sent);

    // 已发送的不能重试，不存在的 id 报参数错误
    let e = w
        .web()
        .call("retry", json!({ "id": id }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    let e = w
        .web()
        .call("retry", json!({ "id": 999 }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
}

#[tokio::test]
async fn test_send_queues_an_alert_for_web_only() {
    let w = setup(json!({}), &[WECHAT]).await;
    let denied = w
        .hub
        .call("dispatch", "test_send", json!({ "channel": "wechat" }))
        .await;
    assert_eq!(denied.unwrap_err().code, ServiceError::FORBIDDEN);
    assert!(w.rows().await.is_empty());

    let r = w
        .web()
        .call("test_send", json!({ "channel": "wechat" }))
        .await
        .unwrap();
    assert_eq!(r["kind"], "alert");
    assert_eq!(r["channel"], "wechat");
    assert!(r["text"].as_str().unwrap().contains("测试消息"));
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 1);

    let e = w
        .web()
        .call("test_send", json!({ "channel": "nope" }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    let e = w.web().call("nope", json!({})).await.unwrap_err();
    assert_eq!(e.code, ServiceError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn digest_survives_failure_before_bookkeeping() {
    let w = setup(json!({}), &[WECHAT]).await;
    screened(&w, 1, "normal", "一般消息").await;
    w.d.store
        .execute(
            "CREATE TRIGGER fail_digest BEFORE INSERT ON digests \
             BEGIN SELECT RAISE(ABORT, 'boom'); END",
            vec![],
        )
        .await
        .unwrap();
    let _ = w
        .hub
        .deliver(TIMER_TICK, json!({ "name": "minute", "at": START }))
        .await;
    assert_eq!(w.count("SELECT id FROM digests").await, 0);
    assert_eq!(w.rows().await.len(), 1); // 已入队，没记账
    w.d.store
        .execute("DROP TRIGGER fail_digest", vec![])
        .await
        .unwrap();
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 1);
    assert_eq!(w.rows().await.len(), 2); // 允许重复入队，不丢
    assert_eq!(
        w.count("SELECT item_id FROM digest_items WHERE digest_id IS NULL")
            .await,
        0
    );
}

fn flash_with_level(id: i64, level: &str) -> Value {
    json!({
        "kind": "flash",
        "items": [{
            "item_id": id, "level": level, "title": "标题", "impact": "影响判断",
            "url": "https://x/1", "source_id": "okx"
        }]
    })
}

#[tokio::test]
async fn push_urgent_off_skips_urgent_flash_only() {
    let w = setup(json!({ "push_urgent": false }), &[WECHAT]).await;
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(1, "urgent"))
        .await
        .unwrap();
    assert!(w.rows().await.is_empty());
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(2, "important"))
        .await
        .unwrap();
    assert_eq!(w.rows().await.len(), 1);
}

#[tokio::test]
async fn push_important_off_skips_important_flash_only() {
    let w = setup(json!({ "push_important": false }), &[WECHAT]).await;
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(1, "important"))
        .await
        .unwrap();
    assert!(w.rows().await.is_empty());
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(2, "urgent"))
        .await
        .unwrap();
    assert_eq!(w.rows().await.len(), 1);
}

#[tokio::test]
async fn push_switches_default_on() {
    let w = setup(json!({}), &[WECHAT]).await;
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(1, "urgent"))
        .await
        .unwrap();
    w.hub
        .deliver(PUSH_PENDING, flash_with_level(2, "important"))
        .await
        .unwrap();
    assert_eq!(w.rows().await.len(), 2);
}

#[tokio::test]
async fn digest_disabled_skips_scheduled_digest_but_keeps_preview() {
    let w = setup(json!({ "digest_enabled": false }), &[WECHAT]).await;
    screened(&w, 1, "normal", "一般消息").await;
    tick(&w, "daily-report").await;
    tick(&w, "minute").await;
    assert_eq!(w.count("SELECT id FROM digests").await, 0);
    assert!(w.rows().await.is_empty());
    let p = w.d.preview_digest().await.unwrap();
    assert_eq!(p.item_count, 1);
}

// Current default policy, using real SQLite/outbox and the fake screening/channel boundary.
async fn focus_push(w: &World, id: i64, level: &str, title: &str, at: &str) {
    let mut v = flash_with_level(id, level);
    v["items"][0]["title"] = json!(title);
    v["items"][0]["published_at"] = json!(at);
    w.hub.deliver(PUSH_PENDING, v).await.unwrap();
}
#[tokio::test]
async fn focus_urgent_deduplicates_event_across_ids_and_retries() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    focus_push(&w, 1, "urgent", "同一个安全事件", START).await;
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 1);
    focus_push(&w, 1, "urgent", "同一个安全事件", START).await;
    focus_push(&w, 2, "urgent", "同一个安全事件", START).await;
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat").sent().len(), 1);
}
#[tokio::test]
async fn focus_important_merges_and_rejects_old_or_unknown_publication() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    focus_push(&w, 1, "important", "第一个重点", START).await;
    w.advance(120);
    focus_push(&w, 2, "important", "第二个重点", "2026-10-16T02:02:00Z").await;
    focus_push(&w, 3, "urgent", "过期紧急", "2026-10-15T19:00:00Z").await;
    w.hub
        .deliver(PUSH_PENDING, flash_payload(4, "没有发布时间"))
        .await
        .unwrap();
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    w.advance(180);
    w.d.send_round().await.unwrap();
    let sent = w.fake("wechat").sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("第一个重点") && sent[0].contains("第二个重点"));
    assert!(!sent[0].contains("过期紧急"));
}
#[tokio::test]
async fn focus_settings_clear_pending_and_do_not_backfill() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    focus_push(&w, 1, "important", "旧重点", START).await;
    w.advance(60);
    let p = plugin_sdk::contracts::focus::PushSettings {
        scope: "followed".into(),
        assets: vec!["ETH".into()],
        ..Default::default()
    };
    w.d.save_focus_settings(p).await.unwrap();
    assert_eq!(w.count("SELECT * FROM focus_pending").await, 0);
    focus_push(&w, 2, "urgent", "新的BTC重点", "2026-10-16T02:01:00Z").await;
    w.advance(400);
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    assert_eq!(w.d.focus_settings().await.unwrap().assets, vec!["ETH"]);
    assert!(w
        .d
        .save_focus_settings(plugin_sdk::contracts::focus::PushSettings {
            scope: "followed".into(),
            ..Default::default()
        })
        .await
        .is_err());
}
#[tokio::test]
async fn focus_rechecks_time_before_channel_retry() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::unavailable("transient")));
    focus_push(&w, 1, "urgent", "即将过期事件", "2026-10-15T20:01:00Z").await;
    w.d.send_round().await.unwrap();
    w.advance(120);
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    assert_eq!(w.rows().await[0].status, OutboxStatus::Dropped);
}
#[tokio::test]
async fn focus_only_substantive_new_revision_notifies_again() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    w.hub.register_fn("screen","screening",|_,params|async move{Ok(json!({"screenings":params["ids"].as_array().unwrap().iter().map(|id|json!({"item_id":id,"status":"done","attempts":0,"assets":["BTC"],"direction":"down","focus":{"event_key":"平台安全事件","update_key":if id.as_i64()==Some(3){"恢复充提"}else{"暂停充提"},"priority":1,"change":"官方处置进展","basis":"需要核实原文","source_confirmed":false}})).collect::<Vec<_>>()}))});
    for id in 1..=3 {
        focus_push(&w, id, "urgent", &format!("报道{id}"), START).await;
        w.d.send_round().await.unwrap()
    }
    assert_eq!(w.fake("wechat").sent().len(), 2);
}
#[tokio::test]
async fn focus_default_does_not_send_legacy_daily_digest() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    screened(&w, 1, "normal", "一般消息").await;
    tick(&w, "minute").await;
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    assert_eq!(w.count("SELECT * FROM digests").await, 0);
}

#[tokio::test]
async fn focus_downgraded_items_are_removed_before_retry() {
    let w = setup(json!({"focus_mode":true}), &[WECHAT]).await;
    w.fake("wechat")
        .script(Err(ServiceError::unavailable("transient")));
    focus_push(&w, 1, "urgent", "已降级事件", START).await;
    w.d.send_round().await.unwrap();
    w.hub.register_fn("screen","screening",|_,params|async move{Ok(json!({"screenings":params["ids"].as_array().unwrap().iter().map(|id|json!({"item_id":id,"level":"normal","status":"done","attempts":0,"assets":["BTC"],"direction":"up"})).collect::<Vec<_>>()}))});
    w.advance(30);
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat").sent().is_empty());
    assert_eq!(w.rows().await[0].status, OutboxStatus::Dropped);
}

const PERSONAL_WECHAT: Spec = ("wechat-ilink", false, ChannelState::Ok, TextFormat::Plain);

#[tokio::test]
async fn recovered_wechat_does_not_drain_legacy_digests_and_alerts() {
    let w = setup(json!({"focus_mode":true}), &[PERSONAL_WECHAT]).await;
    for i in 0..40 {
        let mut n = crate::outbox::NewOutbox::alert("wechat-ilink".into(), format!("旧通知{i}"));
        if i < 29 {
            n.kind = OutboxKind::Digest;
        }
        w.d.insert_outbox(n).await.unwrap();
    }
    focus_push(&w, 100, "urgent", "新的紧急事件", START).await;
    w.hub
        .deliver(
            CHANNEL_STATUS,
            json!({"channel":"wechat-ilink","state":"ok","quota_left":10}),
        )
        .await
        .unwrap();
    w.d.send_round().await.unwrap();
    let sent = w.fake("wechat-ilink").sent();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("新的紧急事件"));
    assert_eq!(
        w.count("SELECT id FROM outbox WHERE kind='digest' AND status='dropped'")
            .await,
        29
    );
    assert_eq!(
        w.count("SELECT id FROM outbox WHERE kind='alert' AND status='manual'")
            .await,
        11
    );
    for _ in 0..3 {
        w.hub
            .deliver(
                CHANNEL_STATUS,
                json!({"channel":"wechat-ilink","state":"ok","quota_left":10}),
            )
            .await
            .unwrap();
        w.d.send_round().await.unwrap();
    }
    assert_eq!(w.fake("wechat-ilink").sent().len(), 1);
}

#[tokio::test]
async fn new_wechat_system_alert_is_recorded_without_spending_push_quota() {
    let w = setup(json!({"focus_mode":true}), &[PERSONAL_WECHAT]).await;
    w.hub
        .deliver(
            SOURCE_FAILED,
            json!({"source_id":"rss:test","consecutive_failures":3,"error":"timeout"}),
        )
        .await
        .unwrap();
    w.d.send_round().await.unwrap();
    assert!(w.fake("wechat-ilink").sent().is_empty());
    assert_eq!(w.rows().await[0].status, OutboxStatus::Manual);
}

#[tokio::test]
async fn wechat_only_current_urgent_with_no_added_count_or_quota_threshold() {
    let w = setup(json!({"focus_mode":true}), &[PERSONAL_WECHAT]).await;
    let fake = w.fake("wechat-ilink").clone();
    w.hub
        .register_fn("wechat-ilink", "channel", move |method, p| {
            let fake = fake.clone();
            async move {
                if method == "info" {
                    let mut info = fake.info.clone();
                    info.quota_left = Some(1);
                    Ok(serde_json::to_value(info).unwrap())
                } else {
                    fake.sent
                        .lock()
                        .unwrap()
                        .push(p["text"].as_str().unwrap().into());
                    Ok(json!({}))
                }
            }
        });
    focus_push(&w, 1, "urgent", "第一个紧急事件", START).await;
    w.d.send_round().await.unwrap();
    focus_push(&w, 2, "urgent", "第二个紧急事件", START).await;
    w.d.send_round().await.unwrap();
    assert_eq!(
        w.fake("wechat-ilink").sent().len(),
        2,
        "同一时间内、余额为1时均不额外限制紧急推送"
    );
    focus_push(&w, 3, "important", "重要但非紧急", START).await;
    w.advance(301);
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat-ilink").sent().len(), 2);
    // 入队时紧急，发送前降为重要，也不能发到微信。
    focus_push(&w, 4, "urgent", "已经降级的事件", START).await;
    w.hub.register_fn("screen","screening",|_,p|async move{Ok(json!({"screenings":p["ids"].as_array().unwrap().iter().map(|id|json!({"item_id":id,"level":"important","status":"done","attempts":0,"assets":[],"direction":"neutral"})).collect::<Vec<_>>()}))});
    w.d.send_round().await.unwrap();
    assert_eq!(w.fake("wechat-ilink").sent().len(), 2);
    w.web()
        .call("test_send", json!({"channel":"wechat-ilink"}))
        .await
        .unwrap();
    w.d.send_round().await.unwrap();
    assert_eq!(
        w.fake("wechat-ilink").sent().len(),
        3,
        "用户主动测试不受自动紧急过滤限制"
    );
}

#[tokio::test]
async fn queue_summary_counts_all_rows_without_paging_and_tracks_transitions() {
    let w = setup(json!({}), &[WECHAT, WECOM_BACKUP]).await;
    for i in 0..550 {
        let mut n = crate::outbox::NewOutbox::alert(
            if !(200..350).contains(&i) {
                "wechat"
            } else {
                "wecom"
            }
            .into(),
            "notification".into(),
        );
        if i >= 350 {
            n.status = OutboxStatus::Manual;
        }
        if (200..350).contains(&i) {
            n.next_at = Some("2026-10-17T02:00:00Z".into());
        }
        w.d.insert_outbox(n).await.unwrap();
    }
    for status in [OutboxStatus::Sent, OutboxStatus::Dropped] {
        let mut n = crate::outbox::NewOutbox::alert("wechat".into(), "history".into());
        n.status = status;
        w.d.insert_outbox(n).await.unwrap();
    }
    w.d.store
        .execute(
            "INSERT INTO focus_pending(event_key,payload,due) VALUES('waiting','{}',?)",
            vec![json!(START)],
        )
        .await
        .unwrap();
    let stats = w.web().call("queue_summary", json!({})).await.unwrap();
    assert_eq!(stats["total"]["pending"], 350);
    assert_eq!(stats["total"]["ready"], 200);
    assert_eq!(stats["total"]["delayed"], 150);
    assert_eq!(stats["total"]["manual"], 200);
    assert_eq!(stats["merging_events"], 1);
    assert_eq!(stats["channels"].as_array().unwrap().len(), 2);
    w.d.store
        .execute("UPDATE outbox SET status='sent' WHERE id=1", vec![])
        .await
        .unwrap();
    let next = w.web().call("queue_summary", json!({})).await.unwrap();
    assert_eq!(next["total"]["pending"], 349);
    assert_eq!(next["total"]["ready"], 199);
}

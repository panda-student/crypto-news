use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use plugin_sdk::contracts::events::{ITEM_COLLECTED, ITEM_SCREENED, PUSH_PENDING};
use plugin_sdk::contracts::screen::{
    Level, ScreenStatus, ScreeningClient, ScreeningQuery, StatsCount, StatsQuery, SERVICE,
};
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Context, Manifest, Plugin, ServiceError};
use serde_json::{json, Value};

use crate::{init, round::run_round, Inner, ScreenPlugin};

const RULEBOOK: &str = "# 规则手册\n\n## 硬规则\n- 紧急：被盗、exploit\n- 归档：广告\n\n\
## 关键词\n- 重要：监管、ETF\n- 一般：升级\n\n## 其他\n随便写点什么\n";
const OLD: &str = "2020-01-01T00:00:00Z";

type Reply = Result<String, ServiceError>;

/// 假模型：按 purpose 分别排队返回，记录全部 chat 请求。
#[derive(Default)]
struct World {
    not_ready: AtomicBool,
    screen: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Value>>,
}

impl World {
    fn screen_ok(&self, v: Value) {
        self.screen.lock().unwrap().push_back(Ok(v.to_string()));
    }
    fn screen_err(&self) {
        self.screen
            .lock()
            .unwrap()
            .push_back(Err(ServiceError::internal("模型挂了")));
    }
    fn requests(&self, purpose: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["purpose"] == purpose)
            .cloned()
            .collect()
    }
}

async fn setup() -> (Arc<TestHub>, Context, Arc<Inner>, Arc<World>) {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let world = Arc::new(World::default());
    let w = world.clone();
    hub.register_fn("llm", "llm", move |method, params| {
        let w = w.clone();
        async move {
            match method.as_str() {
                "ready" => Ok(json!({ "ready": !w.not_ready.load(Ordering::Relaxed) })),
                "chat" => {
                    w.requests.lock().unwrap().push(params.clone());
                    assert_eq!(params["purpose"], "screen", "不得额外调用 analyze");
                    let q = &w.screen;
                    let r = q
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| Err(ServiceError::unavailable("没有预设回复")))?;
                    Ok(json!({ "content": r, "model": "fake" }))
                }
                m => Err(ServiceError::method_not_found(m)),
            }
        }
    });
    hub.register_fn("rulebook", "rulebook", |_m, _p| async {
        Ok(json!({ "text": RULEBOOK }))
    });
    hub.register_fn("collect", "item-query", |_m, p| async move {
        let items: Vec<Value> = p["ids"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|id| {
                json!({
                    "id": id, "source_id": "s", "url": "http://u", "title": "t",
                    "summary": "摘要内容", "published_at": null,
                    "collected_at": "2026-01-01T00:00:00Z"
                })
            })
            .collect();
        Ok(json!({ "items": items }))
    });
    let ctx = hub.context("screen", json!({}), test_http_client());
    let inner = init(&ctx).await.unwrap();
    (hub, ctx, inner, world)
}

fn fresh() -> String {
    plugin_sdk::contracts::now_rfc3339()
}

async fn collect(hub: &TestHub, id: i64, title: &str, published: &str) {
    hub.deliver(
        ITEM_COLLECTED,
        json!({
            "item_id": id, "source_id": "src", "url": format!("http://u/{id}"),
            "title": title, "published_at": published
        }),
    )
    .await
    .unwrap();
}

async fn row(hub: &TestHub, id: i64) -> Value {
    let r = hub
        .call(SERVICE, "get", json!({ "ids": [id] }))
        .await
        .unwrap();
    r["screenings"][0].clone()
}

fn client(hub: &Arc<TestHub>, name: &str) -> ScreeningClient {
    let ctx = hub.context(name, json!({}), test_http_client());
    ScreeningClient(ctx.service(SERVICE))
}

#[test]
fn manifest_is_valid() {
    let m = Manifest::parse(ScreenPlugin.manifest()).unwrap();
    assert_eq!(m.name, "screen");
    assert_eq!(m.provides["screening"], "1.1");
    assert!(m.requires.contains_key("llm"));
    assert!(m.requires.contains_key("rulebook"));
}

#[tokio::test]
async fn collected_makes_pending_row_with_backfill_and_dedup() {
    let (hub, _ctx, _inner, _w) = setup().await;
    collect(&hub, 1, "新消息", &fresh()).await;
    collect(&hub, 2, "旧消息", OLD).await;
    collect(&hub, 1, "新消息", &fresh()).await;
    let r1 = row(&hub, 1).await;
    assert_eq!(r1["status"], "pending");
    assert_eq!(r1["backfill"], false);
    assert_eq!(r1["attempts"], 0);
    assert_eq!(row(&hub, 2).await["backfill"], true);
    let all = hub.call(SERVICE, "list", json!({})).await.unwrap();
    assert_eq!(all["screenings"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn merged_classification_preserves_hard_priority_and_impact() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "某所 被盗 事件", &fresh()).await;
    collect(&hub, 2, "项目广告", &fresh()).await;
    collect(&hub, 3, "普通消息", &fresh()).await;
    collect(&hub, 4, "另一条消息", &fresh()).await;
    w.screen_ok(json!({ "results": [
        { "id": "3", "level": "一般", "score": 0.4, "reason": "平淡" },
        { "id": 4, "level": "Important", "score": 3, "reason": "重要", "impact":"影响B" },
        { "id": 1, "level": "一般", "impact":"被盗会冲击市场" }
    ]}));

    assert_eq!(run_round(&inner).await, 4);

    // 只有一次批量调用；硬归档不调用，硬紧急同批补简析。
    let reqs = w.requests("screen");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["tier"], "small");
    assert_eq!(reqs[0]["json"], true);
    let msgs = reqs[0]["messages"].as_array().unwrap();
    assert!(msgs[0]["content"].as_str().unwrap().contains(RULEBOOK));
    let user: Value = serde_json::from_str(msgs[1]["content"].as_str().unwrap()).unwrap();
    let ids: Vec<i64> = user
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 3, 4]);

    let r1 = row(&hub, 1).await;
    assert_eq!(r1["level"], "urgent");
    assert_eq!(r1["method"], "rule");
    assert_eq!(r1["status"], "done");
    assert_eq!(r1["impact"], "被盗会冲击市场");
    assert_eq!(row(&hub, 2).await["level"], "archive");
    let r3 = row(&hub, 3).await;
    assert_eq!(r3["level"], "normal");
    assert_eq!(r3["method"], "llm");
    assert_eq!(r3["score"], 0.4);
    assert_eq!(r3["impact"], Value::Null);
    let r4 = row(&hub, 4).await;
    assert_eq!(r4["level"], "important");
    assert_eq!(r4["score"], 1.0);
    assert_eq!(r4["impact"], "影响B");

    // 重点简析随初筛一次返回，硬规则级别不能被模型降级。
    assert!(w.requests("analyze").is_empty());

    assert_eq!(hub.published(ITEM_SCREENED).len(), 4);
    let push = hub.published(PUSH_PENDING);
    assert_eq!(push.len(), 1);
    assert_eq!(push[0].payload["kind"], "flash");
    assert_eq!(push[0].payload["items"][0]["item_id"], 1);

    // 已完成的不再重筛
    assert_eq!(run_round(&inner).await, 0);
}

#[tokio::test]
async fn merged_failure_keeps_hard_urgent_without_extra_calls() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "exploit 事件", &fresh()).await;
    assert_eq!(run_round(&inner).await, 1);
    assert!(w.requests("analyze").is_empty());
    let r = row(&hub, 1).await;
    assert_eq!(r["level"], "urgent");
    assert_eq!(r["status"], "done");
    assert_eq!(r["impact"], Value::Null);
    assert_eq!(hub.published(ITEM_SCREENED).len(), 1);
    assert_eq!(hub.published(PUSH_PENDING).len(), 1);
}

#[tokio::test]
async fn backfill_urgent_does_not_push() {
    let (hub, _ctx, inner, _w) = setup().await;
    collect(&hub, 1, "被盗", OLD).await;
    run_round(&inner).await;
    let ev = hub.published(ITEM_SCREENED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].payload["backfill"], true);
    assert!(hub.published(PUSH_PENDING).is_empty());
}

#[tokio::test]
async fn important_does_not_push() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "普通", &fresh()).await;
    w.screen_ok(json!([{ "id": 1, "level": "important" }]));
    run_round(&inner).await;
    assert_eq!(row(&hub, 1).await["level"], "important");
    assert!(hub.published(PUSH_PENDING).is_empty());
}

#[tokio::test]
async fn failures_back_off_then_keyword_fallback() {
    let (hub, ctx, inner, w) = setup().await;
    collect(&hub, 1, "监管新规出台", &fresh()).await;
    w.screen_err();
    run_round(&inner).await;
    let r = row(&hub, 1).await;
    assert_eq!(r["attempts"], 1);
    assert_eq!(r["status"], "pending");
    let next = ctx
        .store()
        .query("SELECT next_at FROM screenings WHERE item_id = 1", vec![])
        .await
        .unwrap();
    assert!(next[0]["next_at"].as_str().is_some());
    // 退避期内不再取
    assert_eq!(run_round(&inner).await, 0);
    assert_eq!(w.requests("screen").len(), 1);

    for expect in [2, 3] {
        ctx.store()
            .execute("UPDATE screenings SET next_at = NULL", vec![])
            .await
            .unwrap();
        w.screen_err();
        run_round(&inner).await;
        assert_eq!(row(&hub, 1).await["attempts"], expect);
    }
    let r = row(&hub, 1).await;
    assert_eq!(r["status"], "done");
    assert_eq!(r["level"], "important");
    assert_eq!(r["method"], "keyword");
    let ev = hub.published(ITEM_SCREENED);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].payload["method"], "keyword");
}

#[tokio::test]
async fn three_failures_without_keyword_go_manual() {
    let (hub, ctx, inner, w) = setup().await;
    collect(&hub, 1, "毫无关系的消息", &fresh()).await;
    for _ in 0..3 {
        ctx.store()
            .execute("UPDATE screenings SET next_at = NULL", vec![])
            .await
            .unwrap();
        w.screen_err();
        run_round(&inner).await;
    }
    let r = row(&hub, 1).await;
    assert_eq!(r["status"], "manual");
    assert_eq!(r["attempts"], 3);
    assert!(hub.published(ITEM_SCREENED).is_empty());
    let manual = client(&hub, "web")
        .list(&ScreeningQuery {
            status: Some(ScreenStatus::Manual),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(manual.len(), 1);
}

#[tokio::test]
async fn missing_item_in_reply_counts_attempt_only_for_it() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "甲", &fresh()).await;
    collect(&hub, 2, "乙", &fresh()).await;
    w.screen_ok(json!({ "results": [{ "id": 1, "level": "归档" }] }));
    run_round(&inner).await;
    assert_eq!(row(&hub, 1).await["status"], "done");
    let r2 = row(&hub, 2).await;
    assert_eq!(r2["status"], "pending");
    assert_eq!(r2["attempts"], 1);
}

#[tokio::test]
async fn llm_not_ready_does_not_count() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "甲", &fresh()).await;
    w.not_ready.store(true, Ordering::Relaxed);
    assert_eq!(run_round(&inner).await, 0);
    assert!(w.requests("screen").is_empty());
    let r = row(&hub, 1).await;
    assert_eq!(r["attempts"], 0);
    assert_eq!(r["status"], "pending");
}

#[tokio::test]
async fn review_web_changes_level_and_republishes() {
    let (hub, _ctx, _inner, _w) = setup().await;
    collect(&hub, 1, "甲", &fresh()).await;
    collect(&hub, 2, "乙", OLD).await;
    let web = client(&hub, "web");

    let r = web.review(1, Level::Urgent).await.unwrap();
    assert_eq!(r.level, Some(Level::Urgent));
    assert_eq!(r.status, ScreenStatus::Done);
    assert_eq!(hub.published(ITEM_SCREENED).len(), 1);
    assert_eq!(hub.published(ITEM_SCREENED)[0].payload["method"], "manual");
    assert_eq!(hub.published(PUSH_PENDING).len(), 1);

    // 改成非紧急不推送；补抓的紧急也不推送
    web.review(1, Level::Normal).await.unwrap();
    web.review(2, Level::Urgent).await.unwrap();
    assert_eq!(hub.published(ITEM_SCREENED).len(), 3);
    assert_eq!(hub.published(PUSH_PENDING).len(), 1);
    assert_eq!(row(&hub, 1).await["level"], "normal");
}

#[tokio::test]
async fn review_rejects_non_web_and_unknown_id() {
    let (hub, _ctx, _inner, _w) = setup().await;
    collect(&hub, 1, "甲", &fresh()).await;
    let e = client(&hub, "dispatch")
        .review(1, Level::Urgent)
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::FORBIDDEN);
    let e = client(&hub, "web")
        .review(99, Level::Urgent)
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    assert!(hub.published(ITEM_SCREENED).is_empty());
}

#[tokio::test]
async fn operations_can_review_but_cannot_delete_screenings() {
    let (hub, _ctx, _inner, _w) = setup().await;
    collect(&hub, 1, "甲", &fresh()).await;
    let operations = client(&hub, "operations");
    assert_eq!(
        operations.review(1, Level::Urgent).await.unwrap().level,
        Some(Level::Urgent)
    );
    assert_eq!(
        operations.delete(&[1]).await.unwrap_err().code,
        ServiceError::FORBIDDEN
    );
    assert_eq!(row(&hub, 1).await["level"], "urgent");
}

#[tokio::test]
async fn get_list_archived_before_and_delete() {
    let (hub, _ctx, _inner, _w) = setup().await;
    for id in 1..=3 {
        collect(&hub, id, "x", &fresh()).await;
    }
    let web = client(&hub, "web");
    web.review(1, Level::Archive).await.unwrap();
    web.review(2, Level::Archive).await.unwrap();
    web.review(3, Level::Normal).await.unwrap();

    assert_eq!(web.get(&[1, 3, 99]).await.unwrap().len(), 2);
    let archived = web
        .list(&ScreeningQuery {
            level: Some(Level::Archive),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(archived.len(), 2);
    assert_eq!(web.list(&ScreeningQuery::default()).await.unwrap().len(), 3);

    let ids = web
        .archived_before("2999-01-01T00:00:00Z", 10)
        .await
        .unwrap();
    assert_eq!(ids, vec![1, 2]);
    assert!(web
        .archived_before("2000-01-01T00:00:00Z", 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        web.archived_before("2999-01-01T00:00:00Z", 1)
            .await
            .unwrap(),
        vec![1]
    );

    let e = web.delete(&[1]).await.unwrap_err();
    assert_eq!(e.code, ServiceError::FORBIDDEN);
    let m = client(&hub, "maintenance");
    assert_eq!(m.delete(&[1, 2, 99]).await.unwrap(), 2);
    assert_eq!(web.get(&[1, 2, 3]).await.unwrap().len(), 1);
}

#[tokio::test]
async fn assets_and_direction_saved_event_review_and_legacy() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "普通消息", &fresh()).await;
    collect(&hub, 2, "项目广告", &fresh()).await;
    w.screen_ok(json!({ "results": [
        { "id": 1, "level": "一般", "assets": [" btc ", "BTC", "eth", "bad-x"],
          "direction": "bullish" }
    ]}));
    run_round(&inner).await;

    let r1 = row(&hub, 1).await;
    assert_eq!(r1["assets"], json!(["BTC", "ETH"]));
    assert_eq!(r1["direction"], "up");
    let ev = hub
        .published(ITEM_SCREENED)
        .into_iter()
        .find(|e| e.payload["item_id"] == 1)
        .unwrap();
    assert_eq!(ev.payload["assets"], json!(["BTC", "ETH"]));
    assert_eq!(ev.payload["direction"], "up");
    // 硬规则：空币种、无方向
    let r2 = row(&hub, 2).await;
    assert_eq!(r2["assets"], json!([]));
    assert!(r2["direction"].is_null());

    // 人工改级保留
    let r = client(&hub, "web")
        .review(1, Level::Important)
        .await
        .unwrap();
    assert_eq!(r.assets, vec!["BTC", "ETH"]);
    assert_eq!(row(&hub, 1).await["direction"], "up");
    let last = hub.published(ITEM_SCREENED).pop().unwrap();
    assert_eq!(last.payload["assets"], json!(["BTC", "ETH"]));

    // 旧行 assets 为 NULL
    inner
        .ctx
        .store()
        .execute(
            "UPDATE screenings SET assets = NULL, direction = NULL WHERE item_id = 1",
            vec![],
        )
        .await
        .unwrap();
    let r1 = row(&hub, 1).await;
    assert_eq!(r1["assets"], json!([]));
    assert!(r1["direction"].is_null());
}

#[tokio::test]
async fn stats_groups_by_day_source_asset() {
    let (hub, _ctx, inner, _w) = setup().await;
    for id in 1..=5 {
        collect(&hub, id, "x", &fresh()).await;
    }
    let store = inner.ctx.store();
    let rows = [
        (
            1,
            "a",
            "important",
            "2026-03-01T23:59:59Z",
            r#"["BTC","ETH"]"#,
            "done",
        ),
        (
            2,
            "a",
            "important",
            "2026-03-02T00:00:00Z",
            r#"["BTC"]"#,
            "done",
        ),
        (
            3,
            "b",
            "normal",
            "2026-03-02T10:00:00Z",
            r#"["BTC"]"#,
            "done",
        ),
        (4, "b", "important", "2026-03-02T11:00:00Z", "[]", "done"),
        (
            5,
            "b",
            "important",
            "2026-03-02T12:00:00Z",
            r#"["BTC"]"#,
            "pending",
        ),
    ];
    for (id, src, level, at, assets, status) in rows {
        store
            .execute(
                "UPDATE screenings SET source_id = ?, level = ?, screened_at = ?, assets = ?, \
                 status = ? WHERE item_id = ?",
                vec![
                    json!(src),
                    json!(level),
                    json!(at),
                    json!(assets),
                    json!(status),
                    json!(id),
                ],
            )
            .await
            .unwrap();
    }
    let c = client(&hub, "web");
    let s = c
        .stats(&StatsQuery {
            since: "2026-03-01T00:00:00Z".into(),
            until: Some("2026-03-03T00:00:00Z".into()),
        })
        .await
        .unwrap();
    let flat = |v: &[StatsCount]| -> Vec<(String, Level, i64)> {
        v.iter()
            .map(|c| (c.key.clone(), c.level, c.count))
            .collect()
    };
    let k = |s: &str| s.to_string();
    assert_eq!(
        flat(&s.by_day),
        vec![
            (k("2026-03-01"), Level::Important, 1),
            (k("2026-03-02"), Level::Important, 2),
            (k("2026-03-02"), Level::Normal, 1),
        ]
    );
    assert_eq!(
        flat(&s.by_source),
        vec![
            (k("a"), Level::Important, 2),
            (k("b"), Level::Important, 1),
            (k("b"), Level::Normal, 1),
        ]
    );
    assert_eq!(
        flat(&s.by_asset),
        vec![
            (k("BTC"), Level::Important, 2),
            (k("BTC"), Level::Normal, 1),
            (k("ETH"), Level::Important, 1),
        ]
    );
    // until 不含
    let s = c
        .stats(&StatsQuery {
            since: "2026-03-01T00:00:00Z".into(),
            until: Some("2026-03-02T00:00:00Z".into()),
        })
        .await
        .unwrap();
    assert_eq!(s.by_day.len(), 1);
    let e = c
        .stats(&StatsQuery {
            since: "昨天".into(),
            until: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
}

#[tokio::test]
async fn zh_title_and_summary_saved_and_optional() {
    let (hub, _ctx, inner, w) = setup().await;
    collect(&hub, 1, "Bitcoin ETF approved", &fresh()).await;
    collect(&hub, 2, "Another headline", &fresh()).await;
    collect(&hub, 3, "项目广告", &fresh()).await;
    w.screen_ok(json!({ "results": [
        { "id": 1, "level": "一般", "title_zh": "比特币 ETF 获批", "summary_zh": "监管批准了现货 ETF" },
        { "id": 2, "level": "一般" }
    ]}));
    run_round(&inner).await;
    let r1 = row(&hub, 1).await;
    assert_eq!(r1["title_zh"], "比特币 ETF 获批");
    assert_eq!(r1["summary_zh"], "监管批准了现货 ETF");
    let r2 = row(&hub, 2).await;
    assert_eq!(r2["status"], "done");
    assert!(r2["title_zh"].is_null());
    assert!(r2["summary_zh"].is_null());
    // 规则命中不调用模型，留空
    let r3 = row(&hub, 3).await;
    assert_eq!(r3["method"], "rule");
    assert!(r3["title_zh"].is_null());
    // 请求显式设置了 max_tokens
    assert!(w.requests("screen")[0]["max_tokens"].as_u64().unwrap() <= 7936);
}

#[tokio::test]
async fn migration_3_upgrades_from_v2_and_is_repeatable() {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let ctx = hub.context("screen", json!({}), test_http_client());
    // 模拟旧版本：只有迁移 1、2
    ctx.store().migrate(1, crate::SCHEMA).await.unwrap();
    ctx.store().migrate(2, crate::MIGRATION_2).await.unwrap();
    ctx.store()
        .execute(
            "INSERT INTO screenings (item_id, status, created_at) VALUES (1, 'done', 'x')",
            vec![],
        )
        .await
        .unwrap();
    init(&ctx).await.unwrap();
    let rows = ctx
        .store()
        .query("SELECT title_zh, summary_zh FROM screenings", vec![])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    // 重复启动不报错
    ctx.store().migrate(3, crate::MIGRATION_3).await.unwrap();
    ctx.store().migrate(2, crate::MIGRATION_2).await.unwrap();
}

#[tokio::test]
async fn idle_loop_never_backfills_history() {
    let (_hub, ctx, inner, w) = setup().await;
    ctx.store().execute("INSERT INTO screenings(item_id,status,level,method,title,created_at) VALUES(90,'done','important','rule','Old news','2020-01-01T00:00:00Z')", vec![]).await.unwrap();
    let task = tokio::spawn(crate::run_loop(inner));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    task.abort();
    assert!(w.requests("screen").is_empty());
    assert!(w.requests("analyze").is_empty());
    let rows = ctx
        .store()
        .query("SELECT analyzed FROM screenings WHERE item_id=90", vec![])
        .await
        .unwrap();
    assert_eq!(rows[0]["analyzed"], 0);
}

#[tokio::test]
async fn exact_repeat_reuses_batch_and_persisted_result_but_changes_get_screened() {
    let (hub, ctx, inner, w) = setup().await;
    let date = fresh();
    collect(&hub, 1, "Platform withdrawal update", &date).await;
    collect(&hub, 2, "Platform withdrawal update", &date).await;
    w.screen_ok(json!({"results":[{"id":1,"level":"important","impact":"注意流动性风险","focus":{"event_key":"withdrawal","update_key":"pause","priority":1}}]}));
    run_round(&inner).await;
    assert_eq!(row(&hub, 1).await["level"], "important");
    assert_eq!(row(&hub, 2).await["impact"], "注意流动性风险");
    let requests = w.requests("screen");
    assert_eq!(requests.len(), 1);
    let input: Value =
        serde_json::from_str(requests[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input.as_array().unwrap().len(), 1);
    collect(&hub, 3, "Platform withdrawal update", &date).await;
    run_round(&inner).await;
    assert_eq!(w.requests("screen").len(), 1);
    assert_eq!(row(&hub, 3).await["level"], "important");
    // 新事实改标题、官方独立信源、摘要变化都必须重新判断。
    collect(&hub, 4, "Platform withdrawal update: losses doubled", &date).await;
    collect(&hub, 5, "Platform withdrawal update", &date).await;
    ctx.store()
        .execute(
            "UPDATE screenings SET source_id='official' WHERE item_id=5",
            vec![],
        )
        .await
        .unwrap();
    w.screen_ok(json!({"results":[{"id":4,"level":"urgent","impact":"风险升级"},{"id":5,"level":"urgent"}]}));
    run_round(&inner).await;
    assert_eq!(w.requests("screen").len(), 2);
    assert_eq!(row(&hub, 4).await["level"], "urgent");
    assert_eq!(row(&hub, 5).await["level"], "urgent");
    assert!(w.requests("analyze").is_empty());
}

#[tokio::test]
async fn changed_summary_missing_content_other_day_and_rules_never_reuse() {
    let (hub, ctx, inner, w) = setup().await;
    hub.register_fn("collect", "item-query", |_m, p| async move {
        Ok(
            json!({"items":p["ids"].as_array().unwrap().iter().map(|id|json!({
            "id":id,"source_id":"src","url":"http://test","title":"Same headline",
            "summary": match id.as_i64().unwrap() {3=>"损失翻倍，暂停范围扩大",4=>"",_=>"调查中"},
            "collected_at":fresh()
        })).collect::<Vec<_>>()}),
        )
    });
    let date = fresh();
    collect(&hub, 1, "Same headline", &date).await;
    w.screen_ok(json!([{"id":1,"level":"normal"}]));
    run_round(&inner).await;
    collect(&hub, 3, "Same headline", &date).await;
    collect(&hub, 4, "Same headline", &date).await;
    collect(&hub, 5, "Same headline", "2020-01-01T00:00:00Z").await;
    w.screen_ok(
        json!([{"id":3,"level":"urgent"},{"id":4,"level":"important"},{"id":5,"level":"normal"}]),
    );
    run_round(&inner).await;
    assert_eq!(row(&hub, 3).await["level"], "urgent");
    assert_eq!(w.requests("screen").len(), 2);
    let requests = w.requests("screen");
    let inputs: Value =
        serde_json::from_str(requests[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(inputs.as_array().unwrap().len(), 3);
    hub.register_fn("rulebook", "rulebook", |_, _| async {
        Ok(json!({"text":"new policy"}))
    });
    collect(&hub, 6, "Same headline", &date).await;
    w.screen_ok(json!([{"id":6,"level":"important"}]));
    run_round(&inner).await;
    assert_eq!(row(&hub, 6).await["level"], "important");
    assert_eq!(w.requests("screen").len(), 3);
    // 缺简析不会把重要级降级，也不会为补一个字段再调用模型。
    assert!(row(&hub, 6).await["impact"].is_null());
    assert!(w.requests("analyze").is_empty());
    assert_eq!(
        ctx.store()
            .query(
                "SELECT COUNT(*) n FROM screenings WHERE status='pending'",
                vec![]
            )
            .await
            .unwrap()[0]["n"],
        0
    );
}

#[tokio::test]
async fn model_may_upgrade_a_hard_important_rule() {
    let (hub, _ctx, inner, w) = setup().await;
    hub.register_fn("rulebook", "rulebook", |_, _| async {
        Ok(json!({"text":"## 硬规则\n- 重要：ETF"}))
    });
    collect(&hub, 1, "ETF重大风险升级", &fresh()).await;
    w.screen_ok(json!([{"id":1,"level":"urgent","impact":"风险升级"}]));
    run_round(&inner).await;
    assert_eq!(row(&hub, 1).await["level"], "urgent");
    assert_eq!(row(&hub, 1).await["impact"], "风险升级");
    assert_eq!(w.requests("screen").len(), 1);
}

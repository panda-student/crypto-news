use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plugin_sdk::contracts::events::{ITEM_COLLECTED, SOURCE_FAILED, SOURCE_RECOVERED};
use plugin_sdk::contracts::source::{
    ItemQuery, ItemQueryClient, SourceStatusClient, ITEM_QUERY_SERVICE, STATUS_SERVICE,
};
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Context, Manifest, Plugin, ServiceError};
use serde_json::{json, Value};

use crate::{init, round::run_round, CollectPlugin};

type Reply = Result<Value, ServiceError>;

/// 假信源提供者：list 固定返回给定信源，fetch 依次吐出预设结果，并记录收到的 cursor。
#[derive(Clone, Default)]
struct Fake {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    cursors: Arc<Mutex<Vec<Value>>>,
    fetches: Arc<AtomicUsize>,
}

impl Fake {
    fn push(&self, r: Reply) {
        self.replies.lock().unwrap().push_back(r);
    }
    fn cursors(&self) -> Vec<Value> {
        self.cursors.lock().unwrap().clone()
    }
    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
    fn register(&self, hub: &TestHub, provider: &str, id: &str, interval: u64) {
        let me = self.clone();
        let id = id.to_string();
        hub.register_fn(provider, "source", move |method, params| {
            let me = me.clone();
            let id = id.clone();
            async move {
                match method.as_str() {
                    "list" => Ok(json!({"sources": [
                        {"id": id, "name": format!("名称 {id}"), "interval_secs": interval}
                    ]})),
                    "fetch" => {
                        me.fetches.fetch_add(1, Ordering::SeqCst);
                        me.cursors.lock().unwrap().push(params["cursor"].clone());
                        me.replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| Ok(json!({"items": [], "cursor": null})))
                    }
                    m => Err(ServiceError::method_not_found(m)),
                }
            }
        });
    }
}

fn ok(items: Vec<Value>, cursor: Option<&str>) -> Reply {
    Ok(json!({ "items": items, "cursor": cursor }))
}

fn item(ext: Option<&str>, url: &str, title: &str) -> Value {
    json!({
        "external_id": ext, "url": url, "title": title,
        "summary": format!("摘要 {title}"), "published_at": "2026-01-01T00:00:00Z",
        "raw": "{}"
    })
}

async fn setup() -> (Arc<TestHub>, Context) {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let ctx = hub.context("collect", json!({}), test_http_client());
    init(&ctx).await.unwrap();
    (hub, ctx)
}

#[test]
fn manifest_is_valid() {
    let m = Manifest::parse(CollectPlugin.manifest()).unwrap();
    assert_eq!(m.name, "collect");
    assert!(m.requires["source"].all());
    assert_eq!(m.provides["item-query"], "1.0");
}

#[tokio::test]
async fn dedup_cursor_and_events() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 0);
    fake.push(ok(
        vec![
            item(Some("1"), "http://u/1", "一"),
            item(None, "http://u/2", "二"),
        ],
        Some("c1"),
    ));
    fake.push(ok(
        vec![
            item(Some("1"), "http://u/1-changed", "一"),
            item(None, "http://u/2", "二"),
            item(Some("3"), "http://u/3", "三"),
        ],
        None,
    ));
    fake.push(ok(vec![], Some("c2")));

    run_round(&ctx).await;
    assert_eq!(hub.published(ITEM_COLLECTED).len(), 2);
    let ev = &hub.published(ITEM_COLLECTED)[0].payload;
    assert_eq!(ev["source_id"], "a:x");
    assert_eq!(ev["title"], "一");

    run_round(&ctx).await;
    // 重复的 id:1、url:http://u/2 不重复入库，只有 3 是新的
    assert_eq!(hub.published(ITEM_COLLECTED).len(), 3);
    run_round(&ctx).await;
    // 第二轮返回 cursor=None，保留旧值 c1；第三轮收到的是 c1
    assert_eq!(fake.cursors(), vec![Value::Null, json!("c1"), json!("c1")]);

    let q = ItemQueryClient(ctx.service(ITEM_QUERY_SERVICE));
    assert_eq!(q.list(&ItemQuery::default()).await.unwrap().len(), 3);
    let st = SourceStatusClient(ctx.service(STATUS_SERVICE))
        .list()
        .await
        .unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, "ok");
    assert_eq!(st[0].total_items, 3);
    assert_eq!(st[0].name, "名称 a:x");
    let rows = ctx
        .store()
        .query("SELECT cursor FROM sources WHERE id = 'a:x'", vec![])
        .await
        .unwrap();
    assert_eq!(rows[0]["cursor"], "c2");
}

#[tokio::test]
async fn multiple_providers_and_list_failure_isolated() {
    let (hub, ctx) = setup().await;
    let a = Fake::default();
    let b = Fake::default();
    a.register(&hub, "a", "a:x", 0);
    b.register(&hub, "b", "b:y", 0);
    hub.register_fn("c", "source", |_m, _p| async {
        Err(ServiceError::internal("坏了"))
    });
    a.push(ok(vec![item(Some("1"), "http://a/1", "A")], None));
    b.push(ok(vec![item(Some("1"), "http://b/1", "B")], None));

    run_round(&ctx).await;
    // 同样的 external_id 在不同信源下互不影响
    assert_eq!(hub.published(ITEM_COLLECTED).len(), 2);
    assert_eq!(a.fetches(), 1);
    assert_eq!(b.fetches(), 1);
}

#[tokio::test]
async fn failure_alert_once_then_recovered_once() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 0);
    for _ in 0..4 {
        fake.push(Err(ServiceError::internal(
            "请求 http://h/p?token=SECRET 失败",
        )));
    }
    fake.push(ok(vec![], None));
    fake.push(ok(vec![], None));

    for _ in 0..2 {
        run_round(&ctx).await;
    }
    assert!(hub.published(SOURCE_FAILED).is_empty());
    run_round(&ctx).await;
    run_round(&ctx).await;
    let failed = hub.published(SOURCE_FAILED);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].payload["consecutive_failures"], 3);
    assert!(!failed[0].payload["error"]
        .as_str()
        .unwrap()
        .contains("SECRET"));

    let st = SourceStatusClient(ctx.service(STATUS_SERVICE))
        .list()
        .await
        .unwrap();
    assert_eq!(st[0].state, "failing");
    assert_eq!(st[0].consecutive_failures, 4);
    assert!(!st[0].last_error.as_deref().unwrap().contains("SECRET"));
    assert_eq!(st[0].last_success_at, None);

    run_round(&ctx).await;
    assert_eq!(hub.published(SOURCE_RECOVERED).len(), 1);
    run_round(&ctx).await;
    assert_eq!(hub.published(SOURCE_RECOVERED).len(), 1);
    let st = SourceStatusClient(ctx.service(STATUS_SERVICE))
        .list()
        .await
        .unwrap();
    assert_eq!(st[0].state, "ok");
    assert_eq!(st[0].consecutive_failures, 0);
    assert_eq!(st[0].last_error, None);
}

#[tokio::test]
async fn not_due_is_skipped() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 3600);
    run_round(&ctx).await;
    run_round(&ctx).await;
    assert_eq!(fake.fetches(), 1);
    let st = SourceStatusClient(ctx.service(STATUS_SERVICE))
        .list()
        .await
        .unwrap();
    assert_eq!(st[0].interval_secs, 3600);
}

#[tokio::test]
async fn item_query_methods() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 0);
    let mut big = item(Some("1"), "http://u/1", "比特币 100%");
    big["raw"] = json!("x".repeat(70 * 1024));
    fake.push(ok(
        vec![
            big,
            item(Some("2"), "http://u/2", "以太坊"),
            item(Some("3"), "http://u/3", "other_ _x"),
        ],
        None,
    ));
    run_round(&ctx).await;

    let q = ItemQueryClient(ctx.service(ITEM_QUERY_SERVICE));
    let all = q.list(&ItemQuery::default()).await.unwrap();
    assert_eq!(all.len(), 3);
    // 同一秒入库，按 id 倒序
    assert_eq!(all[0].title, "other_ _x");

    let hit = q
        .list(&ItemQuery {
            text: Some("以太".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(hit.len(), 1);
    // 通配符按字面匹配
    let pct = q
        .list(&ItemQuery {
            text: Some("100%".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pct.len(), 1);
    let us = q
        .list(&ItemQuery {
            text: Some("r_ _".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(us.len(), 1);
    let limited = q
        .list(&ItemQuery {
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
    let next = q
        .list(&ItemQuery {
            before_id: Some(limited[1].id),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(
        next[0].id, all[2].id,
        "cursor must preserve rows sharing the same timestamp"
    );
    let none = q
        .list(&ItemQuery {
            source_id: Some("nope".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(none.is_empty());
    let future = q
        .list(&ItemQuery {
            since: Some("2999-01-01T00:00:00Z".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(future.is_empty());
    let past = q
        .list(&ItemQuery {
            until: Some("2000-01-01T00:00:00Z".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(past.is_empty());

    let got = q.get(&[all[0].id, all[2].id, 9999]).await.unwrap();
    assert_eq!(got.len(), 2);
    assert!(q.get(&[]).await.unwrap().is_empty());

    let raw = q.raw(all[2].id).await.unwrap().unwrap();
    assert_eq!(raw.len(), 64 * 1024);
    assert_eq!(q.raw(9999).await.unwrap(), None);
}

#[tokio::test]
async fn start_runs_loop_and_serves() {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 60);
    fake.push(ok(vec![item(Some("1"), "http://u/1", "一")], None));

    let ctx = hub.context("collect", json!({"tick_secs": 1}), test_http_client());
    CollectPlugin.start(ctx).await.unwrap();
    for _ in 0..100 {
        if !hub.published(ITEM_COLLECTED).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hub.published(ITEM_COLLECTED).len(), 1);
    let r = hub.call(STATUS_SERVICE, "list", json!({})).await.unwrap();
    assert_eq!(r["sources"][0]["state"], "ok");
    assert!(hub.failures().is_empty());
    hub.stop().await;
}

#[tokio::test]
async fn delete_only_for_maintenance() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 0);
    fake.push(ok(
        vec![
            item(Some("1"), "http://u/1", "一"),
            item(Some("2"), "http://u/2", "二"),
            item(Some("3"), "http://u/3", "三"),
        ],
        None,
    ));
    run_round(&ctx).await;

    let q = ItemQueryClient(ctx.service(ITEM_QUERY_SERVICE));
    let all = q.list(&ItemQuery::default()).await.unwrap();
    assert_eq!(all.len(), 3);

    // 其他插件被拒绝，数据不变
    let err = q.delete(&[all[0].id]).await.unwrap_err();
    assert_eq!(err.code, ServiceError::FORBIDDEN);
    assert_eq!(q.get(&[all[0].id]).await.unwrap().len(), 1);

    let m = hub.context("maintenance", json!({}), test_http_client());
    let mq = ItemQueryClient(m.service(ITEM_QUERY_SERVICE));
    assert_eq!(mq.delete(&[]).await.unwrap(), 0);
    // 不存在的 id 不计数
    assert_eq!(mq.delete(&[all[0].id, all[1].id, 9999]).await.unwrap(), 2);
    assert!(q.get(&[all[0].id, all[1].id]).await.unwrap().is_empty());
    assert_eq!(q.raw(all[0].id).await.unwrap(), None);
    assert_eq!(q.list(&ItemQuery::default()).await.unwrap().len(), 1);

    let too_many: Vec<i64> = (1..=1001).collect();
    let err = mq.delete(&too_many).await.unwrap_err();
    assert_eq!(err.code, ServiceError::INVALID_PARAMS);
    let ok_max: Vec<i64> = (10_000..11_000).collect();
    assert_eq!(mq.delete(&ok_max).await.unwrap(), 0);
}

#[tokio::test]
async fn retry_fetches_without_waiting_for_tick() {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 3600);

    // tick 与信源间隔都很大：没有 retry 就不会有第二次抓取
    let ctx = hub.context("collect", json!({"tick_secs": 3600}), test_http_client());
    CollectPlugin.start(ctx).await.unwrap();
    wait_fetches(&fake, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.fetches(), 1);

    let r = hub
        .call(STATUS_SERVICE, "retry", json!({"id": "a:x"}))
        .await
        .unwrap();
    assert_eq!(r["ok"], true);
    wait_fetches(&fake, 2).await;
    assert!(hub.failures().is_empty());
    hub.stop().await;
}

#[tokio::test]
async fn retry_unknown_id_is_invalid_params() {
    let (hub, ctx) = setup().await;
    let fake = Fake::default();
    fake.register(&hub, "a", "a:x", 60);
    run_round(&ctx).await;
    let c = SourceStatusClient(ctx.service(STATUS_SERVICE));
    c.retry("a:x").await.unwrap();
    let err = c.retry("nope").await.unwrap_err();
    assert_eq!(err.code, ServiceError::INVALID_PARAMS);
    let err = hub
        .call(STATUS_SERVICE, "retry", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ServiceError::INVALID_PARAMS);
}

async fn wait_fetches(fake: &Fake, n: usize) {
    for _ in 0..100 {
        if fake.fetches() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等不到第 {n} 次抓取，当前 {}", fake.fetches());
}

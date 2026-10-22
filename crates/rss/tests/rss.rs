use std::time::Duration;

use plugin_sdk::contracts::source::{FeedAdminClient, FeedInput, SourceClient};
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::Plugin;
use rss::{default_feeds, parse_feed, Feed, RssPlugin};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SEC: &[u8] = include_bytes!("fixtures/sec.rss");
const CFTC: &[u8] = include_bytes!("fixtures/cftc.rss");
const FED: &[u8] = include_bytes!("fixtures/fed.xml");

#[test]
fn parses_sec_with_guid() {
    let items = parse_feed(SEC).unwrap();
    assert_eq!(items.len(), 2);
    let a = &items[0];
    assert_eq!(a.external_id.as_deref(), Some("sec-2026-1"));
    assert_eq!(a.url, "https://www.sec.gov/newsroom/2026-1");
    assert_eq!(a.title, "SEC Charges Firm");
    assert_eq!(a.summary.as_deref(), Some("The SEC today charged & fined."));
    assert_eq!(a.published_at.as_deref(), Some("2026-10-14T14:30:47Z"));
    assert!(a.raw.contains("sec-2026-1"));
    assert!(items[1].summary.is_none());
}

#[test]
fn parses_cftc_without_guid() {
    let items = parse_feed(CFTC).unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i.external_id.is_none()));
    assert_eq!(
        items[0].published_at.as_deref(),
        Some("2026-10-15T12:00:00Z")
    );
    assert_eq!(items[0].summary.as_deref(), Some("Short text"));
}

#[test]
fn parses_atom_uses_updated() {
    let items = parse_feed(FED).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].external_id.as_deref(),
        Some("fed-monetary-20261015a")
    );
    assert_eq!(
        items[0].published_at.as_deref(),
        Some("2026-10-15T18:00:00Z")
    );
}

#[test]
fn summary_truncated_to_500_chars() {
    let long = "字".repeat(800);
    let xml = format!(
        "<rss version=\"2.0\"><channel><title>t</title><item><title>x</title><link>https://e.com/1</link><description>{long}</description></item></channel></rss>"
    );
    let items = parse_feed(xml.as_bytes()).unwrap();
    assert_eq!(items[0].summary.as_ref().unwrap().chars().count(), 500);
}

#[test]
fn list_has_sixteen_unique_sources() {
    let feeds = default_feeds();
    assert_eq!(feeds.len(), 16);
    let mut ids: Vec<_> = feeds.iter().map(|f| f.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 16);
    assert!(feeds.iter().all(|f| f.id.starts_with("rss:")));
}

async fn start(
    server: &MockServer,
    delays: Vec<Duration>,
) -> (std::sync::Arc<TestHub>, SourceClient) {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    let plugin = RssPlugin::with_feeds(
        vec![Feed::new(
            "rss:t",
            "测试",
            &format!("{}/feed", server.uri()),
        )],
        delays,
    );
    let ctx = hub.context(
        "rss",
        json!({"contact_email": "me@example.com"}),
        test_http_client(),
    );
    plugin.start(ctx).await.unwrap();
    let client = SourceClient(hub_service(&hub));
    (hub, client)
}

fn hub_service(hub: &std::sync::Arc<TestHub>) -> plugin_sdk::contracts::Service {
    let ctx = hub.context("caller", json!({}), test_http_client());
    ctx.service("source")
}

#[tokio::test]
async fn sends_ua_with_email_and_returns_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .and(header("user-agent", "crypto-news/0.1 (me@example.com)"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SEC))
        .expect(1)
        .mount(&server)
        .await;
    let (_hub, client) = start(&server, vec![]).await;
    let list = client.list().await.unwrap();
    assert_eq!(list[0].interval_secs, 300);
    let r = client.fetch("rss:t", None).await.unwrap();
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.cursor.as_deref(), Some("2026-10-14T14:30:47Z"));
}

#[tokio::test]
async fn retries_403_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(CFTC))
        .with_priority(2)
        .mount(&server)
        .await;
    let (_hub, client) = start(&server, vec![Duration::ZERO, Duration::ZERO]).await;
    let r = client.fetch("rss:t", None).await.unwrap();
    assert_eq!(r.items.len(), 2);
}

#[tokio::test]
async fn persistent_403_fails_without_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(403))
        .expect(3)
        .mount(&server)
        .await;
    let (_hub, client) = start(&server, vec![Duration::ZERO, Duration::ZERO]).await;
    let e = client.fetch("rss:t", None).await.unwrap_err();
    assert_eq!(e.code, plugin_sdk::ServiceError::UNAVAILABLE);
    assert!(e.message.contains("403"));
    assert!(!e.message.contains("http"));
}

#[test]
fn manifest_is_valid() {
    let m = plugin_sdk::Manifest::parse(RssPlugin::new().manifest()).unwrap();
    assert_eq!(m.name, "rss");
    assert_eq!(m.provides.get("source").map(String::as_str), Some("1.0"));
    assert_eq!(m.provides["feed-admin"], "1.0");
    assert!(m.tables.contains(&"feeds".to_string()));
}

#[tokio::test]
async fn starts_with_empty_config() {
    for cfg in [json!({}), json!({"interval_secs": 60})] {
        let hub = TestHub::new();
        hub.with_store(store::open_memory().await.unwrap());
        let ctx = hub.context("rss", cfg, test_http_client());
        RssPlugin::new().start(ctx).await.unwrap();
        let list = SourceClient(hub_service(&hub)).list().await.unwrap();
        assert_eq!(list.len(), default_feeds().len());
    }
}

#[tokio::test]
async fn no_email_user_agent_is_plain() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .and(header("user-agent", "crypto-news/0.1"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SEC))
        .expect(1)
        .mount(&server)
        .await;
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    let plugin = RssPlugin::with_feeds(
        vec![Feed::new(
            "rss:t",
            "测试",
            &format!("{}/feed", server.uri()),
        )],
        vec![],
    );
    plugin
        .start(hub.context("rss", json!({}), test_http_client()))
        .await
        .unwrap();
    let client = SourceClient(hub_service(&hub));
    client.fetch("rss:t", None).await.unwrap();
}

fn admin(hub: &std::sync::Arc<TestHub>) -> FeedAdminClient {
    let ctx = hub.context("caller", json!({}), test_http_client());
    FeedAdminClient(ctx.service("feed-admin"))
}

async fn start_default(hub: &std::sync::Arc<TestHub>) {
    let ctx = hub.context("rss", json!({}), test_http_client());
    RssPlugin::new().start(ctx).await.unwrap();
}

fn input(id: Option<&str>, name: &str, url: &str) -> FeedInput {
    FeedInput {
        id: id.map(str::to_string),
        name: name.into(),
        url: url.into(),
    }
}

#[tokio::test]
async fn seeds_defaults_once() {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    start_default(&hub).await;
    let feeds = admin(&hub).list().await.unwrap();
    assert_eq!(feeds.len(), 16);
    assert!(feeds.iter().all(|f| f.builtin && !f.paused));
    assert_eq!(hub_service_list(&hub).await.len(), 16);

    // 删除内置订阅后重启，不再播种
    admin(&hub).delete("rss:sec-press").await.unwrap();
    start_default(&hub).await;
    let feeds = admin(&hub).list().await.unwrap();
    assert_eq!(feeds.len(), 15);
    assert!(feeds.iter().all(|f| f.id != "rss:sec-press"));
}

async fn hub_service_list(
    hub: &std::sync::Arc<TestHub>,
) -> Vec<plugin_sdk::contracts::source::SourceInfo> {
    SourceClient(hub_service(hub)).list().await.unwrap()
}

#[tokio::test]
async fn add_edit_pause_delete() {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    start_default(&hub).await;
    let a = admin(&hub);

    let f = a
        .save(&input(None, " 自定义 ", " https://example.com/a.xml "))
        .await
        .unwrap();
    assert!(f.id.starts_with("rss:") && f.id.len() > 4);
    assert_eq!(f.name, "自定义");
    assert_eq!(f.url, "https://example.com/a.xml");
    assert!(!f.builtin && !f.paused);
    assert_eq!(hub_service_list(&hub).await.len(), 17);

    // 修改名称与地址，id 不变
    let g = a
        .save(&input(Some(&f.id), "改名", "http://example.com/b.xml"))
        .await
        .unwrap();
    assert_eq!(g.id, f.id);
    assert_eq!(g.name, "改名");
    assert_eq!(g.url, "http://example.com/b.xml");
    // 地址不变只改名也允许
    a.save(&input(Some(&f.id), "再改", "http://example.com/b.xml"))
        .await
        .unwrap();

    // 暂停后不出现在 source.list
    let p = a.set_paused(&f.id, true).await.unwrap();
    assert!(p.paused);
    let list = hub_service_list(&hub).await;
    assert_eq!(list.len(), 16);
    assert!(list.iter().all(|s| s.id != f.id));
    assert!(a.list().await.unwrap().iter().any(|x| x.id == f.id));
    assert!(!a.set_paused(&f.id, false).await.unwrap().paused);
    assert_eq!(hub_service_list(&hub).await.len(), 17);

    a.delete(&f.id).await.unwrap();
    assert_eq!(hub_service_list(&hub).await.len(), 16);
    assert!(a.delete(&f.id).await.is_err());
    assert!(a.set_paused(&f.id, true).await.is_err());
}

#[tokio::test]
async fn rejects_invalid_input() {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    start_default(&hub).await;
    let a = admin(&hub);
    for bad in [
        input(None, "", "https://example.com/x"),
        input(None, "  ", "https://example.com/x"),
        input(None, "x", "ftp://example.com/x"),
        input(None, "x", "file:///etc/passwd"),
        input(None, "x", "example.com/x"),
        input(None, "x", "https://"),
        input(None, "x", "https://a b.com"),
        input(Some("rss:nope"), "x", "https://example.com/x"),
        // 与内置订阅地址重复
        input(None, "x", "https://www.sec.gov/news/pressreleases.rss"),
    ] {
        let e = a.save(&bad).await.unwrap_err();
        assert_eq!(e.code, plugin_sdk::ServiceError::INVALID_PARAMS, "{bad:?}");
    }
    // 改成别的订阅的地址也被拒绝
    let f = a
        .save(&input(None, "x", "https://example.com/x"))
        .await
        .unwrap();
    let e = a
        .save(&input(
            Some(&f.id),
            "x",
            "https://www.cftc.gov/RSS/RSSGP/rssgp.xml",
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code, plugin_sdk::ServiceError::INVALID_PARAMS);
    assert_eq!(a.list().await.unwrap().len(), 17);
}

#[tokio::test]
async fn fetch_paused_or_missing_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SEC))
        .expect(0)
        .mount(&server)
        .await;
    let (hub, client) = start(&server, vec![]).await;
    admin(&hub).set_paused("rss:t", true).await.unwrap();
    let r = client.fetch("rss:t", None).await.unwrap();
    assert!(r.items.is_empty());
    let r = client.fetch("rss:gone", None).await.unwrap();
    assert!(r.items.is_empty());
}

#[tokio::test]
async fn media_feeds_have_own_intervals() {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    start_default(&hub).await;
    let list = hub_service_list(&hub).await;
    let iv = |id: &str| list.iter().find(|s| s.id == id).unwrap().interval_secs;
    assert_eq!(iv("rss:coindesk"), 300);
    assert_eq!(iv("rss:ethereum-blog"), 1800);
    assert_eq!(iv("rss:sec-press"), 300);
}

#[tokio::test]
async fn legacy_db_gets_new_builtins_only() {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    // 先用空清单建表，再手工造出旧库：只有旧标记，用户删掉了 sec-press
    let ctx = hub.context("rss", json!({}), test_http_client());
    RssPlugin::with_feeds(vec![], vec![])
        .start(ctx.clone())
        .await
        .unwrap();
    let store = ctx.store();
    store
        .execute(
            "INSERT INTO feed_meta (key, value) VALUES ('seeded', '1')",
            vec![],
        )
        .await
        .unwrap();
    for f in default_feeds().iter().take(8).skip(1) {
        store
            .execute(
                "INSERT INTO feeds (id, name, url, paused, builtin, created_at)                  VALUES (?, ?, ?, 0, 1, '2026-01-01T00:00:00Z')",
                vec![json!(f.id), json!(f.name), json!(f.url)],
            )
            .await
            .unwrap();
    }
    start_default(&hub).await;
    let feeds = admin(&hub).list().await.unwrap();
    assert_eq!(feeds.len(), 15);
    assert!(feeds.iter().any(|f| f.id == "rss:coindesk"));
    assert!(feeds.iter().all(|f| f.id != "rss:sec-press"));
    // 再次启动不重复
    start_default(&hub).await;
    assert_eq!(admin(&hub).list().await.unwrap().len(), 15);
}

#[tokio::test]
async fn sec_feed_without_email_makes_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(SEC))
        .expect(0)
        .mount(&server)
        .await;
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    RssPlugin::with_feeds(
        vec![Feed::new("rss:s", "SEC", "https://www.sec.gov/x.rss")],
        vec![],
    )
    .start(hub.context("rss", json!({"contact_email": "  "}), test_http_client()))
    .await
    .unwrap();
    let e = SourceClient(hub_service(&hub))
        .fetch("rss:s", None)
        .await
        .unwrap_err();
    assert!(e.message.contains("contact_email"), "{}", e.message);
}

use std::sync::{Arc, Mutex};
use std::time::Duration;

use plugin_sdk::contracts::channel::{ChannelClient, ChannelState, SendRequest};
use plugin_sdk::contracts::events::ChannelStatus;
use plugin_sdk::contracts::ServiceError;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Context, Plugin};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const KEY: &str = "fake-key-0123456789";

fn fake_secrets(hub: &Arc<TestHub>, key: Option<&str>) -> Arc<Mutex<Option<String>>> {
    let cell = Arc::new(Mutex::new(key.map(str::to_string)));
    let c2 = cell.clone();
    hub.register_fn("secrets", "secrets", move |m, _p| {
        let c = c2.clone();
        async move {
            assert_eq!(m, "get");
            Ok(json!({ "value": c.lock().unwrap().clone() }))
        }
    });
    cell
}

fn ctx(hub: &Arc<TestHub>, server: &MockServer) -> Context {
    hub.context(
        "wecom-webhook",
        json!({ "base_url": server.uri() }),
        test_http_client(),
    )
}

async fn started(hub: &Arc<TestHub>, server: &MockServer) -> ChannelClient {
    WecomWebhookPlugin.start(ctx(hub, server)).await.unwrap();
    ChannelClient(
        hub.context("x", json!({}), test_http_client())
            .service("channel"),
    )
}

fn req(text: &str) -> SendRequest {
    SendRequest {
        text: text.to_string(),
        reply_to: None,
    }
}

async fn mock_reply(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path("/cgi-bin/webhook/send"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn manifest_parses() {
    let m = plugin_sdk::Manifest::parse(WecomWebhookPlugin.manifest()).unwrap();
    assert_eq!(m.name, "wecom-webhook");
    assert_eq!(m.provides.get("channel").map(String::as_str), Some("1.0"));
}

#[tokio::test]
async fn send_success_checks_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cgi-bin/webhook/send"))
        .and(query_param("key", KEY))
        .and(wiremock::matchers::body_json(json!({
            "msgtype": "markdown",
            "markdown": {"content": "## 标题\n正文"}
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"errcode": 0, "errmsg": "ok"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let ch = started(&hub, &server).await;
    let info = ch.info().await.unwrap();
    assert_eq!(info.state, ChannelState::Ok);
    assert_eq!(info.max_bytes, 4000);
    assert!(info.backup);
    assert_eq!(info.id, "wecom-webhook");
    assert_eq!(info.quota_left, Some(20));
    assert_eq!(info.quota_limit, Some(20));
    let sent = ch.send(&req("## 标题\n正文")).await.unwrap();
    assert_eq!(sent.quota_left, Some(19));
    assert_eq!(ch.info().await.unwrap().quota_left, Some(19));
    // 启动时发一次 ok，之后状态未变化不再发
    let evs = hub.published("channel.status");
    assert_eq!(evs.len(), 1);
    assert_eq!(
        evs[0].parse::<ChannelStatus>().unwrap().state,
        ChannelState::Ok
    );
}

#[tokio::test]
async fn errcode_mapping() {
    let cases: [(u16, i64, i64); 6] = [
        (200, 45009, ServiceError::RATE_LIMITED),
        (200, 40058, ServiceError::INVALID_PARAMS),
        (200, 93000, ServiceError::CREDENTIAL_EXPIRED),
        (200, 99999, ServiceError::UNAVAILABLE),
        (500, 0, ServiceError::UNAVAILABLE),
        (200, 40035, ServiceError::INVALID_PARAMS),
    ];
    for (status, errcode, want) in cases {
        let server = MockServer::start().await;
        mock_reply(
            &server,
            status,
            json!({"errcode": errcode, "errmsg": format!("bad key {KEY}")}),
        )
        .await;
        let hub = TestHub::new();
        fake_secrets(&hub, Some(KEY));
        let ch = Arc::new(Channel::new(&ctx(&hub, &server), Duration::ZERO).unwrap());
        let (r, _) = ch.send(req("hi")).await;
        let e = r.unwrap_err();
        assert_eq!(e.code, want, "status {status} errcode {errcode}");
        assert!(!e.to_string().contains(KEY));
        assert_eq!(ch.info().await.quota_left, Some(20), "失败不扣减");
    }
}

#[tokio::test]
async fn status_follows_errors_and_recovers() {
    let server = MockServer::start().await;
    mock_reply(&server, 200, json!({"errcode": 93000, "errmsg": "x"})).await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let ch = started(&hub, &server).await;
    // 3 秒间隔只影响第二次发送，这里只发一次
    let e = ch.send(&req("hi")).await.unwrap_err();
    assert_eq!(e.code, ServiceError::CREDENTIAL_EXPIRED);
    assert_eq!(ch.info().await.unwrap().state, ChannelState::Expired);
    let states: Vec<_> = hub
        .published("channel.status")
        .iter()
        .map(|e| e.parse::<ChannelStatus>().unwrap().state)
        .collect();
    assert_eq!(states, vec![ChannelState::Ok, ChannelState::Expired]);
}

#[tokio::test]
async fn too_long_rejected_without_request() {
    let server = MockServer::start().await;
    mock_reply(&server, 200, json!({"errcode": 0})).await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let ch = started(&hub, &server).await;
    let e = ch.send(&req(&"a".repeat(4001))).await.unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    // 4000 字节整可以发（中文 3 字节：1333 个 + 1 个 ASCII）
    let ok = format!("{}a", "中".repeat(1333));
    assert_eq!(ok.len(), 4000);
    ch.send(&req(&ok)).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unbound_without_key() {
    let server = MockServer::start().await;
    mock_reply(&server, 200, json!({"errcode": 0})).await;
    let hub = TestHub::new();
    let key = fake_secrets(&hub, None);
    let ch = started(&hub, &server).await;
    let info = ch.info().await.unwrap();
    assert_eq!(info.state, ChannelState::Unbound);
    assert_eq!(info.quota_left, None);
    let e = ch.send(&req("hi")).await.unwrap_err();
    assert_eq!(e.code, ServiceError::CREDENTIAL_EXPIRED);
    assert!(server.received_requests().await.unwrap().is_empty());
    let first = hub.published("channel.status")[0]
        .parse::<ChannelStatus>()
        .unwrap();
    assert_eq!(first.state, ChannelState::Unbound);
    // 空白 key 也算未配置
    *key.lock().unwrap() = Some("  ".into());
    assert_eq!(ch.info().await.unwrap().state, ChannelState::Unbound);
}

#[tokio::test]
async fn bind_not_supported() {
    let server = MockServer::start().await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let ch = started(&hub, &server).await;
    let e = ch.bind().await.unwrap_err();
    assert_eq!(e.code, ServiceError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn backup_flag_from_config() {
    let server = MockServer::start().await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let c = hub.context(
        "wecom-webhook",
        json!({ "base_url": server.uri(), "backup": false }),
        test_http_client(),
    );
    WecomWebhookPlugin.start(c).await.unwrap();
    let ch = ChannelClient(
        hub.context("x", json!({}), test_http_client())
            .service("channel"),
    );
    assert!(!ch.info().await.unwrap().backup);
}

#[tokio::test]
async fn min_interval_between_sends() {
    let server = MockServer::start().await;
    mock_reply(&server, 200, json!({"errcode": 0})).await;
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let interval = Duration::from_millis(300);
    let ch = Arc::new(Channel::new(&ctx(&hub, &server), interval).unwrap());
    let t0 = tokio::time::Instant::now();
    ch.send(req("a")).await.0.unwrap();
    assert!(t0.elapsed() < interval, "首次发送不应等待");
    ch.send(req("b")).await.0.unwrap();
    assert!(
        t0.elapsed() >= interval,
        "第二次发送应至少间隔 {interval:?}"
    );
}

#[tokio::test]
async fn network_error_hides_key() {
    // 指向保留的 1 号端口，连接必然被拒绝；不能用“启动后关闭的 MockServer”，
    // 并行测试可能立刻复用同一端口并返回别的响应
    let uri = "http://127.0.0.1:1".to_string();
    let hub = TestHub::new();
    fake_secrets(&hub, Some(KEY));
    let c = hub.context(
        "wecom-webhook",
        json!({ "base_url": uri }),
        test_http_client(),
    );
    let ch = Channel::new(&c, Duration::ZERO).unwrap();
    let (r, state) = ch.send(req("hi")).await;
    let e = r.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(!e.to_string().contains(KEY));
    assert_eq!(state, ChannelState::Offline);
}

#[tokio::test]
async fn complete_webhook_url_extracts_only_key_when_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/cgi-bin/webhook/send"))
        .and(query_param("key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errcode": 0})))
        .expect(1)
        .mount(&server)
        .await;
    let hub = TestHub::new();
    fake_secrets(
        &hub,
        Some(&format!(
            " https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={KEY} "
        )),
    );
    let ch = started(&hub, &server).await;
    ch.send(&req("link regression fixture")).await.unwrap();
}

#[test]
fn webhook_input_validation() {
    assert_eq!(webhook_key(&format!(" {KEY} ")).unwrap(), KEY);
    for input in [
        "https://evil.example/cgi-bin/webhook/send?key=secret",
        "http://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=secret",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=a&key=b",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/upload_media?key=secret",
    ] {
        let e = webhook_key(input).unwrap_err();
        assert_eq!(e.code, ServiceError::INVALID_PARAMS);
        assert!(!e.message.contains("secret"));
    }
}

#[test]
fn minute_quota_recovers_each_success_after_sixty_seconds() {
    let mut window = SendWindow::default();
    let start = Instant::now();
    assert_eq!(window.remaining_at(start), 20);
    assert_eq!(window.record_at(start), 19);
    assert_eq!(window.record_at(start + Duration::from_secs(3)), 18);
    assert_eq!(window.remaining_at(start + Duration::from_secs(59)), 18);
    assert_eq!(window.remaining_at(start + Duration::from_secs(60)), 19);
    assert_eq!(window.remaining_at(start + Duration::from_secs(63)), 20);
    for _ in 0..21 {
        window.record_at(start + Duration::from_secs(64));
    }
    assert_eq!(window.remaining_at(start + Duration::from_secs(64)), 0);
    assert_eq!(window.remaining_at(start + Duration::from_secs(124)), 20);
}

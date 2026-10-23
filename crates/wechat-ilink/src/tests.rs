use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use plugin_sdk::contracts::channel::ChannelState;
use plugin_sdk::contracts::events::{CHANNEL_STATUS, USER_MESSAGE};
use plugin_sdk::contracts::format_rfc3339;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Caller, Manifest, Plugin, ServiceError};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::state::{compute_state, StateInput};
use crate::{
    init, Polled, Shared, WechatIlinkPlugin, KEY_CONTEXT, KEY_CREDENTIALS, MAX_BYTES,
    QUOTA_REMINDER,
};

const TOKEN: &str = "tok-secret-1";
const CTX_TOKEN: &str = "ctx-secret-2";

type Secrets = Arc<Mutex<HashMap<String, String>>>;

struct Env {
    hub: Arc<TestHub>,
    server: MockServer,
    secrets: Secrets,
    sh: Arc<Shared>,
}

async fn setup() -> Env {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    let secrets: Secrets = Arc::default();
    let s = secrets.clone();
    hub.register_fn("secrets", "secrets", move |m, p| {
        let s = s.clone();
        async move {
            let key = p["key"].as_str().unwrap_or_default().to_string();
            let mut map = s.lock().unwrap();
            match m.as_str() {
                "get" => Ok(json!({ "value": map.get(&key) })),
                "set" => {
                    map.insert(key, p["value"].as_str().unwrap().to_string());
                    Ok(json!({}))
                }
                "delete" => {
                    map.remove(&key);
                    Ok(json!({}))
                }
                other => Err(ServiceError::method_not_found(other)),
            }
        }
    });
    let server = MockServer::start().await;
    let ctx = hub.context(
        "wechat-ilink",
        json!({ "qr_base_url": server.uri() }),
        test_http_client(),
    );
    let sh = init(&ctx).await.unwrap();
    Env {
        hub,
        server,
        secrets,
        sh,
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn web() -> Caller {
    Caller::owner("web")
}

async fn call(env: &Env, caller: Caller, m: &str, p: Value) -> Result<Value, ServiceError> {
    env.sh.handle(caller, m, p).await
}

/// 直接设置运行时状态：已绑定、有会话凭证。
fn seed(env: &Env, quota: i64, last_user_at: Option<OffsetDateTime>) {
    let creds = json!({
        "token": TOKEN, "baseUrl": env.server.uri(),
        "accountId": "bot@im.bot", "userId": "u@im.wechat"
    });
    let ctx = json!({
        "account_key": "bot@im.bot", "user_id": "user-1",
        "context_token": CTX_TOKEN, "observed_at_unix_ms": 1
    });
    let mut rt = env.sh.rt.lock().unwrap();
    rt.creds = Some(serde_json::from_value(creds).unwrap());
    rt.session = Some(serde_json::from_value(ctx).unwrap());
    rt.quota_left = quota;
    rt.last_user_at = last_user_at;
}

fn recent() -> Option<OffsetDateTime> {
    Some(OffsetDateTime::now_utc() - Duration::minutes(5))
}

async fn mock_send(env: &Env, resp: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .respond_with(resp)
        .mount(&env.server)
        .await;
}

async fn sent_texts(env: &Env) -> Vec<String> {
    env.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/ilink/bot/sendmessage")
        .map(|r| {
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            v["msg"]["item_list"][0]["text_item"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn send_params(text: &str) -> Value {
    json!({ "text": text })
}

// ------------------------------------------------------------ 状态纯函数

fn input() -> StateInput {
    let now = OffsetDateTime::now_utc();
    StateInput {
        has_credentials: true,
        has_context: true,
        quota_left: 5,
        last_user_at: Some(now - Duration::hours(1)),
        now,
        session_hours: 24,
        receive_failing: false,
        rejected: false,
    }
}

#[test]
fn state_unbound_without_credentials() {
    let c = compute_state(&StateInput {
        has_credentials: false,
        has_context: true,
        ..input()
    });
    assert_eq!(c.state, ChannelState::Unbound);
    assert!(c.detail.unwrap().contains("扫码"));
}

#[test]
fn state_unbound_without_context() {
    let c = compute_state(&StateInput {
        has_context: false,
        ..input()
    });
    assert_eq!(c.state, ChannelState::Unbound);
    assert!(c.detail.unwrap().contains("发一条消息"));
}

#[test]
fn state_ok() {
    let c = compute_state(&input());
    assert_eq!(c.state, ChannelState::Ok);
    assert!(c.detail.is_none());
    assert!(c.expires_at.is_some());
}

#[test]
fn state_expired_by_time_or_rejection() {
    let i = input();
    let c = compute_state(&StateInput {
        last_user_at: Some(i.now - Duration::hours(25)),
        ..i
    });
    assert_eq!(c.state, ChannelState::Expired);
    let c = compute_state(&StateInput {
        rejected: true,
        ..input()
    });
    assert_eq!(c.state, ChannelState::Expired);
}

#[test]
fn state_quota_exhausted() {
    let c = compute_state(&StateInput {
        quota_left: 0,
        ..input()
    });
    assert_eq!(c.state, ChannelState::QuotaExhausted);
}

#[test]
fn state_offline_and_priority() {
    let c = compute_state(&StateInput {
        receive_failing: true,
        ..input()
    });
    assert_eq!(c.state, ChannelState::Offline);
    // 额度用完优先于 offline，过期优先于额度用完
    let c = compute_state(&StateInput {
        receive_failing: true,
        quota_left: 0,
        ..input()
    });
    assert_eq!(c.state, ChannelState::QuotaExhausted);
    let i = input();
    let c = compute_state(&StateInput {
        quota_left: 0,
        last_user_at: Some(i.now - Duration::hours(30)),
        ..i
    });
    assert_eq!(c.state, ChannelState::Expired);
}

#[test]
fn manifest_parses() {
    let m = Manifest::parse(WechatIlinkPlugin.manifest()).unwrap();
    assert_eq!(m.name, "wechat-ilink");
}

// ------------------------------------------------------------ 绑定

#[tokio::test]
async fn bind_returns_qr_and_stores_credentials_after_scan() {
    let env = setup().await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/get_bot_qrcode"))
        .respond_with(ok(
            json!({"qrcode": "qr1", "qrcode_img_content": "https://x/qr1"}),
        ))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ilink/bot/get_qrcode_status"))
        .respond_with(ok(json!({
            "status": "confirmed", "bot_token": TOKEN, "ilink_bot_id": "bot@im.bot",
            "ilink_user_id": "u@im.wechat", "baseurl": env.server.uri()
        })))
        .mount(&env.server)
        .await;

    let r = call(&env, web(), "bind", json!({})).await.unwrap();
    assert_eq!(r["qrcode"], "https://x/qr1");
    assert_eq!(r["state"], "unbound");

    // 手动跑一轮轮询（后台任务在测试里不会自动执行）
    assert!(!env.sh.bind_poll_once(1).await);
    {
        let secrets = env.secrets.lock().unwrap();
        assert!(secrets.contains_key(KEY_CREDENTIALS));
        assert!(!secrets.contains_key(KEY_CONTEXT));
    }
    let r = call(&env, web(), "bind_status", json!({})).await.unwrap();
    assert!(r["qrcode"].is_null());
    assert!(r["detail"].as_str().unwrap().contains("绑定成功"));
    assert_eq!(r["state"], "unbound"); // 还缺会话凭证
    assert!(!r.to_string().contains(TOKEN));
}

#[tokio::test]
async fn bind_poll_reports_expired_qr() {
    let env = setup().await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/get_bot_qrcode"))
        .respond_with(ok(
            json!({"qrcode": "qr1", "qrcode_img_content": "https://x/qr1"}),
        ))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ilink/bot/get_qrcode_status"))
        .respond_with(ok(json!({"status": "expired"})))
        .mount(&env.server)
        .await;
    call(&env, web(), "bind", json!({})).await.unwrap();
    assert!(!env.sh.bind_poll_once(1).await);
    let r = call(&env, web(), "bind_status", json!({})).await.unwrap();
    assert!(r["detail"].as_str().unwrap().contains("过期"));
    assert!(env.secrets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bind_and_status_forbidden_for_non_web() {
    let env = setup().await;
    for m in ["bind", "bind_status"] {
        for who in ["dispatch", "test", "wechat-ilink"] {
            let e = call(&env, Caller::owner(who), m, json!({}))
                .await
                .unwrap_err();
            assert_eq!(e.code, ServiceError::FORBIDDEN, "{m} from {who}");
        }
    }
    // 没发出任何请求
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn operations_can_bind_and_poll_without_exposing_credentials() {
    let env = setup().await;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/get_bot_qrcode"))
        .respond_with(ok(
            json!({"qrcode":"operations-qr","qrcode_img_content":"https://x/operations-qr"}),
        ))
        .mount(&env.server)
        .await;
    let result = call(&env, Caller::owner("operations"), "bind", json!({}))
        .await
        .unwrap();
    assert_eq!(result["qrcode"], "https://x/operations-qr");
    assert!(
        call(&env, Caller::owner("operations"), "bind_status", json!({}))
            .await
            .is_ok()
    );
    let mut guest = Caller::owner("operations");
    guest.user_id = "guest".into();
    assert_eq!(
        call(&env, guest, "bind_status", json!({}))
            .await
            .unwrap_err()
            .code,
        ServiceError::FORBIDDEN
    );
    assert_eq!(env.server.received_requests().await.unwrap().len(), 1);
}

// ------------------------------------------------------------ 收消息

#[tokio::test]
async fn user_message_saves_context_resets_quota_and_publishes() {
    let env = setup().await;
    seed(&env, 0, None);
    env.sh.rt.lock().unwrap().session = None;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ok(json!({
            "ret": 0, "get_updates_buf": "cur-2",
            "msgs": [{
                "message_id": 7, "from_user_id": "user-1", "message_type": 1,
                "context_token": CTX_TOKEN,
                "item_list": [{"type": 1, "text_item": {"text": "你好"}}]
            }]
        })))
        .mount(&env.server)
        .await;
    env.sh.refresh_status().await;
    assert_eq!(env.hub.published(CHANNEL_STATUS).len(), 1);

    assert!(matches!(env.sh.poll_once().await, Polled::Messages(1)));

    let ev = env.hub.published(USER_MESSAGE);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].payload["text"], "你好");
    assert_eq!(ev[0].payload["channel"], "wechat-ilink");
    assert_eq!(ev[0].payload["conversation"], "user-1");
    assert_eq!(ev[0].payload["message_id"], "7");
    assert!(env.secrets.lock().unwrap().contains_key(KEY_CONTEXT));
    {
        let rt = env.sh.rt.lock().unwrap();
        assert_eq!(rt.quota_left, 10);
        assert_eq!(rt.cursor, "cur-2");
    }
    let st = env.hub.published(CHANNEL_STATUS);
    assert_eq!(st.len(), 2);
    assert_eq!(st[1].payload["state"], "ok");
    assert_eq!(st[1].payload["quota_left"], 10);
    assert!(env.sh.info().last_received_at.is_some());
    assert_eq!(env.sh.info().quota_left, Some(10));
    assert_eq!(env.sh.info().quota_limit, Some(10));
    // 再轮询一次状态没变，不重复发
    env.sh.refresh_status().await;
    assert_eq!(env.hub.published(CHANNEL_STATUS).len(), 2);
}

#[tokio::test]
async fn consecutive_receive_failures_go_offline_then_recover() {
    let env = setup().await;
    seed(&env, 5, recent());
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(3)
        .mount(&env.server)
        .await;
    env.sh.refresh_status().await;
    for _ in 0..3 {
        assert!(matches!(env.sh.poll_once().await, Polled::Failed));
    }
    assert_eq!(env.sh.info().state, ChannelState::Offline);
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ok(json!({"ret": 0, "get_updates_buf": "", "msgs": []})))
        .mount(&env.server)
        .await;
    assert!(matches!(env.sh.poll_once().await, Polled::Messages(0)));
    assert_eq!(env.sh.info().state, ChannelState::Ok);
    let states: Vec<_> = env
        .hub
        .published(CHANNEL_STATUS)
        .iter()
        .map(|e| e.payload["state"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, ["ok", "offline", "ok"]);
}

// ------------------------------------------------------------ send

#[tokio::test]
async fn send_decrements_quota() {
    let env = setup().await;
    seed(&env, 5, recent());
    mock_send(&env, ok(json!({"ret": 0, "message_id": "m1"}))).await;
    let r = call(
        &env,
        Caller::owner("dispatch"),
        "send",
        send_params("hello"),
    )
    .await
    .unwrap();
    assert_eq!(r["quota_left"], 4);
    assert_eq!(r["message_id"], "m1");
    assert_eq!(sent_texts(&env).await, ["hello"]);
    assert_eq!(env.sh.rt.lock().unwrap().quota_left, 4);
}

#[tokio::test]
async fn send_appends_reminder_when_quota_is_one() {
    let env = setup().await;
    seed(&env, 1, recent());
    mock_send(&env, ok(json!({"ret": 0}))).await;
    let r = call(
        &env,
        Caller::owner("dispatch"),
        "send",
        send_params("hello"),
    )
    .await
    .unwrap();
    assert_eq!(r["quota_left"], 0);
    let texts = sent_texts(&env).await;
    assert!(texts[0].starts_with("hello"));
    assert!(texts[0].ends_with(QUOTA_REMINDER));
    // 额度用完后状态变化
    assert_eq!(env.sh.info().state, ChannelState::QuotaExhausted);
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("x"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::QUOTA_EXHAUSTED);
    assert_eq!(sent_texts(&env).await.len(), 1);
}

#[tokio::test]
async fn send_skips_reminder_when_text_too_long() {
    let env = setup().await;
    seed(&env, 1, recent());
    mock_send(&env, ok(json!({"ret": 0}))).await;
    let text = "a".repeat(MAX_BYTES - 5);
    call(&env, Caller::owner("dispatch"), "send", send_params(&text))
        .await
        .unwrap();
    assert_eq!(sent_texts(&env).await, [text]);
}

#[tokio::test]
async fn send_rejects_oversize_text() {
    let env = setup().await;
    seed(&env, 5, recent());
    let text = "a".repeat(MAX_BYTES + 1);
    let e = call(&env, Caller::owner("dispatch"), "send", send_params(&text))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    assert_eq!(env.sh.rt.lock().unwrap().quota_left, 5);
}

#[tokio::test]
async fn send_with_zero_quota_is_quota_exhausted() {
    let env = setup().await;
    seed(&env, 0, recent());
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::QUOTA_EXHAUSTED);
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn send_errcode_minus_2_zeroes_quota() {
    let env = setup().await;
    seed(&env, 5, recent());
    mock_send(&env, ok(json!({"ret": -2, "errmsg": "quota"}))).await;
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::QUOTA_EXHAUSTED);
    assert_eq!(env.sh.rt.lock().unwrap().quota_left, 0);
    assert_eq!(env.sh.info().state, ChannelState::QuotaExhausted);
}

#[tokio::test]
async fn send_with_expired_session_is_credential_expired() {
    let env = setup().await;
    seed(
        &env,
        5,
        Some(OffsetDateTime::now_utc() - Duration::hours(30)),
    );
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::CREDENTIAL_EXPIRED);
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn send_unbound_is_credential_expired() {
    let env = setup().await;
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::CREDENTIAL_EXPIRED);
}

#[tokio::test]
async fn send_errcode_minus_14_marks_expired() {
    let env = setup().await;
    seed(&env, 5, recent());
    mock_send(&env, ok(json!({"ret": -14, "errmsg": "expired"}))).await;
    let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::CREDENTIAL_EXPIRED);
    assert_eq!(env.sh.info().state, ChannelState::Expired);
}

#[tokio::test]
async fn secrets_never_leak_into_errors_or_info() {
    let env = setup().await;
    seed(&env, 5, recent());
    let cases = [
        ok(json!({"ret": -14, "errmsg": format!("{TOKEN} {CTX_TOKEN}")})),
        ok(json!({"ret": 9, "errmsg": format!("{TOKEN} {CTX_TOKEN}")})),
        ResponseTemplate::new(502).set_body_string(format!("{TOKEN} {CTX_TOKEN}")),
    ];
    for resp in cases {
        env.server.reset().await;
        mock_send(&env, resp).await;
        seed(&env, 5, recent());
        let e = call(&env, Caller::owner("dispatch"), "send", send_params("hi"))
            .await
            .unwrap_err();
        let shown = format!("{e:?} {}", e.message);
        assert!(
            !shown.contains(TOKEN) && !shown.contains(CTX_TOKEN),
            "{shown}"
        );
        let info = serde_json::to_string(&env.sh.info()).unwrap();
        assert!(!info.contains(TOKEN) && !info.contains(CTX_TOKEN));
    }
}

#[tokio::test]
async fn info_reports_channel_fields() {
    let env = setup().await;
    seed(&env, 3, Some(OffsetDateTime::now_utc()));
    let r = call(&env, Caller::owner("dispatch"), "info", json!({}))
        .await
        .unwrap();
    assert_eq!(r["id"], "wechat-ilink");
    assert_eq!(r["format"], "plain");
    assert_eq!(r["max_bytes"], MAX_BYTES);
    assert_eq!(r["backup"], false);
    assert_eq!(r["state"], "ok");
    assert_eq!(r["quota_left"], 3);
    assert!(r["expires_at"].is_string());
    let _ = format_rfc3339(OffsetDateTime::now_utc());
}

#[tokio::test]
async fn state_survives_restart() {
    let env = setup().await;
    seed(&env, 7, recent());
    env.sh.persist().await;
    env.secrets.lock().unwrap().insert(
        KEY_CREDENTIALS.to_string(),
        json!({"token": TOKEN, "baseUrl": "http://x.invalid", "accountId": "a", "userId": "u"})
            .to_string(),
    );
    env.secrets.lock().unwrap().insert(
        KEY_CONTEXT.to_string(),
        json!({"account_key": "a", "user_id": "user-1", "context_token": CTX_TOKEN,
               "observed_at_unix_ms": 1})
        .to_string(),
    );
    let ctx = env
        .hub
        .context("wechat-ilink", json!(null), test_http_client());
    let sh2 = init(&ctx).await.unwrap();
    assert_eq!(sh2.info().state, ChannelState::Ok);
    assert_eq!(sh2.info().quota_left, Some(7));
}

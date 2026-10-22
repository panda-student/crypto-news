use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plugin_sdk::contracts::llm::{
    Assignment, ChatMessage, ChatRequest, LlmClient, LlmDelta, ProviderInfo, ProviderInput,
    StreamChatRequest, TestRequest, Tier, UsageQuery, UsageRow,
};
use plugin_sdk::contracts::now_rfc3339;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Context, Plugin, ServiceError};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::LlmPlugin;

const KEY: &str = "sk-test-secret-123";

type Secrets = Arc<Mutex<BTreeMap<String, String>>>;

struct Env {
    hub: Arc<TestHub>,
    ctx: Context,
    client: LlmClient,
    secrets: Secrets,
}

async fn setup(config: Value) -> Env {
    setup_with(config, &[]).await
}

/// 内存里的假 secrets 服务；`seed` 是启动前已有的密钥。
async fn setup_with(config: Value, seed: &[(&str, &str)]) -> Env {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let secrets: Secrets = Arc::new(Mutex::new(
        seed.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    ));
    let s = secrets.clone();
    hub.register_fn("secrets", "secrets", move |m, p| {
        let s = s.clone();
        async move {
            let mut map = s.lock().unwrap();
            let key = p["key"].as_str().unwrap_or("").to_string();
            // 与真实 secrets 的键名规则一致：<插件名>.<字段名>，字段名可含点，各段非空
            let seg = |s: &str| {
                !s.is_empty()
                    && s.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-'
                    })
            };
            if m != "list" {
                let ok = key
                    .split_once('.')
                    .is_some_and(|(p, f)| seg(p) && f.split('.').all(seg));
                if !ok {
                    return Err(ServiceError::invalid_params("键名格式不对"));
                }
            }
            match m.as_str() {
                "get" => Ok(json!({ "value": map.get(&key) })),
                "set" => {
                    map.insert(key, p["value"].as_str().unwrap_or("").to_string());
                    Ok(json!({}))
                }
                "delete" => {
                    map.remove(&key);
                    Ok(json!({}))
                }
                "list" => Ok(json!({
                    "secrets": map.keys().map(|k| json!({"key": k, "configured": true})).collect::<Vec<_>>()
                })),
                _ => panic!("unexpected secrets method {m}"),
            }
        }
    });
    let ctx = hub.context("llm", config, test_http_client());
    LlmPlugin.start(ctx.clone()).await.unwrap();
    let client = LlmClient(ctx.service("llm"));
    Env {
        hub,
        ctx,
        client,
        secrets,
    }
}

impl Env {
    /// 新建一个服务（密钥为 KEY）。
    async fn add(&self, name: &str, kind: &str, base_url: &str, models: &[&str]) -> ProviderInfo {
        self.client
            .provider_save(&ProviderInput {
                id: None,
                name: name.into(),
                kind: kind.into(),
                base_url: base_url.into(),
                models: models.iter().map(|m| m.to_string()).collect(),
                api_key: Some(KEY.into()),
            })
            .await
            .unwrap()
    }

    async fn openai(&self, server: &MockServer) -> ProviderInfo {
        self.add(
            "测试",
            "openai",
            &format!("{}/v1", server.uri()),
            &["m-small", "m-large"],
        )
        .await
    }

    fn secret(&self, id: &str) -> Option<String> {
        self.secrets
            .lock()
            .unwrap()
            .get(&format!("llm.key.{id}"))
            .cloned()
    }
}

fn req(json_mode: bool) -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::system("规则"), ChatMessage::user("你好")],
        tier: Tier::Small,
        json: json_mode,
        max_tokens: None,
        temperature: None,
        purpose: Some("screen".into()),
    }
}

fn sreq(json_mode: bool, id: &str) -> StreamChatRequest {
    StreamChatRequest {
        request: req(json_mode),
        stream_id: id.into(),
    }
}

fn openai_ok(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "m-small-1",
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    }))
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

fn openai_sse() -> ResponseTemplate {
    sse(concat!(
        "data: {\"model\":\"m-small-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"吗\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        "data: [DONE]\n\n",
    ))
}

fn deltas(env: &Env, id: &str) -> Vec<String> {
    env.hub
        .published("llm.delta")
        .iter()
        .filter_map(|e| e.parse::<LlmDelta>().ok())
        .filter(|d| d.stream_id == id)
        .map(|d| d.text)
        .collect()
}

async fn rows(env: &Env) -> Vec<UsageRow> {
    env.client.usage(&UsageQuery::default()).await.unwrap()
}

fn assignment(purpose: &str, id: &str, model: &str) -> Assignment {
    Assignment {
        purpose: purpose.into(),
        provider_id: id.into(),
        model: model.into(),
    }
}

fn edit(p: &ProviderInfo, models: &[&str], api_key: Option<&str>) -> ProviderInput {
    ProviderInput {
        id: Some(p.id.clone()),
        name: p.name.clone(),
        kind: p.kind.clone(),
        base_url: p.base_url.clone(),
        models: models.iter().map(|m| m.to_string()).collect(),
        api_key: api_key.map(str::to_string),
    }
}

#[tokio::test]
async fn manifest_is_valid() {
    plugin_sdk::Manifest::parse(LlmPlugin.manifest()).unwrap();
}

#[tokio::test]
async fn openai_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(json!({
            "model": "m-small",
            "response_format": {"type": "json_object"},
            "messages": [{"role": "system", "content": "规则"}, {"role": "user", "content": "你好"}]
        })))
        .respond_with(openai_ok("{\"a\":1}"))
        .expect(1)
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let r = env.client.chat(&req(true)).await.unwrap();
    assert_eq!(r.content, "{\"a\":1}");
    assert_eq!(r.model, "m-small-1");
    assert_eq!(r.usage.input_tokens, 10);
    assert_eq!(r.usage.output_tokens, 5);
}

#[tokio::test]
async fn tier_picks_model_with_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"model": "m-large"})))
        .respond_with(openai_ok("large"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"model": "m-small"})))
        .respond_with(openai_ok("small"))
        .expect(2)
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    let p = env.openai(&server).await;
    let mut r = req(false);
    r.tier = Tier::Large;
    assert_eq!(env.client.chat(&r).await.unwrap().content, "large");
    r.tier = Tier::Small;
    assert_eq!(env.client.chat(&r).await.unwrap().content, "small");

    // 只有一个模型时，大档位回退到第一个
    env.client
        .provider_save(&edit(&p, &["m-small"], None))
        .await
        .unwrap();
    r.tier = Tier::Large;
    assert_eq!(env.client.chat(&r).await.unwrap().content, "small");
}

#[tokio::test]
async fn anthropic_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({
            "model": "m-small",
            "max_tokens": 2048,
            "system": "规则\n\n只输出一个 JSON 对象，不要其他文字",
            "messages": [{"role": "user", "content": "你好"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "m-small",
            "content": [{"type": "text", "text": "{\"a\":"}, {"type": "tool_use"}, {"type": "text", "text": "2}"}],
            "usage": {"input_tokens": 7, "output_tokens": 3}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.add("A", "anthropic", &server.uri(), &["m-small"]).await;
    let r = env.client.chat(&req(true)).await.unwrap();
    assert_eq!(r.content, "{\"a\":2}");
    assert_eq!(r.usage.input_tokens, 7);
    assert_eq!(r.usage.output_tokens, 3);
}

#[tokio::test]
async fn json_mode_strips_fence() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(openai_ok("```json\n{\"a\": 1}\n```\n"))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let r = env.client.chat(&req(true)).await.unwrap();
    assert_eq!(r.content, "{\"a\": 1}");
    // 非 json 模式原样返回
    let r = env.client.chat(&req(false)).await.unwrap();
    assert!(r.content.starts_with("```json"));
}

#[tokio::test]
async fn json_mode_rejects_non_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(openai_ok("今天天气不错"))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let e = env.client.chat(&req(true)).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(e.message.contains("不是 JSON"));
    // 失败也记账，并带上已消耗的 token
    let all = rows(&env).await;
    assert_eq!(all[0].failures, 1);
    assert_eq!(all[0].input_tokens, 10);
}

#[tokio::test]
async fn error_mapping_and_no_key_leak() {
    let cases: [(u16, &str, i64); 8] = [
        (429, "slow down", ServiceError::RATE_LIMITED),
        (401, "bad key", ServiceError::CREDENTIAL_EXPIRED),
        (403, "forbidden", ServiceError::CREDENTIAL_EXPIRED),
        (402, "pay", ServiceError::QUOTA_EXHAUSTED),
        (
            400,
            "{\"error\":{\"code\":\"insufficient_quota\"}}",
            ServiceError::QUOTA_EXHAUSTED,
        ),
        (500, "boom", ServiceError::UNAVAILABLE),
        (503, "down", ServiceError::UNAVAILABLE),
        (404, "nope", ServiceError::UNAVAILABLE),
    ];
    for kind in ["openai", "anthropic"] {
        for (status, body, code) in cases {
            let server = MockServer::start().await;
            // 响应体里故意回显密钥
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_string(format!("{body}\n{KEY}")),
                )
                .mount(&server)
                .await;
            let env = setup(json!({})).await;
            env.add("E", kind, &server.uri(), &["m"]).await;
            let e = env.client.chat(&req(false)).await.unwrap_err();
            assert_eq!(e.code, code, "{kind} {status}");
            assert!(!e.message.contains(KEY), "{kind} {status}: {}", e.message);
            assert!(!e.message.contains('\n'));
            let e = env.client.chat_stream(&sreq(false, "s")).await.unwrap_err();
            assert_eq!(e.code, code, "{kind} {status} stream");
            assert!(!e.message.contains(KEY), "{kind} {status}: {}", e.message);
            assert!(!e.message.contains('\n'));
        }
    }
}

#[tokio::test]
async fn long_body_truncated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(1000)))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let e = env.client.chat(&req(false)).await.unwrap_err();
    assert!(e.message.chars().count() < 260);
}

#[tokio::test]
async fn network_error_is_unavailable() {
    // 占着端口、接受后立刻断开，避免释放的端口被并行的其他测试抢走
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let acceptor = tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    let env = setup(json!({})).await;
    env.add("N", "openai", &format!("{uri}/v1"), &["m"]).await;
    let e = env.client.chat(&req(false)).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(!e.message.contains("127.0.0.1"));
    let e = env.client.chat_stream(&sreq(false, "s")).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(!e.message.contains("127.0.0.1"));
    acceptor.abort();
}

#[tokio::test]
async fn no_provider_and_missing_key() {
    let env = setup(json!({})).await;
    let e = env.client.chat(&req(false)).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(e.message.contains("还没有配置模型服务"));
    assert!(!env.client.ready().await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(openai_ok("{}"))
        .expect(0)
        .mount(&server)
        .await;
    let p = env.openai(&server).await;
    assert!(env.client.ready().await.unwrap());
    // 密钥丢失
    env.secrets
        .lock()
        .unwrap()
        .remove(&format!("llm.key.{}", p.id));
    let e = env.client.chat(&req(false)).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);
    assert!(e.message.contains("未配置模型密钥"));
    assert!(!env.client.ready().await.unwrap());
    assert!(!env.client.providers().await.unwrap().providers[0].has_key);
}

#[tokio::test]
async fn usage_accumulates_and_filters() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(openai_ok("ok"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let day_before = now_rfc3339()[..10].to_string();

    env.client.chat(&req(false)).await.unwrap();
    env.client.chat(&req(false)).await.unwrap();
    // 第三次落到 500
    let e = env.client.chat(&req(false)).await.unwrap_err();
    assert_eq!(e.code, ServiceError::UNAVAILABLE);

    let all = rows(&env).await;
    assert_eq!(all.len(), 2);
    // 成功的响应里 model 是 m-small-1，失败的记请求的 m-small
    let ok = all.iter().find(|r| r.model == "m-small-1").unwrap();
    let bad = all.iter().find(|r| r.model == "m-small").unwrap();
    let day_after = now_rfc3339()[..10].to_string();
    // 允许测试恰好跨过 UTC 零点
    assert!(ok.day == day_before || ok.day == day_after);
    assert_eq!(ok.purpose, "screen");
    assert_eq!((ok.calls, ok.failures), (2, 0));
    assert_eq!((ok.input_tokens, ok.output_tokens), (20, 10));
    assert_eq!((bad.calls, bad.failures), (1, 1));
    assert_eq!((bad.input_tokens, bad.output_tokens), (0, 0));

    // 缺省用途为 other，同一键累加
    let mut other = req(false);
    other.purpose = None;
    let _ = env.client.chat(&other).await;
    let _ = env.client.chat(&other).await;
    let all = rows(&env).await;
    let o = all.iter().find(|r| r.purpose == "other").unwrap();
    assert_eq!((o.calls, o.failures), (2, 2));

    // 日期过滤：since 含、until 不含
    let count = |since: Option<String>, until: Option<String>| {
        let c = env.client.clone();
        async move { c.usage(&UsageQuery { since, until }).await.unwrap().len() }
    };
    let t0 = format!("{}T00:00:00Z", ok.day);
    assert_eq!(count(Some(t0.clone()), None).await, 3);
    assert_eq!(count(Some("2999-01-01T00:00:00Z".into()), None).await, 0);
    assert_eq!(count(None, Some(t0)).await, 0);
    assert_eq!(count(None, Some("2999-01-01T00:00:00Z".into())).await, 3);
    // 时间格式错误
    let e = env
        .client
        .usage(&UsageQuery {
            since: Some("昨天".into()),
            until: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
}

#[tokio::test]
async fn usage_sorted_by_day_desc() {
    let env = setup(json!({})).await;
    for (day, model) in [
        ("2026-01-01", "a"),
        ("2026-01-03", "a"),
        ("2026-01-02", "b"),
    ] {
        env.ctx
            .store()
            .execute(
                "INSERT INTO usage VALUES (?, ?, 'x', 1, 1, 1, 0)",
                vec![json!(day), json!(model)],
            )
            .await
            .unwrap();
    }
    let days: Vec<_> = rows(&env).await.into_iter().map(|r| r.day).collect();
    assert_eq!(days, ["2026-01-03", "2026-01-02", "2026-01-01"]);
}

#[tokio::test]
async fn unknown_method() {
    let env = setup(json!({})).await;
    let e = env.hub.call("llm", "nope", json!({})).await.unwrap_err();
    assert_eq!(e.code, ServiceError::METHOD_NOT_FOUND);
}

// ---- 流式 ----

#[tokio::test]
async fn openai_stream_emits_deltas() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(json!({
            "model": "m-small",
            "stream": true,
            "stream_options": {"include_usage": true}
        })))
        .respond_with(openai_sse())
        .expect(1)
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let r = env.client.chat_stream(&sreq(false, "s1")).await.unwrap();
    assert!(!r.cancelled);
    assert_eq!(r.response.content, "你好吗");
    assert_eq!(r.response.model, "m-small-1");
    assert_eq!(r.response.usage.input_tokens, 10);
    assert_eq!(r.response.usage.output_tokens, 5);
    assert_eq!(deltas(&env, "s1"), ["你", "好", "吗"]);
    // 用量记入
    let all = rows(&env).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].model, "m-small-1");
    assert_eq!(all[0].purpose, "screen");
    assert_eq!(
        (all[0].calls, all[0].input_tokens, all[0].output_tokens),
        (1, 10, 5)
    );
}

#[tokio::test]
async fn anthropic_stream_emits_deltas() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", KEY))
        .and(body_partial_json(json!({"model": "m-small", "stream": true})))
        .respond_with(sse(concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"m-small\",\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"He\"}}\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"llo\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )))
        .expect(1)
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.add("A", "anthropic", &server.uri(), &["m-small"]).await;
    let r = env.client.chat_stream(&sreq(false, "a1")).await.unwrap();
    assert!(!r.cancelled);
    assert_eq!(r.response.content, "Hello");
    assert_eq!(r.response.model, "m-small");
    assert_eq!(r.response.usage.input_tokens, 7);
    assert_eq!(r.response.usage.output_tokens, 3);
    assert_eq!(deltas(&env, "a1"), ["He", "llo"]);
    let all = rows(&env).await;
    assert_eq!(
        (all[0].calls, all[0].input_tokens, all[0].output_tokens),
        (1, 7, 3)
    );
}

#[tokio::test]
async fn stream_falls_back_when_not_sse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(openai_ok("整段"))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let r = env.client.chat_stream(&sreq(false, "f1")).await.unwrap();
    assert_eq!(r.response.content, "整段");
    assert_eq!(r.response.usage.input_tokens, 10);
    assert_eq!(deltas(&env, "f1"), ["整段"]);
}

#[tokio::test]
async fn stream_json_mode_cleans_final_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"```json\\n{\\\"a\\\":\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"1}\\n```\"}}]}\n\n",
            "data: [DONE]\n\n",
        )))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let r = env.client.chat_stream(&sreq(true, "j1")).await.unwrap();
    assert_eq!(r.response.content, "{\"a\":1}");
}

#[tokio::test]
async fn stream_error_event_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"半\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"overloaded\",\"code\":\"insufficient_quota\"}}\n\n",
        )))
        .mount(&server)
        .await;
    let env = setup(json!({})).await;
    env.openai(&server).await;
    let e = env
        .client
        .chat_stream(&sreq(false, "e1"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::QUOTA_EXHAUSTED);
    assert_eq!(rows(&env).await[0].failures, 1);
}

/// 起一个只发一段 SSE 就挂住的服务，直到客户端断开。
async fn hanging_sse_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let chunk =
            "data: {\"model\":\"m-small-1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        let body = format!("{:x}\r\n{chunk}\r\n", chunk.len());
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        // 等客户端断开
        let _ = sock.read(&mut buf).await;
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn cancel_mid_stream_returns_partial() {
    let base = hanging_sse_server().await;
    let env = setup(json!({})).await;
    env.add("C", "openai", &base, &["m-small"]).await;
    let client = env.client.clone();
    let task = tokio::spawn(async move { client.chat_stream(&sreq(false, "c1")).await });

    // 等第一个片段发出
    let mut waited = 0;
    while deltas(&env, "c1").is_empty() {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
        assert!(waited < 250, "没有等到 llm.delta");
    }
    // 别的流的取消不影响它
    env.hub
        .deliver("llm.cancel", json!({"stream_id": "other"}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!task.is_finished());
    env.hub
        .deliver("llm.cancel", json!({"stream_id": "c1"}))
        .await
        .unwrap();
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("取消后应当返回")
        .unwrap()
        .unwrap();
    assert!(r.cancelled);
    assert_eq!(r.response.content, "Hel");
    assert_eq!(r.response.model, "m-small-1");
    assert_eq!(deltas(&env, "c1"), ["Hel"]);
    // 被取消的调用算一次成功调用
    let all = rows(&env).await;
    assert_eq!((all[0].calls, all[0].failures), (1, 0));
}

// ---- 服务管理 ----

#[tokio::test]
async fn provider_crud_default_and_assign() {
    let env = setup(json!({})).await;
    let a = env
        .add("甲", "openai", "https://a.example/v1", &["a1", "a2"])
        .await;
    assert!(a.is_default && a.has_key);
    assert_eq!(a.models, ["a1", "a2"]);
    let b = env
        .add("乙", "anthropic", "https://b.example", &["b1"])
        .await;
    assert!(!b.is_default && b.has_key);

    // 响应里没有密钥
    let list = env.client.providers().await.unwrap();
    assert_eq!(list.providers.len(), 2);
    assert!(!serde_json::to_string(&list).unwrap().contains(KEY));
    assert!(!serde_json::to_string(&a).unwrap().contains(KEY));
    // 密钥存在 secrets 里，键 llm.key.<id>
    assert_eq!(env.secret(&a.id).as_deref(), Some(KEY));

    // 修改：api_key=None 不改密钥
    let edited = env
        .client
        .provider_save(&ProviderInput {
            name: "乙2".into(),
            base_url: "https://b2.example".into(),
            ..edit(&b, &["b1", "b2"], None)
        })
        .await
        .unwrap();
    assert_eq!(edited.name, "乙2");
    assert_eq!(edited.models, ["b1", "b2"]);
    assert!(edited.has_key);
    assert_eq!(env.secret(&b.id).as_deref(), Some(KEY));
    // 传新密钥则覆盖
    env.client
        .provider_save(&edit(&b, &["b1", "b2"], Some("new-key")))
        .await
        .unwrap();
    assert_eq!(env.secret(&b.id).as_deref(), Some("new-key"));

    // 校验
    let base = ProviderInput {
        id: None,
        name: "丙".into(),
        kind: "openai".into(),
        base_url: "https://c.example".into(),
        models: vec!["c1".into()],
        api_key: Some("k".into()),
    };
    for bad in [
        ProviderInput {
            api_key: None,
            ..base.clone()
        },
        ProviderInput {
            kind: "gemini".into(),
            ..base.clone()
        },
        ProviderInput {
            base_url: "ftp://c.example".into(),
            ..base.clone()
        },
        ProviderInput {
            models: vec![],
            ..base.clone()
        },
        ProviderInput {
            models: vec!["  ".into()],
            ..base.clone()
        },
        ProviderInput {
            name: " ".into(),
            ..base.clone()
        },
    ] {
        let e = env.client.provider_save(&bad).await.unwrap_err();
        assert_eq!(e.code, ServiceError::INVALID_PARAMS, "{bad:?}");
    }
    assert_eq!(env.client.providers().await.unwrap().providers.len(), 2);

    // 分配：模型必须在该服务里
    let e = env
        .client
        .assign(&assignment("screen", &a.id, "b1"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    let e = env
        .client
        .assign(&assignment("nope", &a.id, "a1"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    assert!(env
        .client
        .assign(&assignment("screen", "no-such", "a1"))
        .await
        .is_err());
    env.client
        .assign(&assignment("screen", &b.id, "b2"))
        .await
        .unwrap();
    env.client
        .assign(&assignment("screen", &a.id, "a2"))
        .await
        .unwrap();
    env.client
        .assign(&assignment("agent", &b.id, "b1"))
        .await
        .unwrap();
    let list = env.client.providers().await.unwrap();
    assert_eq!(list.assignments.len(), 2);
    let screen = list
        .assignments
        .iter()
        .find(|x| x.purpose == "screen")
        .unwrap();
    assert_eq!(
        (screen.provider_id.as_str(), screen.model.as_str()),
        (a.id.as_str(), "a2")
    );

    // 不能删默认服务
    let e = env.client.provider_delete(&a.id).await.unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    assert!(e.message.contains("默认"));
    // 缩小模型列表会清掉指向被删模型的分配
    env.client
        .provider_save(&ProviderInput {
            name: "乙2".into(),
            base_url: "https://b2.example".into(),
            ..edit(&b, &["b2"], None)
        })
        .await
        .unwrap();
    assert_eq!(env.client.providers().await.unwrap().assignments.len(), 1);
    env.client
        .assign(&assignment("agent", &b.id, "b2"))
        .await
        .unwrap();

    // 换默认后可删旧默认；删除带走密钥和分配
    env.client.set_default(&b.id).await.unwrap();
    let list = env.client.providers().await.unwrap();
    assert!(
        list.providers
            .iter()
            .find(|p| p.id == b.id)
            .unwrap()
            .is_default
    );
    assert!(
        !list
            .providers
            .iter()
            .find(|p| p.id == a.id)
            .unwrap()
            .is_default
    );
    env.client.provider_delete(&a.id).await.unwrap();
    let list = env.client.providers().await.unwrap();
    assert_eq!(list.providers.len(), 1);
    assert!(list.assignments.iter().all(|x| x.provider_id == b.id));
    assert_eq!(list.assignments.len(), 1);
    assert!(env.secret(&a.id).is_none());
    // 不存在的 id
    assert!(env.client.set_default("no-such").await.is_err());
    assert!(env.client.provider_delete("no-such").await.is_err());
}

#[tokio::test]
async fn assignment_routes_to_provider_and_model() {
    let sa = MockServer::start().await;
    let sb = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "a-small"})))
        .respond_with(openai_ok("from-a"))
        .expect(2)
        .mount(&sa)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(json!({"model": "b-big"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "b-big",
            "content": [{"type": "text", "text": "from-b"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(2)
        .mount(&sb)
        .await;
    let env = setup(json!({})).await;
    env.add(
        "A",
        "openai",
        &format!("{}/v1", sa.uri()),
        &["a-small", "a-large"],
    )
    .await;
    let b = env
        .add("B", "anthropic", &sb.uri(), &["b-small", "b-big"])
        .await;
    env.client
        .assign(&assignment("analyze", &b.id, "b-big"))
        .await
        .unwrap();

    let with = |purpose: Option<&str>, tier: Tier| {
        let mut r = req(false);
        r.purpose = purpose.map(str::to_string);
        r.tier = tier;
        r
    };
    // 有分配：用分配的服务和模型，无视档位
    for tier in [Tier::Small, Tier::Large] {
        let r = env.client.chat(&with(Some("analyze"), tier)).await.unwrap();
        assert_eq!(r.content, "from-b");
    }
    // 没分配的用途和无用途：走默认服务 A 的 models[0]
    for purpose in [Some("screen"), None] {
        let r = env.client.chat(&with(purpose, Tier::Small)).await.unwrap();
        assert_eq!(r.content, "from-a");
    }
    // 换默认服务后，没分配的用途跟着走（B 的 b-small 没有 mock，应失败）
    env.client.set_default(&b.id).await.unwrap();
    assert!(env
        .client
        .chat(&with(Some("screen"), Tier::Small))
        .await
        .is_err());
}

#[tokio::test]
async fn legacy_config_migrates_once() {
    let cfg = json!({
        "provider": "anthropic",
        "base_url": "https://legacy.example",
        "small_model": "l-small",
        "large_model": "l-large",
    });
    let env = setup_with(cfg, &[("llm.api_key", KEY)]).await;
    let list = env.client.providers().await.unwrap();
    assert_eq!(list.providers.len(), 1);
    let p = &list.providers[0];
    assert_eq!(p.name, "默认服务");
    assert_eq!(p.kind, "anthropic");
    assert_eq!(p.base_url, "https://legacy.example");
    assert_eq!(p.models, ["l-small", "l-large"]);
    assert!(p.is_default && p.has_key);
    // 密钥复制到新键
    assert_eq!(env.secret(&p.id).as_deref(), Some(KEY));
    let by = |purpose: &str| {
        list.assignments
            .iter()
            .find(|a| a.purpose == purpose)
            .map(|a| a.model.clone())
    };
    assert_eq!(by("screen").as_deref(), Some("l-small"));
    assert_eq!(by("analyze").as_deref(), Some("l-large"));
    assert_eq!(by("agent").as_deref(), Some("l-large"));
    assert_eq!(list.assignments.len(), 3);
    assert!(env.client.ready().await.unwrap());

    // 再导入一次不会重复（表非空）
    let again = crate::Config {
        base_url: "https://other.example".into(),
        api_key: Some("another".into()),
        ..Default::default()
    };
    crate::providers::migrate_legacy(&env.ctx, &again)
        .await
        .unwrap();
    assert_eq!(env.client.providers().await.unwrap().providers.len(), 1);
}

#[tokio::test]
async fn legacy_config_with_inline_key_and_no_large() {
    let cfg =
        json!({"base_url": "https://legacy.example/v1", "small_model": "only", "api_key": KEY});
    let env = setup(cfg).await;
    let list = env.client.providers().await.unwrap();
    assert_eq!(list.providers.len(), 1);
    assert_eq!(list.providers[0].models, ["only"]);
    assert!(list.assignments.is_empty());
    assert!(list.providers[0].has_key);
}

#[tokio::test]
async fn legacy_config_without_key_does_not_migrate() {
    let cfg = json!({"base_url": "https://legacy.example/v1", "small_model": "only"});
    let env = setup(cfg).await;
    assert!(env.client.providers().await.unwrap().providers.is_empty());
    assert!(!env.client.ready().await.unwrap());
}

// ---- 测试连接 ----

#[tokio::test]
async fn test_connection_success_and_failure() {
    let ok_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"model": "m-small", "max_tokens": 8}),
        ))
        .respond_with(openai_ok("pong"))
        .expect(1)
        .mount(&ok_server)
        .await;
    let bad_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!("bad {KEY}")))
        .expect(1)
        .mount(&bad_server)
        .await;
    let env = setup(json!({})).await;
    let good = env.openai(&ok_server).await;
    let bad = env.add("坏", "openai", &bad_server.uri(), &["x"]).await;
    assert!(good.last_test.is_none());

    // 缺省测默认服务
    let r = env.client.test(&TestRequest::default()).await.unwrap();
    assert!(r.ok, "{}", r.message);
    assert_eq!(r.message, "m-small-1");
    let r = env
        .client
        .test(&TestRequest {
            provider_id: Some(bad.id.clone()),
        })
        .await
        .unwrap();
    assert!(!r.ok);
    assert!(!r.message.contains(KEY));

    // 结果写回 last_test
    let list = env.client.providers().await.unwrap();
    let g = list.providers.iter().find(|p| p.id == good.id).unwrap();
    let b = list.providers.iter().find(|p| p.id == bad.id).unwrap();
    assert!(g.last_test.as_ref().unwrap().ok);
    assert!(!b.last_test.as_ref().unwrap().ok);
    assert!(!serde_json::to_string(&list).unwrap().contains(KEY));
    // 测试不记用量
    assert!(rows(&env).await.is_empty());

    // 改地址后旧结果作废
    env.client
        .provider_save(&ProviderInput {
            base_url: "https://elsewhere.example".into(),
            ..edit(&good, &["m-small", "m-large"], None)
        })
        .await
        .unwrap();
    let list = env.client.providers().await.unwrap();
    assert!(list
        .providers
        .iter()
        .find(|p| p.id == good.id)
        .unwrap()
        .last_test
        .is_none());
}

#[tokio::test]
async fn test_connection_without_provider_or_key() {
    let env = setup(json!({})).await;
    let r = env.client.test(&TestRequest::default()).await.unwrap();
    assert!(!r.ok);
    assert!(r.message.contains("还没有配置模型服务"));

    let p = env.add("X", "openai", "https://x.example", &["m"]).await;
    env.secrets.lock().unwrap().clear();
    let r = env.client.test(&TestRequest::default()).await.unwrap();
    assert!(!r.ok);
    assert!(r.message.contains("未配置模型密钥"));
    let list = env.client.providers().await.unwrap();
    let saved = list.providers.iter().find(|x| x.id == p.id).unwrap();
    assert!(!saved.last_test.as_ref().unwrap().ok);
}

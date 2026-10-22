use std::sync::Arc;

use plugin_sdk::contracts::store::SharedStore;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Caller, Context, Manifest, Plugin, ServiceError};
use serde_json::{json, Value};

use crate::{key, MemoryKeySource, SecretsPlugin, State, KEY_LEN};

const KEY_A: [u8; KEY_LEN] = [7; KEY_LEN];
const KEY_B: [u8; KEY_LEN] = [9; KEY_LEN];

async fn setup() -> (Context, State) {
    let hub = TestHub::new();
    let backend: SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let ctx = hub.context("secrets", Value::Null, test_http_client());
    SecretsPlugin::new(Arc::new(MemoryKeySource(KEY_A)))
        .start(ctx.clone())
        .await
        .unwrap();
    let state = State::new(ctx.clone(), &KEY_A);
    (ctx, state)
}

fn caller(p: &str) -> Caller {
    Caller::owner(p)
}

async fn call(s: &State, who: &str, method: &str, params: Value) -> Result<Value, ServiceError> {
    s.handle(&caller(who), method, params).await
}

fn code(r: Result<Value, ServiceError>) -> i64 {
    r.unwrap_err().code
}

#[test]
fn manifest_parses() {
    Manifest::parse(SecretsPlugin::default().manifest()).unwrap();
}

#[test]
fn hex_round_trip() {
    let b = [0u8, 1, 0xab, 0xff];
    assert_eq!(key::from_hex(&key::to_hex(&b)).unwrap(), b);
    assert!(key::from_hex("abc").is_none());
    assert!(key::from_hex("zz").is_none());
}

#[tokio::test]
async fn set_get_round_trip_and_stored_encrypted() {
    let (ctx, s) = setup().await;
    call(
        &s,
        "llm",
        "set",
        json!({"key":"llm.api_key","value":"secret-value-1"}),
    )
    .await
    .unwrap();
    let v = call(&s, "llm", "get", json!({"key":"llm.api_key"}))
        .await
        .unwrap();
    assert_eq!(v["value"], "secret-value-1");

    let rows = ctx
        .store()
        .query("SELECT nonce, ciphertext FROM secrets", vec![])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let ct = rows[0]["ciphertext"].as_str().unwrap();
    assert!(!ct.contains("secret-value-1"));
    assert!(!ct.contains(&key::to_hex(b"secret-value-1")));
}

#[tokio::test]
async fn get_missing_is_null() {
    let (_ctx, s) = setup().await;
    let v = call(&s, "llm", "get", json!({"key":"llm.none"}))
        .await
        .unwrap();
    assert!(v["value"].is_null());
}

#[tokio::test]
async fn overwrite_replaces_value() {
    let (_ctx, s) = setup().await;
    for v in ["a1", "b2"] {
        call(&s, "llm", "set", json!({"key":"llm.k","value":v}))
            .await
            .unwrap();
    }
    let v = call(&s, "llm", "get", json!({"key":"llm.k"}))
        .await
        .unwrap();
    assert_eq!(v["value"], "b2");
}

#[tokio::test]
async fn plugin_cannot_touch_other_prefix() {
    let (_ctx, s) = setup().await;
    call(&s, "llm", "set", json!({"key":"llm.k","value":"x"}))
        .await
        .unwrap();
    assert_eq!(
        code(call(&s, "screen", "get", json!({"key":"llm.k"})).await),
        -32001
    );
    assert_eq!(
        code(call(&s, "screen", "set", json!({"key":"llm.k","value":"y"})).await),
        -32001
    );
    assert_eq!(
        code(call(&s, "screen", "delete", json!({"key":"llm.k"})).await),
        -32001
    );
}

#[tokio::test]
async fn web_can_write_but_not_read() {
    let (_ctx, s) = setup().await;
    call(&s, "web", "set", json!({"key":"llm.k","value":"from-ui"}))
        .await
        .unwrap();
    assert_eq!(
        code(call(&s, "web", "get", json!({"key":"llm.k"})).await),
        -32001
    );
    let v = call(&s, "llm", "get", json!({"key":"llm.k"}))
        .await
        .unwrap();
    assert_eq!(v["value"], "from-ui");

    call(&s, "web", "delete", json!({"key":"llm.k"}))
        .await
        .unwrap();
    let v = call(&s, "llm", "get", json!({"key":"llm.k"}))
        .await
        .unwrap();
    assert!(v["value"].is_null());
}

#[tokio::test]
async fn list_filters_by_caller_and_hides_values() {
    let (_ctx, s) = setup().await;
    call(&s, "web", "set", json!({"key":"llm.k","value":"v1"}))
        .await
        .unwrap();
    call(
        &s,
        "web",
        "set",
        json!({"key":"wecom-webhook.url","value":"v2"}),
    )
    .await
    .unwrap();

    let all = call(&s, "web", "list", json!({})).await.unwrap();
    let keys: Vec<&str> = all["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["llm.k", "wecom-webhook.url"]);
    let text = all.to_string();
    assert!(!text.contains("v1") && !text.contains("v2"));
    assert_eq!(all["secrets"][0]["configured"], true);

    let own = call(&s, "llm", "list", json!({})).await.unwrap();
    assert_eq!(own["secrets"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_keys_and_values_rejected() {
    let (_ctx, s) = setup().await;
    for k in [
        "llm",
        "llm.",
        ".k",
        "LLM.k",
        "llm.a b",
        "llm..x",
        "llm.key.",
        ".llm.x",
        "llm.KEY.x",
    ] {
        assert_eq!(
            code(call(&s, "llm", "set", json!({"key":k,"value":"x"})).await),
            -32602,
            "{k}"
        );
    }
    assert_eq!(
        code(call(&s, "llm", "set", json!({"key":"llm.k","value":""})).await),
        -32602
    );
    let big = "x".repeat(8 * 1024 + 1);
    assert_eq!(
        code(call(&s, "llm", "set", json!({"key":"llm.k","value":big})).await),
        -32602
    );
}

#[tokio::test]
async fn dotted_field_names_accepted() {
    let (_ctx, s) = setup().await;
    for k in ["llm.k", "llm.key.abc", "llm.api_key"] {
        call(&s, "llm", "set", json!({"key":k,"value":"x"}))
            .await
            .unwrap();
    }
    // 归属检查不变：不能写别的插件前缀下的键
    assert!(
        call(&s, "rss", "set", json!({"key":"llm.key.abc","value":"x"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn wrong_master_key_fails_without_leaking() {
    let (ctx, s) = setup().await;
    call(
        &s,
        "llm",
        "set",
        json!({"key":"llm.k","value":"top-secret"}),
    )
    .await
    .unwrap();
    let other = State::new(ctx, &KEY_B);
    let e = other
        .handle(&caller("llm"), "get", json!({"key":"llm.k"}))
        .await
        .unwrap_err();
    assert!(e.message.contains("重新填写"));
    assert!(!e.message.contains("top-secret"));
}

#[tokio::test]
async fn ciphertext_bound_to_key_name() {
    let (ctx, s) = setup().await;
    call(&s, "llm", "set", json!({"key":"llm.a","value":"va"}))
        .await
        .unwrap();
    call(&s, "llm", "set", json!({"key":"llm.b","value":"vb"}))
        .await
        .unwrap();
    // 把 llm.a 的密文挪给 llm.b：解密应失败。
    ctx.store()
        .execute(
            "UPDATE secrets SET nonce = (SELECT nonce FROM secrets WHERE key = 'llm.a'), \
             ciphertext = (SELECT ciphertext FROM secrets WHERE key = 'llm.a') WHERE key = 'llm.b'",
            vec![],
        )
        .await
        .unwrap();
    assert!(call(&s, "llm", "get", json!({"key":"llm.b"}))
        .await
        .is_err());
}

#[tokio::test]
async fn unknown_method() {
    let (_ctx, s) = setup().await;
    assert_eq!(code(call(&s, "llm", "nope", json!({})).await), -32601);
}

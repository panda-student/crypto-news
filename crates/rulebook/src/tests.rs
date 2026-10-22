use plugin_sdk::contracts::screen::Level;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Caller, Context, Manifest, Plugin, ServiceError};
use serde_json::{json, Value};

use crate::{handle, RulebookPlugin, DEFAULT_RULEBOOK, MAX_BYTES};

async fn setup() -> Context {
    let hub = TestHub::new();
    let backend: plugin_sdk::contracts::store::SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let ctx = hub.context("rulebook", Value::Null, test_http_client());
    RulebookPlugin.start(ctx.clone()).await.unwrap();
    ctx
}

fn caller(plugin: &str) -> Caller {
    Caller::owner(plugin)
}

#[test]
fn manifest_parses() {
    Manifest::parse(RulebookPlugin.manifest()).unwrap();
}

#[tokio::test]
async fn get_returns_default_when_unsaved() {
    let ctx = setup().await;
    let v = handle(&ctx, &caller("screen"), "get", json!({}))
        .await
        .unwrap();
    assert_eq!(v["text"], DEFAULT_RULEBOOK);
    assert!(v["updated_at"].is_null());
}

#[tokio::test]
async fn web_can_set_then_get() {
    let ctx = setup().await;
    let v = handle(&ctx, &caller("web"), "set", json!({"text": "  新规则\n "}))
        .await
        .unwrap();
    assert_eq!(v["text"], "新规则");
    assert!(v["updated_at"].is_string());
    handle(&ctx, &caller("web"), "set", json!({"text": "再改"}))
        .await
        .unwrap();
    let v = handle(&ctx, &caller("screen"), "get", json!({}))
        .await
        .unwrap();
    assert_eq!(v["text"], "再改");
    assert!(v["updated_at"].is_string());
}

#[tokio::test]
async fn non_web_set_forbidden() {
    let ctx = setup().await;
    let e = handle(&ctx, &caller("screen"), "set", json!({"text": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::forbidden("").code);
    let v = handle(&ctx, &caller("web"), "get", json!({}))
        .await
        .unwrap();
    assert_eq!(v["text"], DEFAULT_RULEBOOK);
}

#[tokio::test]
async fn invalid_text_rejected() {
    let ctx = setup().await;
    let code = ServiceError::invalid_params("").code;
    for text in [
        "".to_string(),
        "  \n".to_string(),
        "a".repeat(MAX_BYTES + 1),
    ] {
        let e = handle(&ctx, &caller("web"), "set", json!({ "text": text }))
            .await
            .unwrap_err();
        assert_eq!(e.code, code);
    }
    let ok = "a".repeat(MAX_BYTES);
    handle(&ctx, &caller("web"), "set", json!({ "text": ok }))
        .await
        .unwrap();
}

#[tokio::test]
async fn unknown_method() {
    let ctx = setup().await;
    let e = handle(&ctx, &caller("web"), "nope", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::method_not_found("").code);
}

#[test]
fn default_rulebook_sections_are_well_formed() {
    for title in ["## 硬规则", "## 关键词"] {
        assert!(DEFAULT_RULEBOOK.lines().any(|l| l == title), "{title}");
    }
    let mut section = None;
    let mut count = [0usize; 2];
    for line in DEFAULT_RULEBOOK.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            section = match h {
                "硬规则" => Some(0),
                "关键词" => Some(1),
                _ => None,
            };
            continue;
        }
        let Some(idx) = section else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let rest = line.strip_prefix("- ").expect("列表行");
        let (lvl, words) = rest.split_once(['：', ':']).expect("冒号");
        assert!(Level::parse(lvl).is_some(), "级别 {lvl}");
        let n = words
            .split(['、', ',', '，'])
            .filter(|w| !w.trim().is_empty())
            .count();
        assert!(n >= 1, "{line}");
        count[idx] += n;
    }
    for c in count {
        assert!((3..=8).contains(&c), "词数 {c}");
    }
}

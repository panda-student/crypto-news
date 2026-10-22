use std::sync::{Arc, Mutex};

use plugin_sdk::contracts::channel::TextFormat;
use plugin_sdk::contracts::store::SharedStore;
use plugin_sdk::testing::{test_http_client, TestHub};
use plugin_sdk::{Manifest, Plugin, ServiceError};
use serde_json::{json, Value};

use crate::render::{Material, Reply};
use crate::{keywords, parse_answer, AgentPlugin};

/// 模拟的外部服务及其记录。
#[derive(Default)]
struct World {
    /// item-query.list 收到的 text
    searched: Vec<String>,
    /// llm.chat 收到的请求
    chats: Vec<Value>,
    /// channel.send 收到的请求
    sent: Vec<Value>,
    /// 为真时 llm 报错
    llm_down: bool,
    /// 剩余几次 send 返回限流
    rate_limited: u32,
    format: &'static str,
    max_bytes: usize,
    llm_reply: String,
    /// 为真时没有近期紧急、重要消息
    quiet: bool,
}

fn item(id: i64, title: &str) -> Value {
    json!({
        "id": id, "source_id": "rss-a", "url": format!("https://example.com/{id}"),
        "title": title, "summary": "摘要内容", "published_at": "2026-10-15T08:00:00Z",
        "collected_at": "2026-10-15T08:01:00Z"
    })
}

async fn setup(world: World) -> (Arc<TestHub>, Arc<Mutex<World>>) {
    let hub = TestHub::new();
    let backend: SharedStore = store::open_memory().await.unwrap();
    hub.with_store(backend);
    let w = Arc::new(Mutex::new(world));

    let ww = w.clone();
    hub.register_fn("collect", "item-query", move |method, params| {
        let w = ww.clone();
        async move {
            match method.as_str() {
                "list" => {
                    let text = params["text"].as_str().unwrap_or_default().to_string();
                    w.lock().unwrap().searched.push(text.clone());
                    let items = if text.eq_ignore_ascii_case("btc") {
                        vec![item(1, "BTC 现货 ETF 获批"), item(2, "BTC 矿工抛售")]
                    } else {
                        vec![]
                    };
                    Ok(json!({ "items": items }))
                }
                "get" => {
                    let ids: Vec<i64> = serde_json::from_value(params["ids"].clone()).unwrap();
                    let items: Vec<Value> = ids
                        .into_iter()
                        .filter(|&i| (1..=3).contains(&i) || i == 7)
                        .map(|i| match i {
                            3 => item(i, "某交易所被盗"),
                            7 => item(i, "某项目代币解锁"),
                            _ => item(i, "BTC 现货 ETF 获批"),
                        })
                        .collect();
                    Ok(json!({ "items": items }))
                }
                m => Err(ServiceError::method_not_found(m)),
            }
        }
    });

    let ww = w.clone();
    hub.register_fn("screen", "screening", move |method, params| {
        let quiet = ww.lock().unwrap().quiet;
        async move {
        match method.as_str() {
            "list" => {
                let s = if params["level"] == "urgent" && !quiet {
                    vec![json!({"item_id": 3, "level": "urgent", "status": "done", "attempts": 0})]
                } else {
                    vec![]
                };
                Ok(json!({ "screenings": s }))
            }
            "get" => Ok(json!({ "screenings": [
                {"item_id": 1, "level": "important", "impact": "资金流入预期增强",
                 "status": "done", "attempts": 0, "assets": ["BTC"], "direction": "up"}
            ]})),
            m => Err(ServiceError::method_not_found(m)),
        }
        }
    });

    let ww = w.clone();
    hub.register_fn("llm", "llm", move |_method, params| {
        let w = ww.clone();
        async move {
            let mut w = w.lock().unwrap();
            w.chats.push(params);
            if w.llm_down {
                return Err(ServiceError::unavailable("未配置"));
            }
            Ok(json!({ "content": w.llm_reply, "model": "m", "usage": {"input_tokens": 1, "output_tokens": 1} }))
        }
    });

    let ww = w.clone();
    hub.register_fn("wecom-webhook", "channel", move |method, params| {
        let w = ww.clone();
        async move {
            let mut w = w.lock().unwrap();
            match method.as_str() {
                "info" => Ok(json!({
                    "id": "wecom-webhook", "name": "企业微信", "format": w.format,
                    "max_bytes": w.max_bytes, "backup": false, "state": "ok"
                })),
                "send" => {
                    if w.rate_limited > 0 {
                        w.rate_limited -= 1;
                        return Err(ServiceError {
                            code: ServiceError::RATE_LIMITED,
                            message: "限流".into(),
                        });
                    }
                    w.sent.push(params);
                    Ok(json!({}))
                }
                m => Err(ServiceError::method_not_found(m)),
            }
        }
    });

    let ctx = hub.context("agent", Value::Null, test_http_client());
    AgentPlugin.start(ctx).await.unwrap();
    (hub, w)
}

fn world() -> World {
    World {
        format: "markdown",
        max_bytes: 4096,
        llm_reply: r#"{"answer":"ETF 获批是主要利好 [1]。","cited":[1,9]}"#.into(),
        ..Default::default()
    }
}

fn message(text: &str, id: &str) -> Value {
    json!({
        "channel": "wecom-webhook", "user_id": "u1", "conversation": "conv-1",
        "text": text, "message_id": id, "received_at": "2026-10-16T01:00:00Z"
    })
}

#[test]
fn manifest_parses() {
    Manifest::parse(AgentPlugin.manifest()).unwrap();
}

#[test]
fn keywords_drop_fillers() {
    assert_eq!(keywords("BTC ETF 最近有什么消息？"), ["BTC", "ETF"]);
    assert_eq!(keywords("以太坊升级的情况"), ["以太坊升级"]);
    assert!(keywords("最近有什么消息").is_empty());
    assert_eq!(keywords("btc 和 BTC").len(), 1);
}

#[test]
fn parse_answer_falls_back_and_filters() {
    let a = parse_answer("```json\n{\"answer\":\"x\",\"cited\":[2,2,0,5]}\n```", 3);
    assert_eq!(a.answer, "x");
    assert_eq!(a.cited, [2]);
    let a = parse_answer("不是 JSON", 5);
    assert_eq!(a.answer, "不是 JSON");
    assert_eq!(a.cited, [1, 2, 3]);
}

#[tokio::test]
async fn answers_with_sources_in_markdown() {
    let (hub, w) = setup(world()).await;
    hub.deliver("user.message", message("BTC 最近有什么消息", "m1"))
        .await
        .unwrap();
    let w = w.lock().unwrap();
    assert_eq!(w.searched, ["BTC"]);
    assert_eq!(w.chats.len(), 1);
    let chat = &w.chats[0];
    assert_eq!(chat["tier"], "large");
    assert_eq!(chat["purpose"], "agent");
    let user = chat["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("[1]（重要） BTC 现货 ETF 获批"));
    assert!(user.contains("影响：资金流入预期增强"));
    assert!(user.contains("某交易所被盗"), "应补上最近的紧急消息");

    assert_eq!(w.sent.len(), 1);
    assert_eq!(w.sent[0]["reply_to"], "conv-1");
    let text = w.sent[0]["text"].as_str().unwrap();
    assert!(text.starts_with("**追问回复**"));
    assert!(text.contains("> 问：BTC 最近有什么消息"));
    assert!(text.contains("[1] [BTC 现货 ETF 获批](https://example.com/1)"));
    assert!(!text.contains("[9]"));
    assert!(text.contains("不构成投资建议"));
}

#[tokio::test]
async fn redelivery_does_not_reply_twice() {
    let (hub, w) = setup(world()).await;
    for _ in 0..2 {
        hub.deliver("user.message", message("BTC 怎么样", "m1"))
            .await
            .unwrap();
    }
    assert_eq!(w.lock().unwrap().sent.len(), 1);
}

#[tokio::test]
async fn urgent_news_backs_up_empty_search() {
    let mut wd = world();
    wd.format = "plain";
    let (hub, w) = setup(wd).await;
    // 关键词查不到时，近期紧急消息仍作为兜底资料交给模型。
    hub.deliver("user.message", message("SOL 生态", "m2"))
        .await
        .unwrap();
    let w = w.lock().unwrap();
    // 有紧急消息兜底，仍会调模型
    assert_eq!(w.chats.len(), 1);
    drop(w);

    let empty = Reply::not_found("SOL").render(TextFormat::Plain, 2048);
    assert!(empty.starts_with("【追问回复】"));
    assert!(empty.contains("没找到相关消息"));
}

#[tokio::test]
async fn model_failure_replies_politely() {
    let mut wd = world();
    wd.llm_down = true;
    let (hub, w) = setup(wd).await;
    hub.deliver("user.message", message("BTC", "m3"))
        .await
        .unwrap();
    let w = w.lock().unwrap();
    let text = w.sent[0]["text"].as_str().unwrap();
    assert!(text.contains("模型暂时不可用"));
}

#[tokio::test]
async fn rate_limited_send_is_retried_by_hub() {
    let mut wd = world();
    wd.rate_limited = 1;
    let (hub, w) = setup(wd).await;
    assert!(hub
        .deliver("user.message", message("BTC", "m4"))
        .await
        .is_err());
    assert!(w.lock().unwrap().sent.is_empty());
    hub.deliver("user.message", message("BTC", "m4"))
        .await
        .unwrap();
    assert_eq!(w.lock().unwrap().sent.len(), 1);
}

#[tokio::test]
async fn unknown_channel_is_ignored() {
    let (hub, w) = setup(world()).await;
    let mut m = message("BTC", "m5");
    m["channel"] = json!("wechat-ilink");
    hub.deliver("user.message", m).await.unwrap();
    let w = w.lock().unwrap();
    assert!(w.sent.is_empty());
    assert!(w.chats.is_empty());
}

fn material(i: i64) -> Material {
    Material {
        item: serde_json::from_value(item(i, &format!("标题{i}"))).unwrap(),
        level: None,
        impact: None,
        assets: Vec::new(),
        direction: None,
    }
}

#[test]
fn render_drops_sources_then_truncates() {
    let ms: Vec<Material> = (1..=5).map(material).collect();
    let r = Reply::answer("问题", "正文".repeat(50), &[1, 2, 3, 4, 5], &ms);
    let full = r.render(TextFormat::Plain, 10_000);
    assert!(full.contains("[5] 标题5\nhttps://example.com/5"));
    assert!(full.contains("━━━━━━━━━━"));

    let short = r.render(TextFormat::Plain, 400);
    assert!(short.len() <= 400);
    assert!(!short.contains("[5] 标题5"));
    assert!(short.ends_with("不构成投资建议"));

    let tiny = r.render(TextFormat::Markdown, 120);
    assert!(tiny.len() <= 120);
}

// ---- agent.ask 服务 ----

use plugin_sdk::contracts::agent::{AgentClient, AskRequest, ChatTurn, SERVICE as AGENT};

fn web_client(hub: &Arc<TestHub>) -> AgentClient {
    let ctx = hub.context("web", Value::Null, test_http_client());
    AgentClient(ctx.service(AGENT))
}

const ASK_REPLY: &str = r#"{"found":true,"conclusion":"整体偏利好 [2]。",
"bullish":["ETF 获批带来资金流入 [2]"],"bearish":["交易所被盗影响情绪 [5]"],
"unconfirmed":["", "后续资金规模待观察"],"cited":[2,9]}"#;

#[tokio::test]
async fn ask_returns_four_parts_with_renumbered_citations() {
    let mut wd = world();
    wd.llm_reply = ASK_REPLY.into();
    let (hub, w) = setup(wd).await;
    let req = AskRequest {
        question: "对 BTC 影响多大？".into(),
        item_id: Some(7),
        item_ids: None,
        history: vec![ChatTurn {
            question: "以太坊升级".into(),
            answer: "升级顺利".into(),
        }],
    };
    let r = web_client(&hub).ask(&req).await.unwrap();
    assert!(r.found);
    // 资料：[1] 指定的 7，[2] BTC 命中的 1，[3] 2，[4] 紧急的 3；[5]/[9] 越界被丢掉
    assert_eq!(r.conclusion, "整体偏利好 [1]。");
    assert_eq!(r.bullish, ["ETF 获批带来资金流入 [1]"]);
    assert_eq!(r.bearish, ["交易所被盗影响情绪"]);
    assert_eq!(r.unconfirmed, ["后续资金规模待观察"]);
    assert_eq!(r.citations.len(), 1);
    assert_eq!(r.citations[0].item_id, 1);
    assert_eq!(r.citations[0].url, "https://example.com/1");

    let w = w.lock().unwrap();
    assert!(
        w.searched.iter().any(|s| s == "以太坊升级"),
        "历史问题参与检索"
    );
    let msgs = w.chats[0]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "system + 一轮历史 + 当前问题");
    assert_eq!(msgs[2]["role"], "assistant");
    let user = msgs[3]["content"].as_str().unwrap();
    assert!(
        user.contains("[1] 某项目代币解锁"),
        "指定消息排第一：{user}"
    );
    assert!(user.contains("币种：BTC；方向：利好"));
    // 界面追问不发渠道回复
    assert!(w.sent.is_empty());
}

#[tokio::test]
async fn ask_without_materials_does_not_call_model() {
    let mut wd = world();
    wd.llm_reply = ASK_REPLY.into();
    wd.quiet = true;
    let (hub, w) = setup(wd).await;
    let req = AskRequest {
        question: "SOL 生态".into(),
        ..Default::default()
    };
    let r = web_client(&hub).ask(&req).await.unwrap();
    assert!(!r.found);
    assert!(r.conclusion.contains("没找到"));
    assert!(r.citations.is_empty());
    assert!(w.lock().unwrap().chats.is_empty());
}

#[tokio::test]
async fn ask_model_failure_is_unavailable() {
    let mut wd = world();
    wd.llm_down = true;
    let (hub, _w) = setup(wd).await;
    let req = AskRequest {
        question: "BTC".into(),
        ..Default::default()
    };
    let err = web_client(&hub).ask(&req).await.unwrap_err();
    assert_eq!(err.code, ServiceError::UNAVAILABLE);
}

#[tokio::test]
async fn ask_rejects_other_callers_and_empty_question() {
    let (hub, _w) = setup(world()).await;
    let ctx = hub.context("wecom-webhook", Value::Null, test_http_client());
    let req = AskRequest {
        question: "BTC".into(),
        ..Default::default()
    };
    let err = AgentClient(ctx.service(AGENT)).ask(&req).await.unwrap_err();
    assert_eq!(err.code, ServiceError::FORBIDDEN);

    let err = web_client(&hub)
        .ask(&AskRequest {
            question: "   ".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ServiceError::INVALID_PARAMS);
}

#[test]
fn parse_reply_handles_not_found_and_plain_text() {
    let ms: Vec<Material> = (1..=4).map(material).collect();
    let r = crate::ask::parse_reply(r#"{"found":false,"conclusion":"没找到 [1]"}"#, &ms);
    assert!(!r.found);
    assert_eq!(r.conclusion, "没找到");
    assert!(r.citations.is_empty());

    let r = crate::ask::parse_reply("不是 JSON", &ms);
    assert!(r.found);
    assert_eq!(r.conclusion, "不是 JSON");
    assert_eq!(r.citations.len(), 3);
}

// ---- agent.ask_stream ----

use plugin_sdk::contracts::agent::AskStreamRequest;

#[tokio::test]
async fn ask_stream_keeps_numbers_and_passes_stream_id() {
    let mut wd = world();
    wd.llm_reply = "整体偏利好 [2]。\n利好：ETF 资金流入 [2]\n利空：被盗 [9] [1]".into();
    let (hub, w) = setup(wd).await;
    let req = AskStreamRequest {
        request: AskRequest {
            question: "BTC".into(),
            ..Default::default()
        },
        stream_id: "s-1".into(),
    };
    let r = web_client(&hub).ask_stream(&req).await.unwrap();
    assert!(r.found);
    assert!(!r.cancelled);
    assert!(r.answer.contains("[2]"));
    let ns: Vec<u32> = r.citations.iter().map(|c| c.n).collect();
    assert_eq!(ns[0], 2);
    assert!(!ns.contains(&9), "越界编号不进引用列表");
    let w = w.lock().unwrap();
    assert_eq!(w.chats[0]["stream_id"], "s-1");
    assert_eq!(w.chats[0]["json"], false);
    assert_eq!(w.chats[0]["purpose"], "agent");
}

#[tokio::test]
async fn ask_stream_rules() {
    let mut wd = world();
    wd.quiet = true;
    let (hub, w) = setup(wd).await;
    // 没有资料：不调用模型
    let req = AskStreamRequest {
        request: AskRequest {
            question: "最近有什么消息".into(),
            ..Default::default()
        },
        stream_id: "s-2".into(),
    };
    let r = web_client(&hub).ask_stream(&req).await.unwrap();
    assert!(!r.found);
    assert!(w.lock().unwrap().chats.is_empty());
    // 缺 stream_id
    let mut bad = req.clone();
    bad.stream_id = " ".into();
    assert!(web_client(&hub).ask_stream(&bad).await.is_err());
    // 其他调用方
    let ctx = hub.context("rss", Value::Null, test_http_client());
    let err = AgentClient(ctx.service(AGENT))
        .ask_stream(&req)
        .await
        .unwrap_err();
    assert_eq!(err.code, ServiceError::FORBIDDEN);
}

#[tokio::test]
async fn explicit_context_uses_all_selected_items_and_excludes_removed_item() {
    let (hub, w) = setup(world()).await;
    for ids in [vec![7, 1], vec![1]] {
        web_client(&hub)
            .ask(&AskRequest {
                question: "比较影响".into(),
                item_id: Some(7),
                item_ids: Some(ids),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let w = w.lock().unwrap();
    let first = w.chats[0]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    let second = w.chats[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(first.contains("某项目代币解锁") && first.contains("BTC 现货 ETF 获批"));
    assert!(!second.contains("某项目代币解锁"));
    assert!(second.contains("BTC 现货 ETF 获批"));
    assert!(
        w.searched.is_empty(),
        "explicit context must not retrieve removed sources"
    );
}

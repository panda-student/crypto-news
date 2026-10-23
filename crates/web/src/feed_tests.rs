//! 消息流、详情、追问、统计、设置的测试。服务全部用 `register_fn` 模拟，不访问网络，不打开浏览器。

use std::sync::{Arc, Mutex};

use plugin_sdk::testing::{test_http_client, TestHub};
use serde_json::{json, Value};
use time::{Duration, OffsetDateTime};

use super::{data, opener, router_with};

struct FakeOpener(Arc<Mutex<Vec<String>>>);

impl opener::Opener for FakeOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

fn rfc(t: OffsetDateTime) -> String {
    plugin_sdk::contracts::format_rfc3339(t)
}

struct F {
    base: String,
    client: reqwest::Client,
    opened: Arc<Mutex<Vec<String>>>,
    asks: Arc<Mutex<Vec<Value>>>,
    _hub: Arc<TestHub>,
}

impl F {
    async fn get(&self, p: &str) -> (u16, String) {
        let r = self
            .client
            .get(format!("{}{p}", self.base))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
    async fn post(&self, p: &str, form: &[(&str, &str)]) -> (u16, String) {
        let r = self
            .client
            .post(format!("{}{p}", self.base))
            .form(form)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }
    async fn jget(&self, p: &str) -> (u16, Value) {
        let r = self
            .client
            .get(format!("{}{p}", self.base))
            .header("accept", "application/json")
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    async fn jpost(&self, p: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{p}", self.base))
            .header("accept", "application/json")
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

struct Opts {
    allowed: bool,
    agent: bool,
    screening: bool,
}

fn items(now: OffsetDateTime) -> Vec<Value> {
    let m = |n: i64| rfc(now - Duration::minutes(n));
    let old = (now - Duration::days(3)).date().midnight().assume_utc() + Duration::hours(12);
    vec![
        json!({"id":1,"source_id":"rss:a","url":"https://x.test/1","title":"BTC突发","summary":"摘要一",
            "published_at":m(15),"collected_at":m(10)}),
        json!({"id":2,"source_id":"rss:b","url":"https://x.test/2?a=1&b=2","title":"ETH升级","summary":null,
            "published_at":null,"collected_at":m(20)}),
        json!({"id":3,"source_id":"rss:a","url":"https://x.test/3","title":"BTC日常","summary":null,
            "published_at":null,"collected_at":m(30)}),
        json!({"id":4,"source_id":"rss:b","url":"javascript:alert(1)","title":"未分级消息","summary":null,
            "published_at":null,"collected_at":m(40)}),
        json!({"id":5,"source_id":"rss:b","url":"file:///etc/passwd","title":"SOL归档","summary":null,
            "published_at":null,"collected_at":m(50)}),
        json!({"id":6,"source_id":"rss:a","url":"https://x.test/6","title":"旧消息","summary":null,
            "published_at":null,"collected_at":rfc(old)}),
    ]
}

fn screenings() -> Vec<Value> {
    let s = |id: i64, level: &str, dir: &str, assets: Value, impact: Value| {
        json!({"item_id":id,"level":level,"status":"done","attempts":0,"assets":assets,
            "direction":dir,"impact":impact,"reason":format!("原因{id}")})
    };
    vec![
        s(1, "urgent", "up", json!(["BTC"]), json!("影响一句话")),
        {
            let mut v = s(2, "important", "down", json!(["ETH"]), Value::Null);
            v["title_zh"] = json!("以太坊升级");
            v["summary_zh"] = json!("以太坊将升级");
            v
        },
        s(3, "normal", "neutral", json!(["BTC"]), Value::Null),
        s(5, "archive", "up", json!(["SOL"]), Value::Null),
        s(6, "normal", "neutral", json!([]), Value::Null),
    ]
}

async fn fixture(o: Opts) -> F {
    let hub = TestHub::new();
    hub.with_store(store::open_memory().await.unwrap());
    let now = OffsetDateTime::now_utc();
    let all = items(now);
    let allowed = o.allowed;
    hub.register_fn("auth", "auth", move |_m, _p| async move {
        Ok(json!({ "allowed": allowed }))
    });
    hub.register_fn("collect", "item-query", move |m, p| {
        let all = all.clone();
        async move {
            let list: Vec<Value> = match m.as_str() {
                "get" => {
                    let ids: Vec<i64> =
                        serde_json::from_value(p["ids"].clone()).unwrap_or_default();
                    all.into_iter()
                        .filter(|i| ids.contains(&i["id"].as_i64().unwrap()))
                        .collect()
                }
                _ => all
                    .into_iter()
                    .filter(|i| {
                        p["source_id"].as_str().is_none_or(|s| i["source_id"] == s)
                            && p["text"]
                                .as_str()
                                .is_none_or(|t| i["title"].as_str().unwrap().contains(t))
                    })
                    .collect(),
            };
            Ok(json!({ "items": list }))
        }
    });
    if o.screening {
        let sc = screenings();
        hub.register_fn("screen", "screening", move |m, p| {
            let sc = sc.clone();
            async move {
                if m == "stats" {
                    let today = OffsetDateTime::now_utc().date();
                    let day = format!(
                        "{:04}-{:02}-{:02}",
                        today.year(),
                        today.month() as u8,
                        today.day()
                    );
                    return Ok(json!({
                        "by_day":[{"key":day,"level":"urgent","count":2},{"key":day,"level":"normal","count":5}],
                        "by_source":[{"key":"rss:a","level":"urgent","count":2}],
                        "by_asset":[{"key":"BTC","level":"urgent","count":2},{"key":"ETH","level":"important","count":1}]
                    }));
                }
                let ids: Vec<i64> = serde_json::from_value(p["ids"].clone()).unwrap_or_default();
                let v: Vec<Value> = sc
                    .into_iter()
                    .filter(|s| ids.contains(&s["item_id"].as_i64().unwrap()))
                    .collect();
                Ok(json!({ "screenings": v }))
            }
        });
    }
    hub.register_fn("dispatch", "dispatch", |_m, _p| async move {
        Ok(json!({"entries":[{"id":1,"kind":"flash","channel":"wecom-webhook","status":"sent",
            "attempts":1,"batch":1,"text":"t","item_ids":[1],"created_at":"2026-01-01T00:00:00Z"}]}))
    });
    let asks: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    if o.agent {
        let a = asks.clone();
        hub.register_fn("agent", "agent", move |m, p| {
            let a = a.clone();
            async move {
                if m == "operate" {
                    return Ok(Value::Null);
                }
                if m == "conversation_turns" {
                    return Ok(json!([]));
                }
                a.lock().unwrap().push(p.clone());
                Ok(
                    json!({"found":true,"answer":"结论文本 [1]","cancelled":false,
                    "citations":[{"n":1,"item_id":1,"title":"引用标题","url":"https://x.test/1"}]}),
                )
            }
        });
    }
    let ctx = hub.context("web", json!({}), test_http_client());
    ctx.store().migrate(1, data::SCHEMA).await.unwrap();
    ctx.store()
        .migrate(2, data::CONVERSATION_SCHEMA)
        .await
        .unwrap();
    let opened = Arc::new(Mutex::new(Vec::new()));
    let app = router_with(ctx, Arc::new(FakeOpener(opened.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    F {
        base: format!("http://{addr}"),
        client: test_http_client(),
        opened,
        asks,
        _hub: hub,
    }
}

async fn std_fixture() -> F {
    fixture(Opts {
        allowed: true,
        agent: true,
        screening: true,
    })
    .await
}

#[tokio::test]
async fn home_renders_main_ui_and_updates_last_seen() {
    let f = std_fixture().await;
    let ctx = f._hub.context("probe", json!({}), test_http_client());
    let d = data::Db(ctx.store());
    assert!(d.pref("last_seen_at").await.unwrap().is_none());
    let (s, b) = f.get("/").await;
    assert_eq!(s, 200);
    assert!(b.contains("id=\"w\"") && b.contains("id=\"rb\"") && b.contains("id=\"sbtn\""));
    assert!(b.contains("id=\"feed\"") && b.contains("id=\"sb\"") && !b.contains("href=\"/stats\""));
    assert!(b.contains("href=\"/settings\""));
    // v17 点击标题加入会话，打开原文是独立操作。
    assert!(b.contains("class=\"item-title\" data-id="));
    assert!(!b.contains("<a class=\"it"), "消息行不能是链接");
    assert!(b.contains("data-last=\"\""), "首次访问没有上次查看时间");
    assert!(d.pref("last_seen_at").await.unwrap().is_some());
    // 第二次访问把上次查看时间交给摘要条
    let (_, b2) = f.get("/").await;
    assert!(!b2.contains("data-last=\"\""), "{b2}");
}

#[tokio::test]
async fn dash_returns_week_items_with_levels_and_push() {
    let f = std_fixture().await;
    let (s, v) = f.jget("/api/v1/dash").await;
    assert_eq!(s, 200);
    assert_eq!(v["days"].as_array().unwrap().len(), 7);
    let its = v["items"].as_array().unwrap();
    let it1 = its.iter().find(|i| i["id"] == 1).unwrap();
    assert_eq!(it1["lv"], "u");
    assert_eq!(it1["impact"], "影响一句话");
    assert_eq!(it1["pushed"], true);
    assert_eq!(it1["pushed_to"], json!(["wecom-webhook"]));
    let it2 = its.iter().find(|i| i["id"] == 2).unwrap();
    assert_eq!(it2["lv"], "i");
    assert_eq!(it2["pushed"], false);
    assert!(its.iter().all(|i| i["d"].as_i64().unwrap() < 7));
    assert_eq!(v["sources_unknown"], true);
    // v16b 新增字段
    assert_eq!(it1["dc"], "up");
    assert_eq!(it1["lag"], 5);
    assert!(it1["at"].as_str().unwrap().contains('T'));
    assert!(it1["ca"].as_str().unwrap().contains('T'));
    assert!(v["now"].as_str().unwrap().contains('T'));
}

#[tokio::test]
async fn poll_returns_json_count_and_now() {
    let f = std_fixture().await;
    let since = rfc(OffsetDateTime::now_utc() - Duration::minutes(25));
    let (s, v) = f.jget(&format!("/feed/new?since={since}")).await;
    assert_eq!(s, 200);
    assert_eq!(v["n"], 2);
    assert!(v["now"].as_str().unwrap().contains('T'));
    let since = rfc(OffsetDateTime::now_utc() + Duration::minutes(1));
    let (_, v) = f.jget(&format!("/feed/new?since={since}")).await;
    assert_eq!(v["n"], 0);
}

#[tokio::test]
async fn main_page_actions_are_json_and_reflected_in_dash() {
    let f = std_fixture().await;
    // 待跟进、全部已读都走 JSON 接口，并体现在 dash 里
    let (s, v) = f.jpost("/api/v1/items/2/star", json!({})).await;
    assert_eq!((s, &v["starred"]), (200, &json!(true)));
    let (_, v) = f.jget("/api/v1/dash").await;
    let it2 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == 2)
        .unwrap();
    assert_eq!(it2["starred"], true);
    let (s, v) = f.jpost("/api/v1/items/read_all", json!({})).await;
    assert_eq!(s, 200);
    assert!(v["marked"].as_u64().unwrap() >= 1);
    let (_, v) = f.jget("/api/v1/dash").await;
    assert!(v["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["unread"] == false));
}

#[tokio::test]
async fn open_original_uses_opener_and_rejects_non_http() {
    let f = std_fixture().await;
    let (s, _) = f.post("/api/v1/items/2/open", &[]).await;
    assert_eq!(s, 204);
    assert_eq!(
        f.opened.lock().unwrap().clone(),
        vec!["https://x.test/2?a=1&b=2".to_string()]
    );
    let (s, _) = f.post("/api/v1/items/4/open", &[]).await;
    assert_eq!(s, 400, "javascript: 不能打开");
    let (s, _) = f.post("/api/v1/items/5/open", &[]).await;
    assert_eq!(s, 400, "file: 不能打开");
    let (s, _) = f.post("/api/v1/items/99/open", &[]).await;
    assert_eq!(s, 404);
    assert_eq!(f.opened.lock().unwrap().len(), 1);
}

/// 读完一条流：返回拼起来的增量文本和 done 事件的数据。
async fn drain(f: &F, sid: &str) -> (String, Value) {
    let mut from = 0usize;
    let mut text = String::new();
    for _ in 0..50 {
        let (s, b) = f
            .get(&format!("/api/v1/chat/stream/{sid}?from={from}"))
            .await;
        assert_eq!(s, 200);
        let mut ev = "";
        for line in b.lines() {
            if let Some(e) = line.strip_prefix("event: ") {
                ev = e;
            } else if let Some(d) = line.strip_prefix("id: ") {
                from = d.parse().unwrap();
            } else if let Some(d) = line.strip_prefix("data: ") {
                let v: Value = serde_json::from_str(d).unwrap();
                if ev == "delta" {
                    text.push_str(v.as_str().unwrap());
                } else if ev == "done" {
                    return (text, v);
                }
            }
        }
    }
    panic!("流没有结束");
}

#[tokio::test]
async fn chat_new_session_with_item_and_history() {
    let f = std_fixture().await;
    let (s, v) = f
        .jpost(
            "/api/v1/chat",
            json!({"question":"BTC 影响多大？","item_id":"1"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["new_session"], true);
    let sid = v["stream_id"].as_str().unwrap().to_string();
    let (_, done) = drain(&f, &sid).await;
    let html = done["html"].as_str().unwrap();
    assert!(html.contains("结论文本"));
    assert!(html.contains("data-n=\"1\""), "{html}");
    assert!(html.contains("data-cite=\"1\"") && html.contains("data-loc=\"1\""));
    assert!(html.contains("引用标题"));
    {
        let asks = f.asks.lock().unwrap();
        assert_eq!(asks[0]["item_id"], 1);
        assert_eq!(asks[0]["history"].as_array().unwrap().len(), 0);
        assert_eq!(asks[0]["stream_id"], sid.as_str());
    }
    let (s, v) = f.jget("/api/v1/chat/session/1").await;
    assert_eq!(s, 200);
    assert_eq!(v["item"]["id"], 1);
    assert_eq!(v["turns"][0]["question"], "BTC 影响多大？");
    assert!(v["turns"][0]["html"].as_str().unwrap().contains("结论文本"));
    let (s, v) = f
        .jpost(
            "/api/v1/chat",
            json!({"question":"还有呢？","session_id":"1"}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["new_session"], false);
    drain(&f, v["stream_id"].as_str().unwrap()).await;
    {
        let asks = f.asks.lock().unwrap();
        assert_eq!(asks[1]["item_id"], 1);
        let h = asks[1]["history"].as_array().unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["question"], "BTC 影响多大？");
        assert_eq!(h[0]["answer"], "结论文本 [1]");
    }
    let (_, v) = f.jget("/api/v1/chat/session/1").await;
    assert_eq!(v["turns"].as_array().unwrap().len(), 2);
    let (s, _) = f.jget("/api/v1/chat/session/99").await;
    assert_eq!(s, 404);
    let (s, _) = f.get("/api/v1/chat/stream/nope").await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn chat_streams_deltas_then_done_and_saves_answer() {
    let f = std_fixture().await;
    let hub = f._hub.clone();
    f._hub.register_fn("agent", "agent", move |m, p| {
        let hub = hub.clone();
        async move {
            if m == "operate" {
                return Ok(Value::Null);
            }
            if m == "conversation_turns" {
                return Ok(json!([]));
            }
            let sid = p["stream_id"].as_str().unwrap().to_string();
            for t in ["比特币", "走强 ", "[1]"] {
                hub.deliver("llm.delta", json!({"stream_id": sid, "text": t}))
                    .await
                    .unwrap();
            }
            hub.deliver("llm.delta", json!({"stream_id": "other", "text": "不相干"}))
                .await
                .unwrap();
            Ok(
                json!({"found":true,"answer":"比特币走强 [1]","cancelled":false,
                "citations":[{"n":1,"item_id":2,"title":"ETH升级","url":"https://x.test/2"}]}),
            )
        }
    });
    let (_, v) = f
        .jpost("/api/v1/chat", json!({"question":"行情如何"}))
        .await;
    let (text, done) = drain(&f, v["stream_id"].as_str().unwrap()).await;
    assert_eq!(text, "比特币走强 [1]", "只收本流的增量，且按顺序");
    assert_eq!(done["stopped"], false);
    assert_eq!(done["answer"], "比特币走强 [1]");
    assert!(done["html"].as_str().unwrap().contains("data-cite=\"2\""));
    let (_, s) = f.jget("/api/v1/chat/session/1").await;
    let t = &s["turns"][0];
    assert_eq!(t["pending"], false);
    assert!(t["html"].as_str().unwrap().contains("比特币走强"));
}

#[tokio::test]
async fn chat_cancel_publishes_llm_cancel() {
    let f = std_fixture().await;
    let (s, v) = f
        .jpost("/api/v1/chat/stream/abc123/cancel", json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["cancelled"], true);
    let ev = f._hub.published("llm.cancel");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].payload["stream_id"], "abc123");
}

#[tokio::test]
async fn chat_rename_delete_undo() {
    let f = std_fixture().await;
    let (_, v) = f
        .jpost("/api/v1/chat", json!({"question":"最近怎么样"}))
        .await;
    assert_eq!(v["session_id"], 1);
    drain(&f, v["stream_id"].as_str().unwrap()).await;
    let (s, _) = f.jpost("/api/v1/chat", json!({"question":"  "})).await;
    assert_eq!(s, 400);
    let (s, v) = f
        .jpost("/api/v1/chat/1/rename", json!({"title":"新标题"}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["title"], "新标题");
    let (_, v) = f.jget("/api/v1/chat/sessions").await;
    assert_eq!(v["sessions"][0]["title"], "新标题");
    assert_eq!(v["sessions"][0]["group"], "今天");
    let (s, _) = f.jpost("/api/v1/chat/1/rename", json!({"title":" "})).await;
    assert_eq!(s, 400);
    let (s, _) = f
        .jpost("/api/v1/chat/99/rename", json!({"title":"x"}))
        .await;
    assert_eq!(s, 404);
    let (s, v) = f.jpost("/api/v1/chat/1/delete", json!({})).await;
    assert_eq!(s, 200);
    let token = v["undo"].as_str().unwrap().to_string();
    let (s, _) = f.jget("/api/v1/chat/session/1").await;
    assert_eq!(s, 404);
    let (_, v) = f.jget("/api/v1/chat/sessions").await;
    assert!(v["sessions"].as_array().unwrap().is_empty());
    let (s, _) = f.jpost("/api/v1/chat/1/delete", json!({})).await;
    assert_eq!(s, 404);
    let (s, v) = f
        .jpost(&format!("/api/v1/chat/undo/{token}"), json!({}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["session_id"], 1);
    let (_, v) = f.jget("/api/v1/chat/session/1").await;
    assert_eq!(v["title"], "新标题");
    assert_eq!(v["turns"][0]["question"], "最近怎么样");
    let (s, _) = f
        .jpost(&format!("/api/v1/chat/undo/{token}"), json!({}))
        .await;
    assert_eq!(s, 404, "撤销只能用一次");
}

#[tokio::test]
async fn chat_without_agent_saves_hint_not_500() {
    let f = fixture(Opts {
        allowed: true,
        agent: false,
        screening: true,
    })
    .await;
    let (s, v) = f.jpost("/api/v1/chat", json!({"question":"你好"})).await;
    assert_eq!(s, 200);
    let (_, done) = drain(&f, v["stream_id"].as_str().unwrap()).await;
    assert!(done["error"]
        .as_str()
        .unwrap()
        .contains("追问功能当前不可用"));
    assert!(done["html"].as_str().unwrap().contains("cerr"));
    let (_, s) = f.jget("/api/v1/chat/session/1").await;
    assert_eq!(s["turns"][0]["question"], "你好");
    assert!(s["turns"][0]["html"].as_str().unwrap().contains("cerr"));
}

#[tokio::test]
async fn stats_page_and_json() {
    let f = std_fixture().await;
    let (s, b) = f.get("/stats").await;
    assert_eq!(s, 200);
    // 统计页由脚本按 /api/v1/dash 渲染，点击跳回 /?筛选
    assert!(b.contains("id=\"stv\"") && b.contains("/api/v1/dash"));
    assert!(b.contains("href=\"/\"") && b.contains("lv=ui") && b.contains("asset="));
    let (s, v) = f.jget("/api/v1/stats").await;
    assert_eq!(s, 200);
    assert_eq!(v["days"], 7);
    assert_eq!(v["cards"][1]["n"], 2);
    let (s, v) = f.jget("/api/v1/stats?days=30").await;
    assert_eq!(s, 200);
    assert_eq!(v["days"], 30);
    let (s, _) = f.jget("/api/v1/stats?days=99").await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn stats_page_opens_without_screening() {
    let f = fixture(Opts {
        allowed: true,
        agent: true,
        screening: false,
    })
    .await;
    let (s, b) = f.get("/stats").await;
    assert_eq!(s, 200);
    assert!(b.contains("id=\"stv\""));
}

#[tokio::test]
async fn settings_page_shares_modal_panel_and_preserves_management_entries() {
    let f = std_fixture().await;
    let (s, b) = f.get("/settings").await;
    assert_eq!(s, 200);
    for l in [
        "/settings/llm",
        "/settings/sources",
        "/settings/push",
        "/settings/plugins",
        "/settings/system",
    ] {
        assert!(b.contains(&format!("data-settings-url=\"{l}\"")), "{l}");
    }
    assert!(b.contains("关闭设置"));
    let (status, panel) = f.get("/settings/panel").await;
    assert_eq!(status, 200);
    assert!(panel.contains("data-settings-shell") && !panel.contains("<html"));
    assert!(panel.contains("aria-controls=\"s-watch\""));
    // 旧的插件页、信源页并入设置页
    let r = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{}/plugins", f.base))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_redirection());
    // 顶栏
    let (_, b) = f.get("/").await;
    assert!(b.contains("href=\"/settings\""));
}

#[tokio::test]
async fn new_endpoints_denied_without_permission() {
    let f = fixture(Opts {
        allowed: false,
        agent: true,
        screening: true,
    })
    .await;
    for p in [
        "/",
        "/feed/new",
        "/stats",
        "/settings",
        "/settings/panel",
        "/api/v1/dash",
        "/api/v1/chat/sessions",
        "/api/v1/chat/session/1",
        "/api/v1/stats",
    ] {
        let (s, _) = f.get(p).await;
        assert_eq!(s, 403, "GET {p}");
    }
    for p in [
        "/api/v1/items/read_all",
        "/api/v1/items/1/read",
        "/api/v1/items/1/star",
        "/api/v1/items/1/open",
        "/api/v1/chat",
        "/api/v1/chat/1/rename",
        "/api/v1/chat/1/delete",
        "/api/v1/chat/stream/x/cancel",
        "/api/v1/chat/undo/x",
        "/api/v1/prefs/watch_assets",
    ] {
        let (s, _) = f
            .post(p, &[("question", "x"), ("title", "x"), ("assets", "BTC")])
            .await;
        assert_eq!(s, 403, "POST {p}");
    }
    assert!(f.opened.lock().unwrap().is_empty());
    assert!(f.asks.lock().unwrap().is_empty());
}

fn dash_ids(v: &Value) -> Vec<i64> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn read_items_stay_in_dash_and_order_is_time_only() {
    let f = std_fixture().await;
    let (_, v) = f.jget("/api/v1/dash").await;
    // 紧急、重要不置顶：只按入库时间倒序
    assert_eq!(dash_ids(&v), vec![1, 2, 3, 4, 5, 6]);
    for id in [1, 3] {
        let (s, _) = f.post(&format!("/api/v1/items/{id}/read"), &[]).await;
        assert_eq!(s, 204);
    }
    let (_, v) = f.jget("/api/v1/dash").await;
    assert_eq!(dash_ids(&v), vec![1, 2, 3, 4, 5, 6], "已读后消息仍在原位");
    let unread = |id: i64| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id)
            .unwrap()["unread"]
            .clone()
    };
    assert_eq!(
        (unread(1), unread(3), unread(2)),
        (json!(false), json!(false), json!(true))
    );
    // 全部已读只改标识，不清空列表
    let (_, r) = f.jpost("/api/v1/items/read_all", json!({})).await;
    assert!(r["marked"].as_u64().unwrap() >= 1);
    let (_, v) = f.jget("/api/v1/dash").await;
    assert_eq!(dash_ids(&v), vec![1, 2, 3, 4, 5, 6]);
    assert!(v["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["unread"] == false));
}

#[tokio::test]
async fn home_has_refresh_button_no_pinning_and_unread_filter_off() {
    let f = std_fixture().await;
    let (_, b) = f.get("/").await;
    assert!(b.contains("data-refresh=\"1\""), "刷新按钮");
    assert!(
        !b.contains("k === 'r'") && !b.contains("J/K 切换"),
        "无快捷键"
    );
    assert!(!b.contains("置顶"), "不再有置顶分组");
    assert!(b.contains("F = { asset: null, lv: 'all', dir: null, unread: false"));
    // 旧的首页相关路由已删除
    for p in ["/feed", "/items/1", "/sources/summary"] {
        let (s, _) = f.get(p).await;
        assert!(s == 404 || s == 405, "{p} {s}");
    }
}

#[tokio::test]
async fn dash_shows_chinese_title_and_summary_with_fallback() {
    let f = std_fixture().await;
    let (_, v) = f.jget("/api/v1/dash").await;
    let its = v["items"].as_array().unwrap();
    let it2 = its.iter().find(|i| i["id"] == 2).unwrap();
    assert_eq!(it2["t"], "以太坊升级");
    assert_eq!(it2["ot"], "ETH升级");
    assert_eq!(it2["sz"], "以太坊将升级");
    let it1 = its.iter().find(|i| i["id"] == 1).unwrap();
    assert_eq!(it1["t"], "BTC突发");
    assert!(it1["ot"].is_null() && it1["sz"].is_null());
}

#[tokio::test]
async fn multiple_contexts_survive_removal_reload_and_keep_turn_snapshot() {
    let f = std_fixture().await;
    let (status, v) = f
        .jpost(
            "/api/v1/chat/context",
            json!({"items":[{"id":1,"after":0},{"id":2,"after":0}]}),
        )
        .await;
    assert_eq!(status, 200);
    let id = v["session_id"].as_i64().unwrap();
    let (_, session) = f.jget(&format!("/api/v1/chat/session/{id}")).await;
    assert_eq!(session["context"].as_array().unwrap().len(), 2);
    assert_eq!(session["context"][1]["item"]["title"], "以太坊升级");
    let (_, r) = f
        .jpost(
            "/api/v1/chat",
            json!({"session_id":id.to_string(),"question":"比较影响","item_ids":[1,2]}),
        )
        .await;
    drain(&f, r["stream_id"].as_str().unwrap()).await;
    assert_eq!(
        f.asks.lock().unwrap().last().unwrap()["item_ids"],
        json!([1, 2])
    );
    let (status, _) = f
        .jpost(
            "/api/v1/chat/context",
            json!({"session_id":id,"items":[{"id":2,"after":0}]}),
        )
        .await;
    assert_eq!(status, 200);
    let (_, r) = f
        .jpost(
            "/api/v1/chat",
            json!({"session_id":id.to_string(),"question":"继续","item_ids":[2]}),
        )
        .await;
    drain(&f, r["stream_id"].as_str().unwrap()).await;
    assert_eq!(
        f.asks.lock().unwrap().last().unwrap()["item_ids"],
        json!([2])
    );
    let (_, session) = f.jget(&format!("/api/v1/chat/session/{id}")).await;
    assert_eq!(session["context"].as_array().unwrap().len(), 1);
    assert_eq!(session["turns"][0]["item_ids"], json!([1, 2]));
    let (status, _) = f
        .jpost("/api/v1/chat/context", json!({"session_id":999,"items":[]}))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn channel_and_desktop_continue_one_durable_conversation_without_duplicate_turns() {
    use plugin_sdk::contracts::agent::AgentClient;
    use plugin_sdk::Plugin;
    let f = fixture(Opts {
        allowed: true,
        agent: false,
        screening: true,
    })
    .await;
    let chats = Arc::new(Mutex::new(Vec::<Value>::new()));
    let log = chats.clone();
    f._hub.register_fn("llm","llm",move |method,params|{let log=log.clone();async move {
        log.lock().unwrap().push(params.clone());
        let content=if method=="chat_stream" {"桌面继续分析风险 [1]"} else {r#"{"answer":"微信分析结果 [1]","cited":[1]}"#};
        let response=json!({"content":content,"model":"test","usage":{"input_tokens":1,"output_tokens":1}});
        Ok(response)
    }});
    f._hub.register_fn("wechat-ilink","channel",|method,_|async move {
        if method=="info" {Ok(json!({"id":"wechat-ilink","name":"微信","format":"plain","max_bytes":4000,"backup":false,"state":"ok"}))}else{Ok(json!({}))}
    });
    agent::AgentPlugin
        .start(f._hub.context("agent", Value::Null, test_http_client()))
        .await
        .unwrap();
    let message = |id: &str, text: &str, user: &str| json!({"channel":"wechat-ilink","user_id":user,"conversation":"wx-1","text":text,"message_id":id,"received_at":rfc(OffsetDateTime::now_utc())});
    f._hub
        .deliver(
            "user.message",
            message("wx-msg-1", "BTC 有什么风险", "owner"),
        )
        .await
        .unwrap();
    let (_, sessions) = f.jget("/api/v1/chat/sessions").await;
    assert!(sessions["sync_error"].is_null(), "{sessions}");
    let sid = sessions["sessions"][0]["id"].as_i64().unwrap();
    let path = format!("/api/v1/chat/session/{sid}");
    let (_, session) = f.jget(&path).await;
    assert_eq!(session["turns"].as_array().unwrap().len(), 1);
    assert_eq!(session["turns"][0]["origin"], "wechat-ilink");
    let (_, r) = f
        .jpost(
            "/api/v1/chat",
            json!({"session_id":sid.to_string(),"question":"继续分析这个风险"}),
        )
        .await;
    drain(&f, r["stream_id"].as_str().unwrap()).await;
    let client = AgentClient(
        f._hub
            .context("web", Value::Null, test_http_client())
            .service("agent"),
    );
    let records = client.conversation_turns(0).await.unwrap();
    assert_eq!(
        records.len(),
        2,
        "desktop answer must reach the shared store"
    );
    assert_eq!(records[1].origin, "desktop");
    let messages = chats.lock().unwrap()[1]["messages"].clone();
    assert!(
        messages.to_string().contains("BTC 有什么风险"),
        "desktop must use WeChat history"
    );
    for _ in 0..2 {
        f.jget("/api/v1/chat/sessions").await;
    }
    let (_, session) = f.jget(&path).await;
    assert_eq!(
        session["turns"].as_array().unwrap().len(),
        2,
        "import must not duplicate desktop turns"
    );
    f._hub
        .deliver("user.message", message("wx-msg-2", "那现在呢", "owner"))
        .await
        .unwrap();
    assert!(
        chats.lock().unwrap().last().unwrap()["messages"]
            .to_string()
            .contains("桌面继续分析风险"),
        "WeChat must use desktop history"
    );
    f._hub
        .deliver("user.message", message("wx-msg-2", "那现在呢", "owner"))
        .await
        .unwrap();
    f.jget("/api/v1/chat/sessions").await;
    let (_, session) = f.jget(&path).await;
    assert_eq!(session["turns"].as_array().unwrap().len(), 3);
    f._hub
        .deliver(
            "user.message",
            message("wx-msg-3", "ETH 呢", "another-user"),
        )
        .await
        .unwrap();
    let (_, sessions) = f.jget("/api/v1/chat/sessions").await;
    assert_eq!(
        sessions["sessions"].as_array().unwrap().len(),
        2,
        "different users must not share context"
    );
    assert!(!chats.lock().unwrap().last().unwrap()["messages"]
        .to_string()
        .contains("桌面继续分析风险"));
}

#[tokio::test]
async fn shared_outbox_retries_without_duplicate_import() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = std_fixture().await;
    let key = r#"["wechat-ilink","owner","conversation"]"#;
    let records = Arc::new(Mutex::new(vec![
        json!({"id":1,"turn_key":"wechat:1","conversation_key":key,"channel":"wechat-ilink","origin":"wechat-ilink","question":"微信先问","reply_json":"{\"answer\":\"先前回答\"}","created_at":rfc(OffsetDateTime::now_utc())}),
    ]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let log = records.clone();
    let count = attempts.clone();
    f._hub.register_fn("agent", "agent", move |m, p| {
        let log = log.clone();
        let count = count.clone();
        async move {
            match m.as_str() {
                "operate" => Ok(Value::Null),
                "conversation_turns" => Ok(json!(log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["id"].as_i64().unwrap() > p["after"].as_i64().unwrap())
                    .cloned()
                    .collect::<Vec<_>>())),
                "record_conversation_turn" => {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(plugin_sdk::ServiceError::unavailable("temporary failure"));
                    }
                    let mut all = log.lock().unwrap();
                    if !all.iter().any(|t| t["turn_key"] == p["turn_key"]) {
                        let mut row = p;
                        row["id"] = json!(2);
                        row["created_at"] = json!(rfc(OffsetDateTime::now_utc()));
                        all.push(row);
                    }
                    Ok(json!(2))
                }
                _ => Ok(
                    json!({"answer":"桌面续问结果","found":true,"citations":[],"cancelled":false}),
                ),
            }
        }
    });
    let (_, sessions) = f.jget("/api/v1/chat/sessions").await;
    let id = sessions["sessions"][0]["id"].as_i64().unwrap();
    let (_, r) = f
        .jpost(
            "/api/v1/chat",
            json!({"session_id":id.to_string(),"question":"接着问"}),
        )
        .await;
    drain(&f, r["stream_id"].as_str().unwrap()).await;
    for _ in 0..2 {
        f.jget("/api/v1/chat/sessions").await;
    }
    assert_eq!(records.lock().unwrap().len(), 2);
    let (_, s) = f.jget(&format!("/api/v1/chat/session/{id}")).await;
    assert_eq!(s["turns"].as_array().unwrap().len(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(data::Db(
        f._hub
            .context("web", Value::Null, test_http_client())
            .store()
    )
    .pending_shared()
    .await
    .unwrap()
    .is_empty());
}

#[tokio::test]
async fn push_settings_api_validates_before_persistence() {
    let f = std_fixture().await;
    let saved = Arc::new(Mutex::new(
        json!({"enabled":true,"scope":"all","assets":[],"interval":5,"effective_at":""}),
    ));
    let state = saved.clone();
    f._hub
        .register_fn("dispatch", "dispatch", move |method, params| {
            let state = state.clone();
            async move {
                match method.as_str() {
                    "focus_settings" => Ok(state.lock().unwrap().clone()),
                    "save_focus_settings" => {
                        *state.lock().unwrap() = params.clone();
                        Ok(params)
                    }
                    _ => Err(plugin_sdk::ServiceError::method_not_found(&method)),
                }
            }
        });
    let (status, v) = f.jget("/api/v1/push/settings").await;
    assert_eq!(status, 200);
    assert_eq!(v["scope"], "all");
    let r = f
        .client
        .post(format!("{}/api/v1/push/settings", f.base))
        .json(&json!({"enabled":true,"scope":"followed","assets":[],"interval":5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(saved.lock().unwrap()["scope"], "all");
    let r = f
        .client
        .post(format!("{}/api/v1/push/settings", f.base))
        .json(&json!({"enabled":true,"scope":"followed","assets":["BTC"],"interval":10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(saved.lock().unwrap()["scope"], "followed");
}

#[test]
fn webhook_invalid_is_not_displayed_as_expired_session() {
    use plugin_sdk::contracts::channel::ChannelState;
    assert_eq!(
        crate::pages::channel_state_label("wecom-webhook", ChannelState::Expired),
        "链接无效"
    );
    assert_eq!(
        crate::pages::channel_state_label("wechat-ilink", ChannelState::Expired),
        "已过期"
    );
}

#[tokio::test]
async fn unified_status_reads_plugin_owned_state_and_isolates_section_failure() {
    let f = fixture(Opts {
        allowed: true,
        agent: false,
        screening: true,
    })
    .await;
    let quota = Arc::new(Mutex::new(10));
    let q = quota.clone();
    f._hub.register_fn("wechat-ilink","channel",move |_,_|{let q=q.clone();async move{Ok(json!({
        "id":"wechat-ilink","name":"微信","format":"plain","max_bytes":2000,"backup":false,"state":"ok",
        "quota_left":*q.lock().unwrap(),"last_received_at":"2026-10-21T15:13:57Z"
    }))}});
    f._hub.register_fn("admin","hub-admin",|_,_|async{Ok(json!({"plugins":[{"name":"custom-extension","version":"1","kind":"extension","status":"运行中","missing":[],"permissions":[],"granted":true,"enabled":true,"last_error":null,"config_fields":{"private":"must-not-be-exposed"}}]}))});
    f._hub.register_fn("llm", "llm", |method, _| async move {
        assert_eq!(method, "ready");
        Ok(json!({"ready":true}))
    });
    let (code, first) = f.jget("/api/v1/status").await;
    assert_eq!(code, 200);
    assert_eq!(first["channels"]["data"][0]["quota_left"], 10);
    assert_eq!(
        first["channels"]["data"][0]["last_received_at"],
        "2026-10-21T15:13:57Z"
    );
    assert_eq!(first["plugins"]["data"][0]["name"], "custom-extension");
    assert!(!first.to_string().contains("must-not-be-exposed"));
    *quota.lock().unwrap() = 9;
    f._hub
        .register_fn("collect", "source-status", |_, _| async {
            Err(plugin_sdk::ServiceError::unavailable("test source offline"))
        });
    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    let (_, second) = f.jget("/api/v1/status").await;
    assert_eq!(second["channels"]["data"][0]["quota_left"], 9);
    assert_eq!(second["sources"]["ok"], false);
    assert_eq!(second["plugins"]["ok"], true);
    assert_eq!(second["model"]["data"]["ready"], true);
}

#[tokio::test]
async fn unified_status_does_not_bypass_channel_permission_or_cached_owner_result() {
    let f = fixture(Opts {
        allowed: true,
        agent: false,
        screening: false,
    })
    .await;
    assert_eq!(f.jget("/api/v1/status").await.0, 200);
    f._hub.register_fn("auth", "auth", |_, p| async move {
        Ok(json!({"allowed":p["action"]!="channels.read"}))
    });
    assert_eq!(f.jget("/api/v1/status").await.0, 403);
}

#[tokio::test]
async fn unified_status_queue_counts_respect_permission_even_with_cached_snapshot() {
    let f = fixture(Opts {
        allowed: true,
        agent: false,
        screening: false,
    })
    .await;
    f._hub.register_fn("dispatch","dispatch",|method,_|async move{
        assert_eq!(method,"queue_summary");Ok(json!({"total":{"channel":"","pending":8,"ready":3,"delayed":5,"manual":2},"channels":[],"merging_events":4}))
    });
    let (_, first) = f.jget("/api/v1/status").await;
    assert_eq!(first["queue"]["data"]["total"]["pending"], 8);
    f._hub.register_fn("auth", "auth", |_, p| async move {
        Ok(json!({"allowed":p["action"]!="outbox.read"}))
    });
    let (code, hidden) = f.jget("/api/v1/status").await;
    assert_eq!(code, 200);
    assert_eq!(hidden["queue"]["ok"], false);
    assert!(hidden["queue"]["data"].is_null());
}

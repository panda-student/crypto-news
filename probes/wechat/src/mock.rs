//! `--mock`：在本机起一个模拟的 iLink 服务（wiremock），让 login / listen / push 不连腾讯也能完整跑通。
//!
//! 模拟的行为（均为假设，真实服务的错误码以实测为准）：
//! - 二维码：先 wait，再 scaned，再 confirmed 并下发 bot_token；
//! - getupdates：第一次调用返回一条用户消息和 context_token，之后返回空；
//! - sendmessage：每个 context_token 最多 10 条推送，第 11 条起返回 ret=-2（配额）；
//!   context_token 签发超过 24 小时返回 ret=-14（失效）。发送"已收到"这条回复不计入配额；
//! - 时间可模拟：`elapsed_hours` 会加到服务端的"当前时间"上。
//!
//! token 里编码了签发时间（`mocktok.<毫秒>` / `mockctx.<毫秒>`），配额计数存在文件里，
//! 所以 login、listen、push 是不同进程时状态仍然连续。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::timeutil::now_ms;

pub const MAX_PUSHES: usize = 10;
pub const CTX_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// listen 里对用户消息的回复文案；mock 里不计入配额。
pub const ACK_TEXT: &str = "已收到";
pub const TOKEN_PREFIX: &str = "mocktok.";
pub const CTX_PREFIX: &str = "mockctx.";

/// 一次 sendmessage 的判定结果，纯函数，便于单测。
pub fn decide_send(
    ctx: &str,
    text: &str,
    server_now_ms: i64,
    counts: &mut BTreeMap<String, usize>,
) -> Value {
    let Some(issued) = ctx
        .strip_prefix(CTX_PREFIX)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return json!({"ret": -1, "errmsg": "invalid context_token (mock)"});
    };
    if server_now_ms - issued > CTX_TTL_MS {
        return json!({"ret": -14, "errmsg": "context_token expired, session invalid (mock)"});
    }
    if text == ACK_TEXT {
        return json!({"ret": 0});
    }
    let used = counts.entry(ctx.to_string()).or_insert(0);
    if *used >= MAX_PUSHES {
        return json!({"ret": -2, "errmsg": "push quota exceeded for this context (mock)"});
    }
    *used += 1;
    json!({"ret": 0, "message_id": format!("mock-msg-{used}")})
}

pub struct MockOptions {
    /// 模拟已过去的小时数，加到服务端"当前时间"上。
    pub elapsed_hours: f64,
    pub quota_file: PathBuf,
}

fn load_counts(file: &PathBuf) -> BTreeMap<String, usize> {
    fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn ok_json(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// 启动模拟服务，返回服务（保持存活）。地址用 `server.uri()`。
pub async fn start(opts: MockOptions) -> MockServer {
    let server = MockServer::start().await;
    let uri = server.uri();
    let elapsed_ms = (opts.elapsed_hours * 3_600_000.0) as i64;

    // 二维码
    Mock::given(method("POST"))
        .and(path("/ilink/bot/get_bot_qrcode"))
        .respond_with(ok_json(json!({
            "qrcode": "mock-qrcode-1",
            "qrcode_img_content": "https://mock.invalid/liteapp/qr/mock-qrcode-1"
        })))
        .mount(&server)
        .await;

    // 扫码状态：wait -> scaned -> confirmed
    let polls = Arc::new(AtomicUsize::new(0));
    let uri_for_status = uri.clone();
    Mock::given(method("GET"))
        .and(path("/ilink/bot/get_qrcode_status"))
        .respond_with(move |_: &Request| {
            let n = polls.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => ok_json(json!({"status": "wait"})),
                1 => ok_json(json!({"status": "scaned"})),
                _ => ok_json(json!({
                    "status": "confirmed",
                    "bot_token": format!("{TOKEN_PREFIX}{}", now_ms()),
                    "ilink_bot_id": "mock-bot@im.bot",
                    "ilink_user_id": "mock-user@im.wechat",
                    "baseurl": uri_for_status,
                })),
            }
        })
        .mount(&server)
        .await;

    // 收消息：只下发一次用户消息
    let delivered = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(move |_: &Request| {
            if delivered.swap(true, Ordering::SeqCst) {
                // 之后模拟长轮询：隔一会儿返回空
                return ok_json(json!({"ret": 0, "msgs": [], "get_updates_buf": "mock-cursor-1"}))
                    .set_delay(Duration::from_secs(2));
            }
            let t = now_ms();
            ok_json(json!({
                "ret": 0,
                "get_updates_buf": "mock-cursor-1",
                "msgs": [{
                    "message_id": 1,
                    "from_user_id": "mock-user@im.wechat",
                    "to_user_id": "mock-bot@im.bot",
                    "client_id": "mock-client-1",
                    "create_time_ms": t,
                    "message_type": 1,
                    "message_state": 2,
                    "context_token": format!("{CTX_PREFIX}{t}"),
                    "item_list": [{"type": 1, "text_item": {"text": "你好（mock 用户消息）"}}]
                }]
            }))
        })
        .mount(&server)
        .await;

    // notifystart / notifystop
    for p in ["/ilink/bot/msg/notifystart", "/ilink/bot/msg/notifystop"] {
        Mock::given(method("POST"))
            .and(path(p))
            .respond_with(ok_json(json!({"ret": 0})))
            .mount(&server)
            .await;
    }

    // 发消息：配额与失效判定
    let counts_lock = Arc::new(Mutex::new(()));
    let quota_file = opts.quota_file;
    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let msg = &body["msg"];
            let ctx = msg["context_token"].as_str().unwrap_or("");
            let text = msg["item_list"][0]["text_item"]["text"]
                .as_str()
                .unwrap_or("");
            let _guard = counts_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut counts = load_counts(&quota_file);
            let out = decide_send(ctx, text, now_ms() + elapsed_ms, &mut counts);
            if let Some(parent) = quota_file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&quota_file, serde_json::to_vec(&counts).unwrap_or_default());
            ok_json(out)
        })
        .mount(&server)
        .await;

    server
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eleventh_push_hits_quota_and_reply_is_free() {
        let mut counts = BTreeMap::new();
        let ctx = format!("{CTX_PREFIX}1000");
        assert_eq!(decide_send(&ctx, ACK_TEXT, 2000, &mut counts)["ret"], 0);
        for i in 1..=MAX_PUSHES {
            let r = decide_send(&ctx, "x", 2000, &mut counts);
            assert_eq!(r["ret"], 0, "push {i}");
        }
        assert_eq!(decide_send(&ctx, "x", 2000, &mut counts)["ret"], -2);
        assert_eq!(decide_send(&ctx, "x", 2000, &mut counts)["ret"], -2);
    }

    #[test]
    fn old_context_is_invalid() {
        let mut counts = BTreeMap::new();
        let ctx = format!("{CTX_PREFIX}1000");
        let r = decide_send(&ctx, "x", 1000 + CTX_TTL_MS + 1, &mut counts);
        assert_eq!(r["ret"], -14);
        let r = decide_send(&ctx, "x", 1000 + CTX_TTL_MS - 1, &mut counts);
        assert_eq!(r["ret"], 0);
    }

    #[test]
    fn garbage_context_rejected() {
        let mut counts = BTreeMap::new();
        assert_eq!(decide_send("nope", "x", 5, &mut counts)["ret"], -1);
    }
}

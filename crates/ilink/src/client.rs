//! 协议客户端：每个方法对应一个接口，一次调用一次请求。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::RequestBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::types::{Context, Credentials, Message, QrCode, QrStatus, SendReceipt, Updates};

/// 客户端配置。所有地址都从这里（或凭证）来。
#[derive(Debug, Clone)]
pub struct Config {
    /// 获取二维码、轮询扫码状态的主机，例如 `https://ilinkai.weixin.qq.com`。
    pub qr_base_url: String,
    /// 可选的 `SKRouteTag` 请求头。
    pub route_tag: Option<String>,
    /// `base_info.channel_version`。
    pub channel_version: String,
    /// 请求头 `iLink-App-ClientVersion`（十进制数字，版本号按 0x00MMNNPP 打包）。
    pub client_version: String,
    /// `base_info.bot_agent`。
    pub bot_agent: String,
    /// 扫码状态轮询的请求超时；超时按 `wait` 处理。
    pub qr_poll_timeout: Duration,
    /// 长轮询收消息的请求超时；超时按“没有新消息”处理。
    pub updates_timeout: Duration,
    /// 其余请求（取二维码、发消息）的超时。
    pub request_timeout: Duration,
}

impl Config {
    pub fn new(qr_base_url: impl Into<String>) -> Self {
        Self {
            qr_base_url: qr_base_url.into(),
            route_tag: None,
            channel_version: "0.6.0".to_string(),
            client_version: "1536".to_string(),
            bot_agent: "OpenClaw".to_string(),
            qr_poll_timeout: Duration::from_secs(35),
            updates_timeout: Duration::from_secs(45),
            request_timeout: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    cfg: Config,
}

#[derive(Deserialize)]
struct QrStatusBody {
    #[serde(default)]
    status: String,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    ilink_user_id: Option<String>,
    baseurl: Option<String>,
    redirect_host: Option<String>,
}

#[derive(Deserialize)]
struct UpdatesBody {
    #[serde(default)]
    msgs: Vec<Message>,
    #[serde(default)]
    get_updates_buf: String,
}

fn trim_base(url: &str) -> &str {
    url.trim_end_matches('/')
}

impl Client {
    /// `http` 由调用方构造（代理、TLS、全局超时都在那里设置）。
    pub fn new(http: reqwest::Client, cfg: Config) -> Self {
        Self { http, cfg }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn common(&self, rb: RequestBuilder) -> RequestBuilder {
        let rb = rb
            .header("iLink-App-Id", "bot")
            .header("iLink-App-ClientVersion", &self.cfg.client_version);
        match &self.cfg.route_tag {
            Some(tag) => rb.header("SKRouteTag", tag),
            None => rb,
        }
    }

    fn authed_post(&self, creds: &Credentials, path: &str) -> RequestBuilder {
        let url = format!("{}{path}", trim_base(&creds.base_url));
        self.common(self.http.post(url))
            .header("Content-Type", "application/json")
            .header("AuthorizationType", "ilink_bot_token")
            .header("Authorization", format!("Bearer {}", creds.token))
            .header("X-WECHAT-UIN", random_uin())
    }

    fn base_info(&self) -> Value {
        json!({"channel_version": self.cfg.channel_version, "bot_agent": self.cfg.bot_agent})
    }

    /// 发送请求并解出 JSON。HTTP 非 2xx 时先看响应体里有没有 -2 / -14，否则返回 `HttpStatus`。
    async fn send_json(&self, rb: RequestBuilder) -> Result<Value> {
        let resp = rb.send().await.map_err(Error::network)?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(Error::network)?;
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        if !status.is_success() {
            if let Some(v) = &parsed {
                if let Some(e) = code_error(v) {
                    return Err(e);
                }
            }
            return Err(Error::HttpStatus {
                status: status.as_u16(),
            });
        }
        parsed.ok_or_else(|| Error::Decode("response is not valid JSON".to_string()))
    }

    /// 获取登录二维码。`POST {qr_base_url}/ilink/bot/get_bot_qrcode?bot_type=3`。
    pub async fn get_qr_code(&self) -> Result<QrCode> {
        let url = format!(
            "{}/ilink/bot/get_bot_qrcode",
            trim_base(&self.cfg.qr_base_url)
        );
        let rb = self
            .common(self.http.post(url))
            .query(&[("bot_type", "3")])
            .timeout(self.cfg.request_timeout)
            .json(&json!({"local_token_list": []}));
        let v = self.send_json(rb).await?;
        if let Some(e) = code_error(&v) {
            return Err(e);
        }
        serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string()))
    }

    /// 轮询一次扫码状态。`poll_base_url` 一开始用 `config().qr_base_url`；
    /// 收到 `ScanedButRedirect` 后改用 `https://<redirect_host>`。请求超时按 `Wait` 处理。
    pub async fn poll_qr_status(
        &self,
        poll_base_url: &str,
        qrcode: &str,
        verify_code: Option<&str>,
    ) -> Result<QrStatus> {
        let url = format!("{}/ilink/bot/get_qrcode_status", trim_base(poll_base_url));
        let mut query = vec![("qrcode", qrcode)];
        if let Some(code) = verify_code {
            query.push(("verify_code", code));
        }
        let rb = self
            .common(self.http.get(url))
            .query(&query)
            .timeout(self.cfg.qr_poll_timeout);
        let v = match self.send_json(rb).await {
            Ok(v) => v,
            Err(Error::Network(e)) if e.is_timeout() => return Ok(QrStatus::Wait),
            Err(e) => return Err(e),
        };
        if let Some(e) = code_error(&v) {
            return Err(e);
        }
        let body: QrStatusBody =
            serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string()))?;
        Ok(match body.status.as_str() {
            "wait" | "" => QrStatus::Wait,
            "scaned" => QrStatus::Scaned,
            "expired" => QrStatus::Expired,
            "need_verifycode" => QrStatus::NeedVerifyCode,
            "verify_code_blocked" => QrStatus::VerifyCodeBlocked,
            "binded_redirect" => QrStatus::BindedRedirect,
            "scaned_but_redirect" => QrStatus::ScanedButRedirect {
                redirect_host: body.redirect_host.unwrap_or_default(),
            },
            "confirmed" => {
                let token = body
                    .bot_token
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| Error::Decode("confirmed without bot_token".to_string()))?;
                QrStatus::Confirmed(Credentials {
                    token,
                    base_url: body
                        .baseurl
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| poll_base_url.to_string()),
                    account_id: body.ilink_bot_id.unwrap_or_default(),
                    user_id: body.ilink_user_id.unwrap_or_default(),
                    saved_at: OffsetDateTime::now_utc().format(&Rfc3339).ok(),
                })
            }
            other => QrStatus::Unknown(other.to_string()),
        })
    }

    /// 长轮询收消息。`POST {base}/ilink/bot/getupdates`。
    /// 请求超时视为没有新消息：返回空结果，游标不变。
    pub async fn get_updates(&self, creds: &Credentials, cursor: &str) -> Result<Updates> {
        let rb = self
            .authed_post(creds, "/ilink/bot/getupdates")
            .timeout(self.cfg.updates_timeout)
            .json(&json!({"get_updates_buf": cursor, "base_info": self.base_info()}));
        let v = match self.send_json(rb).await {
            Ok(v) => v,
            Err(Error::Network(e)) if e.is_timeout() => {
                return Ok(Updates {
                    messages: Vec::new(),
                    cursor: cursor.to_string(),
                    cursor_advanced: false,
                })
            }
            Err(e) => return Err(e),
        };
        if let Some(e) = code_error(&v) {
            return Err(e);
        }
        let body: UpdatesBody =
            serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string()))?;
        let advanced = !body.get_updates_buf.is_empty() && body.get_updates_buf != cursor;
        Ok(Updates {
            messages: body.msgs,
            cursor: if advanced {
                body.get_updates_buf
            } else {
                cursor.to_string()
            },
            cursor_advanced: advanced,
        })
    }

    /// 发一条文本。`POST {base}/ilink/bot/sendmessage`，收件人和 `context_token` 取自会话凭证。
    pub async fn send_text(
        &self,
        creds: &Credentials,
        ctx: &Context,
        text: &str,
    ) -> Result<SendReceipt> {
        let client_id = new_client_id();
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": ctx.user_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "context_token": ctx.context_token,
                "item_list": [{
                    "type": 1,
                    "msg_id": client_id,
                    "text_item": {"text": text}
                }]
            },
            "base_info": self.base_info()
        });
        let rb = self
            .authed_post(creds, "/ilink/bot/sendmessage")
            .timeout(self.cfg.request_timeout)
            .json(&body);
        let v = self.send_json(rb).await?;
        if let Some(e) = code_error(&v) {
            return Err(e);
        }
        let message_id = match v.get("message_id") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        Ok(SendReceipt {
            client_id,
            message_id,
        })
    }
}

/// 响应里 `errcode` 或 `ret` 非 0 时，转成对应的错误。
fn code_error(v: &Value) -> Option<Error> {
    let get = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0) as i32;
    let errcode = match get("errcode") {
        0 => get("ret"),
        c => c,
    };
    if errcode == 0 {
        return None;
    }
    let message = v
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(Error::from_code(errcode, message))
}

fn rand_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    h.write_u64(nanos);
    h.finish()
}

fn new_client_id() -> String {
    format!("ilink-{:016x}{:016x}", rand_u64(), rand_u64())
}

/// `X-WECHAT-UIN`：随机 u32 的十进制字符串再做 base64。
fn random_uin() -> String {
    base64(((rand_u64() & 0xFFFF_FFFF) as u32).to_string().as_bytes())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Client {
        Client::new(reqwest::Client::new(), Config::new(server.uri()))
    }

    fn creds(server: &MockServer) -> Credentials {
        Credentials {
            token: "tok-secret-1".into(),
            base_url: server.uri(),
            account_id: "bot".into(),
            user_id: "me".into(),
            saved_at: None,
        }
    }

    fn ctx() -> Context {
        Context {
            account_key: "bot".into(),
            user_id: "user-1".into(),
            context_token: "ctx-secret-2".into(),
            observed_at_unix_ms: 1,
            source_message_id: None,
        }
    }

    fn ok(body: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    #[test]
    fn base64_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"1234567"), "MTIzNDU2Nw==");
    }

    #[tokio::test]
    async fn qr_flow_wait_scaned_confirmed_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/get_bot_qrcode"))
            .and(query_param("bot_type", "3"))
            .and(header("iLink-App-Id", "bot"))
            .respond_with(ok(
                json!({"qrcode": "qr1", "qrcode_img_content": "https://x/qr1"}),
            ))
            .mount(&server)
            .await;
        for (status, extra) in [
            ("wait", json!({})),
            ("scaned", json!({})),
            ("expired", json!({})),
            (
                "confirmed",
                json!({"bot_token": "tok-secret-1", "ilink_bot_id": "bot@im.bot",
                       "ilink_user_id": "u@im.wechat", "baseurl": "http://api.example.invalid"}),
            ),
        ] {
            let mut body = json!({"status": status});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Mock::given(method("GET"))
                .and(path("/ilink/bot/get_qrcode_status"))
                .and(query_param("qrcode", format!("qr-{status}")))
                .respond_with(ok(body))
                .mount(&server)
                .await;
        }
        let c = client(&server);
        let qr = c.get_qr_code().await.unwrap();
        assert_eq!(
            (qr.qrcode.as_str(), qr.qrcode_img_content.as_str()),
            ("qr1", "https://x/qr1")
        );

        let base = c.config().qr_base_url.clone();
        let st = |q: &'static str| {
            let c = c.clone();
            let base = base.clone();
            async move { c.poll_qr_status(&base, q, None).await.unwrap() }
        };
        assert!(matches!(st("qr-wait").await, QrStatus::Wait));
        assert!(matches!(st("qr-scaned").await, QrStatus::Scaned));
        assert!(matches!(st("qr-expired").await, QrStatus::Expired));
        let QrStatus::Confirmed(cr) = st("qr-confirmed").await else {
            panic!("expected confirmed")
        };
        assert_eq!(cr.token, "tok-secret-1");
        assert_eq!(cr.base_url, "http://api.example.invalid");
        assert_eq!(cr.account_id, "bot@im.bot");
        assert!(cr.saved_at.is_some());
    }

    #[tokio::test]
    async fn qr_status_redirect_and_verify_code() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_qrcode_status"))
            .and(query_param("verify_code", "123456"))
            .respond_with(ok(
                json!({"status": "scaned_but_redirect", "redirect_host": "h2.example.invalid"}),
            ))
            .mount(&server)
            .await;
        let c = client(&server);
        let st = c
            .poll_qr_status(&server.uri(), "q", Some("123456"))
            .await
            .unwrap();
        assert!(
            matches!(st, QrStatus::ScanedButRedirect { redirect_host } if redirect_host == "h2.example.invalid")
        );
    }

    #[tokio::test]
    async fn get_updates_parses_and_advances_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/getupdates"))
            .and(header("Authorization", "Bearer tok-secret-1"))
            .and(header("AuthorizationType", "ilink_bot_token"))
            .respond_with(ok(json!({
                "ret": 0,
                "get_updates_buf": "cur-2",
                "msgs": [{
                    "message_id": 7, "from_user_id": "user-1", "message_type": 1,
                    "context_token": "ctx-secret-2",
                    "item_list": [{"type": 1, "text_item": {"text": "你好"}}]
                }]
            })))
            .mount(&server)
            .await;
        let c = client(&server);
        let u = c.get_updates(&creds(&server), "cur-1").await.unwrap();
        assert!(u.cursor_advanced);
        assert_eq!(u.cursor, "cur-2");
        assert_eq!(u.messages.len(), 1);
        let m = &u.messages[0];
        assert!(m.is_from_user());
        assert_eq!(m.text(), "你好");
        assert_eq!(m.message_id.as_deref(), Some("7"));
        let cx = m.context("bot", 5).unwrap();
        assert_eq!(
            (cx.user_id.as_str(), cx.context_token.as_str()),
            ("user-1", "ctx-secret-2")
        );

        // 请求体带游标和 base_info
        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["get_updates_buf"], "cur-1");
        assert!(body["base_info"]["channel_version"].is_string());
        assert!(reqs[0].headers.get("x-wechat-uin").is_some());
    }

    #[tokio::test]
    async fn get_updates_keeps_cursor_when_unchanged_or_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/getupdates"))
            .respond_with(ok(json!({"ret": 0, "get_updates_buf": "", "msgs": []})))
            .mount(&server)
            .await;
        let u = client(&server)
            .get_updates(&creds(&server), "cur-1")
            .await
            .unwrap();
        assert!(!u.cursor_advanced);
        assert_eq!(u.cursor, "cur-1");
        assert!(u.messages.is_empty());
    }

    #[tokio::test]
    async fn get_updates_timeout_is_empty_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/getupdates"))
            .respond_with(ok(json!({"ret": 0})).set_delay(Duration::from_millis(500)))
            .mount(&server)
            .await;
        let mut cfg = Config::new(server.uri());
        cfg.updates_timeout = Duration::from_millis(50);
        let c = Client::new(reqwest::Client::new(), cfg);
        let u = c.get_updates(&creds(&server), "cur-1").await.unwrap();
        assert_eq!(u.cursor, "cur-1");
        assert!(u.messages.is_empty());
    }

    #[tokio::test]
    async fn send_text_body_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/sendmessage"))
            .and(header("Authorization", "Bearer tok-secret-1"))
            .respond_with(ok(json!({"ret": 0, "message_id": "m1"})))
            .mount(&server)
            .await;
        let receipt = client(&server)
            .send_text(&creds(&server), &ctx(), "hello")
            .await
            .unwrap();
        assert_eq!(receipt.message_id.as_deref(), Some("m1"));
        let reqs = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let msg = &body["msg"];
        assert_eq!(msg["to_user_id"], "user-1");
        assert_eq!(msg["from_user_id"], "");
        assert_eq!(msg["context_token"], "ctx-secret-2");
        assert_eq!(msg["message_type"], 2);
        assert_eq!(msg["message_state"], 2);
        assert_eq!(msg["client_id"], receipt.client_id.as_str());
        assert_eq!(msg["item_list"][0]["type"], 1);
        assert_eq!(msg["item_list"][0]["msg_id"], receipt.client_id.as_str());
        assert_eq!(msg["item_list"][0]["text_item"]["text"], "hello");
        assert!(body["base_info"]["bot_agent"].is_string());
    }

    #[tokio::test]
    async fn error_codes_map_to_classes() {
        let server = MockServer::start().await;
        let c = client(&server);
        let cr = creds(&server);
        let cx = ctx();
        let send = || c.send_text(&cr, &cx, "x");

        for (resp, check) in [
            (ok(json!({"ret": -2, "errmsg": "quota"})), "rate"),
            (ok(json!({"ret": -14, "errmsg": "expired"})), "expired"),
            (ok(json!({"ret": 0, "errcode": -14})), "expired"),
            (ok(json!({"ret": 5, "errmsg": "other"})), "api"),
            (ResponseTemplate::new(500), "http"),
            (
                ResponseTemplate::new(401).set_body_json(json!({"errcode": -14})),
                "expired",
            ),
            (
                ResponseTemplate::new(200).set_body_string("not json"),
                "decode",
            ),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .and(path("/ilink/bot/sendmessage"))
                .respond_with(resp)
                .mount(&server)
                .await;
            let err = send().await.unwrap_err();
            match (check, &err) {
                ("rate", Error::RateLimited { errcode: -2, .. }) => {}
                ("expired", Error::CredentialExpired { errcode: -14, .. }) => {}
                ("api", Error::Api { errcode: 5, .. }) => {}
                ("http", Error::HttpStatus { status: 500 }) => {}
                ("decode", Error::Decode(_)) => {}
                _ => panic!("{check}: unexpected {err:?}"),
            }
        }
    }

    #[tokio::test]
    async fn network_error_is_classified_and_hides_url() {
        let c = Client::new(reqwest::Client::new(), Config::new("http://127.0.0.1:1"));
        let err = c.get_qr_code().await.unwrap_err();
        assert!(matches!(err, Error::Network(_)));
        assert!(!err.to_string().contains("127.0.0.1"));
    }

    #[test]
    fn debug_output_is_redacted() {
        let cr = Credentials {
            token: "tok-secret-1".into(),
            base_url: "https://x".into(),
            account_id: "a".into(),
            user_id: "u".into(),
            saved_at: None,
        };
        let s = format!("{cr:?} {:?}", ctx());
        assert!(
            !s.contains("tok-secret-1") && !s.contains("ctx-secret-2"),
            "{s}"
        );
        let m: Message = serde_json::from_value(json!({"context_token": "ctx-secret-2"})).unwrap();
        assert!(!format!("{m:?}").contains("ctx-secret-2"));
        let err = Error::from_code(-14, "boom".into());
        assert!(!err.to_string().contains("tok-secret-1"));
    }

    #[test]
    fn credentials_json_compatible_with_old_state_files() {
        let old = r#"{"token":"t","baseUrl":"http://h","accountId":"a","userId":"u","saved_at":"2026-01-01T00:00:00Z"}"#;
        let c: Credentials = serde_json::from_str(old).unwrap();
        assert_eq!(
            (c.base_url.as_str(), c.account_id.as_str()),
            ("http://h", "a")
        );
        let c2: Credentials =
            serde_json::from_str(r#"{"token":"t","base_url":"http://h"}"#).unwrap();
        assert_eq!(c2.user_id, "");
        let round = serde_json::to_string(&c).unwrap();
        assert!(round.contains("baseUrl"));
    }
}

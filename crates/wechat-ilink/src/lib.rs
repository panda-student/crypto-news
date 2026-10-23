//! wechat-ilink 插件：经 iLink 协议扫码绑定个人微信，提供 `channel` 服务（纯文本推送），
//! 接收用户消息并发出 `user.message`；管理“用户回一条、机器人可发 N 条”的额度和会话过期。

mod state;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ilink::{Credentials, QrStatus};
use plugin_sdk::contracts::channel::{
    BindInfo, ChannelInfo, ChannelState, SendRequest, SendResult, TextFormat,
};
use plugin_sdk::contracts::events::{ChannelStatus, UserMessage, CHANNEL_STATUS, USER_MESSAGE};
use plugin_sdk::contracts::secrets::SecretsClient;
use plugin_sdk::contracts::{
    format_rfc3339, new_trace_id, parse_params, parse_rfc3339, to_result, OWNER,
};
use plugin_sdk::{BoxFuture, Caller, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;

use state::{compute_state, Computed, StateInput, OFFLINE_AFTER_FAILURES};

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "
CREATE TABLE wechat_session (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    cursor TEXT,
    quota_left INTEGER,
    last_user_at TEXT,
    logged_in_at TEXT
);
";

const KEY_CREDENTIALS: &str = "wechat-ilink.credentials";
const KEY_CONTEXT: &str = "wechat-ilink.context";
const MAX_BYTES: usize = 2000;
const QUOTA_REMINDER: &str = "⚠️ 推送额度即将用完，回复任意内容恢复";
const BIND_TIMEOUT: Duration = Duration::from_secs(300);
const BIND_POLL_INTERVAL: Duration = Duration::from_secs(2);
const BIND_MAX_ERRORS: u32 = 5;

#[derive(Deserialize)]
#[serde(default)]
struct Cfg {
    qr_base_url: String,
    quota_per_reply: i64,
    session_hours: i64,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            qr_base_url: "https://ilinkai.weixin.qq.com".to_string(),
            quota_per_reply: 10,
            session_hours: 24,
        }
    }
}

#[derive(Default)]
pub struct WechatIlinkPlugin;

impl Plugin for WechatIlinkPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let sh = init(&ctx).await?;
            let s = sh.clone();
            ctx.spawn("receive", async move { s.receive_loop().await });
            sh.refresh_status().await;
            Ok(())
        })
    }
}

/// 建表、读取已保存的状态、登记 `channel` 服务。不启动后台循环（测试直接用）。
async fn init(ctx: &Context) -> Result<Arc<Shared>, PluginError> {
    let cfg: Cfg = if ctx.config().is_null() {
        Cfg::default()
    } else {
        ctx.config_as()?
    };
    ctx.store().migrate(1, SCHEMA).await?;
    let secrets = SecretsClient(ctx.service(plugin_sdk::contracts::secrets::SERVICE));
    let mut rt = Runtime {
        creds: load_secret(&secrets, KEY_CREDENTIALS).await?,
        session: load_secret(&secrets, KEY_CONTEXT).await?,
        ..Runtime::default()
    };
    let rows = ctx
        .store()
        .query(
            "SELECT cursor, quota_left, last_user_at, logged_in_at FROM wechat_session WHERE id = 1",
            vec![],
        )
        .await?;
    if let Some(r) = rows.first() {
        rt.cursor = r
            .get("cursor")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        rt.quota_left = r.get("quota_left").and_then(Value::as_i64).unwrap_or(0);
        rt.last_user_at = r
            .get("last_user_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        rt.logged_in_at = r
            .get("logged_in_at")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let client = ilink::Client::new(ctx.http().clone(), ilink::Config::new(&cfg.qr_base_url));
    let sh = Arc::new(Shared {
        ctx: ctx.clone(),
        client,
        secrets,
        cfg,
        rt: Mutex::new(rt),
        send_lock: tokio::sync::Mutex::new(()),
        bind: Mutex::new(BindSlot::default()),
    });
    let s = sh.clone();
    ctx.provide_fn("channel", move |caller, method, params| {
        let s = s.clone();
        async move { s.handle(caller, &method, params).await }
    })?;
    Ok(sh)
}

/// 读取并解析 JSON 形式的密钥；内容损坏时当作没有（只记日志，不带内容）。
async fn load_secret<T: serde::de::DeserializeOwned>(
    secrets: &SecretsClient,
    key: &str,
) -> Result<Option<T>, PluginError> {
    let Some(raw) = secrets.get(key).await? else {
        return Ok(None);
    };
    match serde_json::from_str(&raw) {
        Ok(v) => Ok(Some(v)),
        Err(_) => {
            tracing::warn!(key, "已保存的凭证无法解析，按未绑定处理");
            Ok(None)
        }
    }
}

#[derive(Default)]
struct Runtime {
    creds: Option<Credentials>,
    session: Option<ilink::Context>,
    cursor: String,
    quota_left: i64,
    last_user_at: Option<OffsetDateTime>,
    logged_in_at: Option<String>,
    failures: u32,
    rejected: bool,
    last_emitted: Option<ChannelState>,
}

impl Runtime {
    fn computed(&self, now: OffsetDateTime, session_hours: i64) -> Computed {
        compute_state(&StateInput {
            has_credentials: self.creds.is_some(),
            has_context: self.session.is_some(),
            quota_left: self.quota_left,
            last_user_at: self.last_user_at,
            now,
            session_hours,
            receive_failing: self.failures >= OFFLINE_AFTER_FAILURES,
            rejected: self.rejected,
        })
    }
}

#[derive(Debug, Clone)]
enum Phase {
    Waiting,
    Scanned,
    Done,
    Failed(String),
}

struct BindState {
    qrcode: String,
    qr_content: String,
    poll_base: String,
    phase: Phase,
    errors: u32,
}

#[derive(Default)]
struct BindSlot {
    generation: u64,
    active: Option<BindState>,
}

struct Shared {
    ctx: Context,
    client: ilink::Client,
    secrets: SecretsClient,
    cfg: Cfg,
    rt: Mutex<Runtime>,
    /// 串行化发送，保证额度计数准确
    send_lock: tokio::sync::Mutex<()>,
    bind: Mutex<BindSlot>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 把 iLink 错误转成不含服务端文本和凭证的简短说明。
fn describe(e: &ilink::Error) -> String {
    match e {
        ilink::Error::Network(_) => "网络错误".to_string(),
        ilink::Error::HttpStatus { status } => format!("HTTP 状态 {status}"),
        ilink::Error::Api { errcode, .. } => format!("协议错误 {errcode}"),
        ilink::Error::CredentialExpired { errcode, .. } => format!("凭证失效 {errcode}"),
        ilink::Error::RateLimited { errcode, .. } => format!("额度或限流 {errcode}"),
        ilink::Error::Decode(_) => "响应格式错误".to_string(),
    }
}

fn now_ms(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

fn require_management(caller: &Caller) -> Result<(), ServiceError> {
    if caller.user_id == plugin_sdk::contracts::OWNER
        && matches!(caller.plugin.as_str(), "web" | "operations")
    {
        Ok(())
    } else {
        Err(ServiceError::forbidden("只允许界面或已确认的应用操作调用"))
    }
}

impl Shared {
    async fn handle(
        self: &Arc<Self>,
        caller: Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        match method {
            "info" => to_result(&self.info()),
            "send" => {
                let req: SendRequest = parse_params(params)?;
                to_result(&self.send(req).await?)
            }
            "bind" => {
                require_management(&caller)?;
                to_result(&self.bind().await?)
            }
            "bind_status" => {
                require_management(&caller)?;
                to_result(&self.bind_info())
            }
            m => Err(ServiceError::method_not_found(m)),
        }
    }

    fn computed(&self) -> Computed {
        lock(&self.rt).computed(OffsetDateTime::now_utc(), self.cfg.session_hours)
    }

    fn info(&self) -> ChannelInfo {
        let (c, quota, last_received) = {
            let rt = lock(&self.rt);
            (
                rt.computed(OffsetDateTime::now_utc(), self.cfg.session_hours),
                rt.quota_left,
                rt.last_user_at.map(format_rfc3339),
            )
        };
        ChannelInfo {
            id: self.ctx.name().to_string(),
            name: "微信".to_string(),
            format: TextFormat::Plain,
            max_bytes: MAX_BYTES,
            backup: false,
            state: c.state,
            quota_left: Some(quota),
            last_received_at: last_received,
            quota_limit: Some(self.cfg.quota_per_reply),
            expires_at: c.expires_at,
            detail: c.detail,
        }
    }

    /// 状态与上次发出的不同时发 `channel.status`。
    async fn refresh_status(&self) {
        let (c, quota, changed) = {
            let mut rt = lock(&self.rt);
            let c = rt.computed(OffsetDateTime::now_utc(), self.cfg.session_hours);
            let changed = rt.last_emitted != Some(c.state);
            if changed {
                rt.last_emitted = Some(c.state);
            }
            (c, rt.quota_left, changed)
        };
        if !changed {
            return;
        }
        let ev = ChannelStatus {
            channel: self.ctx.name().to_string(),
            state: c.state,
            detail: c.detail,
            quota_left: Some(quota),
            expires_at: c.expires_at,
        };
        if let Err(e) = self.ctx.publish(CHANNEL_STATUS, &ev).await {
            tracing::warn!(error = %e, "发出 channel.status 失败");
        }
    }

    /// 把会话状态写进 `wechat_session`。
    async fn persist(&self) {
        let (cursor, quota, last_user_at, logged_in_at) = {
            let rt = lock(&self.rt);
            (
                rt.cursor.clone(),
                rt.quota_left,
                rt.last_user_at.map(format_rfc3339),
                rt.logged_in_at.clone(),
            )
        };
        let r = self
            .ctx
            .store()
            .execute(
                "INSERT OR REPLACE INTO wechat_session (id, cursor, quota_left, last_user_at, logged_in_at) VALUES (1, ?, ?, ?, ?)",
                vec![json!(cursor), json!(quota), json!(last_user_at), json!(logged_in_at)],
            )
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "保存微信会话状态失败");
        }
    }

    // ---------------------------------------------------------------- 发送

    async fn send(&self, req: SendRequest) -> Result<SendResult, ServiceError> {
        if req.text.len() > MAX_BYTES {
            return Err(ServiceError::invalid_params(format!(
                "正文超过 {MAX_BYTES} 字节上限"
            )));
        }
        let _guard = self.send_lock.lock().await;
        let (c, creds, session, quota) = {
            let rt = lock(&self.rt);
            (
                rt.computed(OffsetDateTime::now_utc(), self.cfg.session_hours),
                rt.creds.clone(),
                rt.session.clone(),
                rt.quota_left,
            )
        };
        match c.state {
            ChannelState::Unbound | ChannelState::Expired => {
                return Err(ServiceError::credential_expired(
                    c.detail.unwrap_or_else(|| "微信渠道不可用".to_string()),
                ));
            }
            ChannelState::QuotaExhausted => {
                return Err(ServiceError::quota_exhausted(
                    c.detail.unwrap_or_else(|| "推送额度已用完".to_string()),
                ));
            }
            // offline 只表示收消息失败，发送照常尝试，网络不通会返回 UNAVAILABLE
            ChannelState::Ok | ChannelState::Offline => {}
        }
        let (Some(creds), Some(session)) = (creds, session) else {
            return Err(ServiceError::credential_expired("微信渠道未绑定"));
        };
        let mut text = req.text;
        if quota == 1 {
            let extra = format!("\n\n{QUOTA_REMINDER}");
            if text.len() + extra.len() <= MAX_BYTES {
                text.push_str(&extra);
            }
        }
        match self.client.send_text(&creds, &session, &text).await {
            Ok(receipt) => {
                let left = {
                    let mut rt = lock(&self.rt);
                    rt.quota_left = (rt.quota_left - 1).max(0);
                    rt.quota_left
                };
                self.persist().await;
                self.refresh_status().await;
                Ok(SendResult {
                    message_id: receipt.message_id,
                    quota_left: Some(left),
                })
            }
            Err(ilink::Error::RateLimited { .. }) => {
                lock(&self.rt).quota_left = 0;
                self.persist().await;
                self.refresh_status().await;
                Err(ServiceError::quota_exhausted("推送额度已用完"))
            }
            Err(ilink::Error::CredentialExpired { .. }) => {
                lock(&self.rt).rejected = true;
                self.refresh_status().await;
                Err(ServiceError::credential_expired(state::DETAIL_EXPIRED))
            }
            Err(e) => {
                let d = describe(&e);
                tracing::warn!(error = %d, "微信发送失败");
                Err(ServiceError::unavailable(format!("微信发送失败：{d}")))
            }
        }
    }

    // ---------------------------------------------------------------- 收消息

    async fn receive_loop(self: Arc<Self>) -> Result<(), PluginError> {
        loop {
            let pause = match self.poll_once().await {
                Polled::NoCredentials => Duration::from_secs(2),
                Polled::Failed => Duration::from_secs(5),
                Polled::Messages(0) => Duration::from_secs(1),
                Polled::Messages(_) => Duration::ZERO,
            };
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
        }
    }

    /// 长轮询一次并处理结果。
    async fn poll_once(&self) -> Polled {
        let (creds, cursor) = {
            let rt = lock(&self.rt);
            (rt.creds.clone(), rt.cursor.clone())
        };
        let Some(creds) = creds else {
            return Polled::NoCredentials;
        };
        match self.client.get_updates(&creds, &cursor).await {
            Ok(updates) => {
                lock(&self.rt).failures = 0;
                let count = updates.messages.len();
                for m in &updates.messages {
                    if m.is_from_user() {
                        self.on_user_message(m, &creds.account_id).await;
                    }
                }
                if updates.cursor_advanced {
                    lock(&self.rt).cursor = updates.cursor;
                    self.persist().await;
                }
                self.refresh_status().await;
                Polled::Messages(count)
            }
            Err(e) => {
                {
                    let mut rt = lock(&self.rt);
                    rt.failures = rt.failures.saturating_add(1);
                    if matches!(e, ilink::Error::CredentialExpired { .. }) {
                        rt.rejected = true;
                    }
                }
                tracing::warn!(error = %describe(&e), "微信收消息失败");
                self.refresh_status().await;
                Polled::Failed
            }
        }
    }

    async fn on_user_message(&self, m: &ilink::Message, account_key: &str) {
        let now = OffsetDateTime::now_utc();
        let Some(session) = m.context(account_key, now_ms(now)) else {
            tracing::debug!("用户消息没有会话凭证，忽略");
            return;
        };
        match serde_json::to_string(&session) {
            Ok(raw) => {
                if self.secrets.set(KEY_CONTEXT, &raw).await.is_err() {
                    tracing::warn!("保存会话凭证失败");
                }
            }
            Err(_) => tracing::warn!("序列化会话凭证失败"),
        }
        let conversation = session.user_id.clone();
        {
            let mut rt = lock(&self.rt);
            rt.session = Some(session);
            rt.quota_left = self.cfg.quota_per_reply;
            rt.last_user_at = Some(now);
            rt.rejected = false;
        }
        self.persist().await;
        let text = m.text();
        if !text.is_empty() {
            let ev = UserMessage {
                channel: self.ctx.name().to_string(),
                user_id: OWNER.to_string(),
                conversation,
                text,
                message_id: m.message_id.clone().unwrap_or_else(new_trace_id),
                received_at: format_rfc3339(now),
            };
            if let Err(e) = self.ctx.publish(USER_MESSAGE, &ev).await {
                tracing::warn!(error = %e, "发出 user.message 失败");
            }
        }
        self.refresh_status().await;
    }

    // ---------------------------------------------------------------- 绑定

    async fn bind(self: &Arc<Self>) -> Result<BindInfo, ServiceError> {
        let qr =
            self.client.get_qr_code().await.map_err(|e| {
                ServiceError::unavailable(format!("获取二维码失败：{}", describe(&e)))
            })?;
        let generation = {
            let mut slot = lock(&self.bind);
            slot.generation += 1;
            slot.active = Some(BindState {
                qrcode: qr.qrcode,
                qr_content: qr.qrcode_img_content.clone(),
                poll_base: self.client.config().qr_base_url.clone(),
                phase: Phase::Waiting,
                errors: 0,
            });
            slot.generation
        };
        let s = self.clone();
        self.ctx
            .spawn("bind", async move { s.bind_loop(generation).await });
        Ok(BindInfo {
            state: self.computed().state,
            qrcode: Some(qr.qrcode_img_content),
            qrcode_image: None,
            detail: Some("等待扫码".to_string()),
        })
    }

    /// 后台轮询扫码状态，直到成功、失败、超时或被新的绑定取代。始终返回 Ok，避免触发插件重启。
    async fn bind_loop(self: Arc<Self>, generation: u64) -> Result<(), PluginError> {
        let started = Instant::now();
        while self.bind_poll_once(generation).await {
            if started.elapsed() > BIND_TIMEOUT {
                self.set_phase(
                    generation,
                    Phase::Failed("扫码超时，请重新获取二维码".into()),
                );
                break;
            }
            tokio::time::sleep(BIND_POLL_INTERVAL).await;
        }
        Ok(())
    }

    fn set_phase(&self, generation: u64, phase: Phase) {
        let mut slot = lock(&self.bind);
        if slot.generation == generation {
            if let Some(b) = slot.active.as_mut() {
                b.phase = phase;
            }
        }
    }

    /// 轮询一次扫码状态；返回是否继续轮询。
    async fn bind_poll_once(&self, generation: u64) -> bool {
        let (qrcode, poll_base) = {
            let slot = lock(&self.bind);
            match slot.active.as_ref() {
                Some(b) if slot.generation == generation => {
                    if matches!(b.phase, Phase::Done | Phase::Failed(_)) {
                        return false;
                    }
                    (b.qrcode.clone(), b.poll_base.clone())
                }
                _ => return false,
            }
        };
        match self.client.poll_qr_status(&poll_base, &qrcode, None).await {
            Ok(QrStatus::Wait) | Ok(QrStatus::Unknown(_)) => true,
            Ok(QrStatus::Scaned) => {
                self.set_phase(generation, Phase::Scanned);
                true
            }
            Ok(QrStatus::ScanedButRedirect { redirect_host }) => {
                let mut slot = lock(&self.bind);
                if slot.generation == generation {
                    if let Some(b) = slot.active.as_mut() {
                        if !redirect_host.is_empty() {
                            b.poll_base = format!("https://{redirect_host}");
                        }
                        b.phase = Phase::Scanned;
                    }
                }
                true
            }
            Ok(QrStatus::Confirmed(creds)) => {
                let phase = match self.save_login(creds).await {
                    Ok(()) => Phase::Done,
                    Err(()) => Phase::Failed("保存登录凭证失败".to_string()),
                };
                self.set_phase(generation, phase);
                false
            }
            Ok(QrStatus::Expired) => {
                self.set_phase(generation, Phase::Failed("二维码已过期，请重新获取".into()));
                false
            }
            Ok(QrStatus::NeedVerifyCode) | Ok(QrStatus::VerifyCodeBlocked) => {
                self.set_phase(generation, Phase::Failed("需要验证码，暂不支持".into()));
                false
            }
            Ok(QrStatus::BindedRedirect) => {
                self.set_phase(generation, Phase::Failed("该账号已在别处绑定".into()));
                false
            }
            Err(e) => {
                tracing::warn!(error = %describe(&e), "轮询扫码状态失败");
                let mut slot = lock(&self.bind);
                if slot.generation != generation {
                    return false;
                }
                let Some(b) = slot.active.as_mut() else {
                    return false;
                };
                b.errors += 1;
                if b.errors >= BIND_MAX_ERRORS {
                    b.phase = Phase::Failed(format!("轮询扫码状态失败：{}", describe(&e)));
                    return false;
                }
                true
            }
        }
    }

    /// 登录成功：凭证存 secrets，重置会话状态（旧会话凭证属于旧登录）。
    async fn save_login(&self, creds: Credentials) -> Result<(), ()> {
        let raw = serde_json::to_string(&creds).map_err(|_| ())?;
        self.secrets
            .set(KEY_CREDENTIALS, &raw)
            .await
            .map_err(|_| ())?;
        if self.secrets.delete(KEY_CONTEXT).await.is_err() {
            tracing::warn!("清除旧会话凭证失败");
        }
        {
            let mut rt = lock(&self.rt);
            rt.creds = Some(creds);
            rt.session = None;
            rt.cursor.clear();
            rt.quota_left = 0;
            rt.last_user_at = None;
            rt.logged_in_at = Some(format_rfc3339(OffsetDateTime::now_utc()));
            rt.failures = 0;
            rt.rejected = false;
        }
        self.persist().await;
        self.refresh_status().await;
        Ok(())
    }

    fn bind_info(&self) -> BindInfo {
        let c = self.computed();
        let phase = {
            let slot = lock(&self.bind);
            slot.active
                .as_ref()
                .map(|b| (b.phase.clone(), b.qr_content.clone()))
        };
        let (qrcode, detail) = match phase {
            None => (None, c.detail),
            Some((Phase::Waiting, qr)) => (Some(qr), Some("等待扫码".to_string())),
            Some((Phase::Scanned, qr)) => (Some(qr), Some("已扫码，请在手机上确认".to_string())),
            Some((Phase::Done, _)) => (
                None,
                Some(match c.detail {
                    Some(d) => format!("绑定成功；{d}"),
                    None => "绑定成功".to_string(),
                }),
            ),
            Some((Phase::Failed(msg), _)) => (None, Some(msg)),
        };
        BindInfo {
            state: c.state,
            qrcode,
            qrcode_image: None,
            detail,
        }
    }
}

enum Polled {
    NoCredentials,
    Failed,
    Messages(usize),
}

//! 企业微信群机器人渠道（`channel` 1.0）：经 webhook 发 markdown 消息。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use plugin_sdk::contracts::channel::{
    ChannelInfo, ChannelState, SendRequest, SendResult, TextFormat, SERVICE,
};
use plugin_sdk::contracts::events::ChannelStatus;
use plugin_sdk::contracts::secrets::SecretsClient;
use plugin_sdk::contracts::{parse_params, to_result};
use plugin_sdk::{BoxFuture, Caller, Context, Plugin, PluginError, ServiceError, ServiceHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::Instant;

const MANIFEST: &str = include_str!("../plugin.toml");
const SECRET_KEY: &str = "wecom-webhook.webhook_key";
const DEFAULT_BASE_URL: &str = "https://qyapi.weixin.qq.com";
const MAX_BYTES: usize = 4000;
/// 两次发送的最小间隔。
const MIN_INTERVAL: Duration = Duration::from_secs(3);
const QUOTA_LIMIT: i64 = 20;
const QUOTA_WINDOW: Duration = Duration::from_secs(60);

/// 本进程成功发送记录；服务端没有余额查询，不能观察其他客户端的发送。
#[derive(Default)]
struct SendWindow(VecDeque<Instant>);

impl SendWindow {
    fn remaining_at(&mut self, now: Instant) -> i64 {
        while self
            .0
            .front()
            .is_some_and(|t| now.duration_since(*t) >= QUOTA_WINDOW)
        {
            self.0.pop_front();
        }
        QUOTA_LIMIT.saturating_sub(self.0.len() as i64).max(0)
    }

    fn record_at(&mut self, now: Instant) -> i64 {
        self.0.push_back(now);
        self.remaining_at(now)
    }
}

pub struct WecomWebhookPlugin;

impl Plugin for WecomWebhookPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let ch = Arc::new(Channel::new(&ctx, MIN_INTERVAL)?);
            let state = ch.current_state().await;
            ch.publish_if_changed(&ctx, state).await;
            let handler = Arc::new(Handler {
                ch,
                ctx: ctx.clone(),
            });
            ctx.provide(SERVICE, handler)
        })
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    backup: bool,
    base_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backup: true,
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

struct Channel {
    http: reqwest::Client,
    secrets: SecretsClient,
    plugin: String,
    base_url: String,
    backup: bool,
    interval: Duration,
    /// 上一次发送开始的时间；持锁期间串行发送。
    last_send: Mutex<Option<Instant>>,
    sent_window: Mutex<SendWindow>,
    /// 运行中观察到的状态（Ok 表示没有异常）。
    runtime: Mutex<ChannelState>,
    /// 最近一次发出的状态。
    published: Mutex<Option<ChannelState>>,
}

impl Channel {
    fn new(ctx: &Context, interval: Duration) -> Result<Self, PluginError> {
        let cfg: Config = if ctx.config().is_null() {
            Config::default()
        } else {
            ctx.config_as()?
        };
        Ok(Channel {
            http: ctx.http().clone(),
            secrets: SecretsClient(ctx.service("secrets")),
            plugin: ctx.name().to_string(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            backup: cfg.backup,
            interval,
            last_send: Mutex::new(None),
            sent_window: Mutex::new(SendWindow::default()),
            runtime: Mutex::new(ChannelState::Ok),
            published: Mutex::new(None),
        })
    }

    async fn key(&self) -> Result<Option<String>, ServiceError> {
        let k = self.secrets.get(SECRET_KEY).await?;
        k.filter(|k| !k.trim().is_empty())
            .map(|k| webhook_key(&k))
            .transpose()
    }

    async fn current_state(&self) -> ChannelState {
        match self.key().await {
            Ok(Some(_)) => *self.runtime.lock().await,
            Ok(None) => ChannelState::Unbound,
            Err(_) => ChannelState::Offline,
        }
    }

    async fn publish_if_changed(&self, ctx: &Context, state: ChannelState) {
        {
            let mut p = self.published.lock().await;
            if *p == Some(state) {
                return;
            }
            *p = Some(state);
        }
        let ev = ChannelStatus {
            channel: self.plugin.clone(),
            state,
            detail: state_detail(state),
            quota_left: None,
            expires_at: None,
        };
        if let Err(e) = ctx.publish("channel.status", &ev).await {
            tracing::warn!("发布 channel.status 失败：{e}");
        }
    }

    async fn info(&self) -> ChannelInfo {
        let state = self.current_state().await;
        let remaining = self.sent_window.lock().await.remaining_at(Instant::now());
        ChannelInfo {
            id: self.plugin.clone(),
            name: "企业微信群机器人".to_string(),
            format: TextFormat::Markdown,
            max_bytes: MAX_BYTES,
            backup: self.backup,
            state,
            quota_left: (state != ChannelState::Unbound).then_some(remaining),
            last_received_at: None,
            quota_limit: Some(QUOTA_LIMIT),
            expires_at: None,
            detail: state_detail(state),
        }
    }

    /// 发送，返回结果和发送后的渠道状态；同时更新运行状态。
    async fn send(&self, req: SendRequest) -> (Result<SendResult, ServiceError>, ChannelState) {
        let r = self.do_send(&req.text).await;
        let state = match &r {
            Ok(_) => ChannelState::Ok,
            Err(e) => match e.code {
                ServiceError::CREDENTIAL_EXPIRED => ChannelState::Expired,
                ServiceError::UNAVAILABLE => ChannelState::Offline,
                // 限流、参数错误不改变渠道状态
                _ => *self.runtime.lock().await,
            },
        };
        *self.runtime.lock().await = state;
        (r, self.current_state().await)
    }

    async fn do_send(&self, text: &str) -> Result<SendResult, ServiceError> {
        if text.is_empty() {
            return Err(ServiceError::invalid_params("正文为空"));
        }
        if text.len() > MAX_BYTES {
            return Err(ServiceError::invalid_params(format!(
                "正文 {} 字节，超过上限 {MAX_BYTES}",
                text.len()
            )));
        }
        let key = self
            .key()
            .await?
            .ok_or_else(|| ServiceError::credential_expired("未配置 webhook key"))?;

        let mut last = self.last_send.lock().await;
        if let Some(t) = *last {
            tokio::time::sleep_until(t + self.interval).await;
        }
        *last = Some(Instant::now());

        let body = json!({"msgtype": "markdown", "markdown": {"content": text}}).to_string();
        let url = format!("{}/cgi-bin/webhook/send", self.base_url);
        let resp = self
            .http
            .post(url)
            .query(&[("key", key.as_str())])
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| net_error(e, &key))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ServiceError::unavailable(format!(
                "企业微信返回 HTTP {}",
                status.as_u16()
            )));
        }
        let bytes = resp.bytes().await.map_err(|e| net_error(e, &key))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| ServiceError::unavailable("企业微信返回内容无法解析"))?;
        match v.get("errcode").and_then(Value::as_i64) {
            Some(0) => {
                let remaining = self.sent_window.lock().await.record_at(Instant::now());
                Ok(SendResult {
                    quota_left: Some(remaining),
                    ..SendResult::default()
                })
            }
            Some(code) => Err(map_errcode(code)),
            None => Err(ServiceError::unavailable("企业微信返回缺少 errcode")),
        }
    }
}

fn state_detail(state: ChannelState) -> Option<String> {
    match state {
        ChannelState::Ok => None,
        ChannelState::Unbound => Some("未配置 webhook key".to_string()),
        ChannelState::Expired => {
            Some("Webhook 链接无效，请核对机器人及链接（不是定时过期）".to_string())
        }
        ChannelState::Offline => Some("网络或服务异常".to_string()),
        ChannelState::QuotaExhausted => Some("额度用完".to_string()),
    }
}

/// 网络错误转换：去掉 URL，再把 key 抹掉。
fn net_error(e: reqwest::Error, key: &str) -> ServiceError {
    let msg = PluginError::from(e.without_url()).to_string();
    ServiceError::unavailable(format!("请求企业微信失败：{}", msg.replace(key, "<key>")))
}

/// 设置页接受完整地址；仅提取官方地址的 key，不把用户输入当请求目的地。
fn webhook_key(value: &str) -> Result<String, ServiceError> {
    let value = value.trim();
    if value.starts_with("https://") || value.starts_with("http://") {
        let url = reqwest::Url::parse(value)
            .map_err(|_| ServiceError::invalid_params("Webhook 地址格式不正确"))?;
        if url.scheme() != "https"
            || url.host_str() != Some("qyapi.weixin.qq.com")
            || url.port_or_known_default() != Some(443)
            || url.path() != "/cgi-bin/webhook/send"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(ServiceError::invalid_params(
                "请填写企业微信群机器人的官方 HTTPS Webhook 地址",
            ));
        }
        let keys: Vec<_> = url
            .query_pairs()
            .filter(|(name, _)| name == "key")
            .collect();
        if keys.len() != 1 || keys[0].1.trim().is_empty() {
            return Err(ServiceError::invalid_params(
                "Webhook 地址必须包含一个非空 key",
            ));
        }
        return Ok(keys[0].1.trim().to_string());
    }
    if value.is_empty()
        || value.contains(['/', '?', '&', '='])
        || value.chars().any(char::is_whitespace)
    {
        return Err(ServiceError::invalid_params(
            "请填写完整 Webhook 地址或单独的 key",
        ));
    }
    Ok(value.to_string())
}

fn map_errcode(code: i64) -> ServiceError {
    match code {
        45009 => ServiceError::rate_limited("企业微信限流（errcode 45009）"),
        40035 | 40058 => ServiceError::invalid_params("企业微信拒绝参数（errcode 40058）"),
        93000 | 40013 | 40014 | 42001 => {
            ServiceError::credential_expired(format!("webhook key 无效（errcode {code}）"))
        }
        _ => ServiceError::unavailable(format!("企业微信返回错误（errcode {code}）")),
    }
}

struct Handler {
    ch: Arc<Channel>,
    ctx: Context,
}

impl ServiceHandler for Handler {
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            match method {
                "info" => to_result(&self.ch.info().await),
                "send" => {
                    let req: SendRequest = parse_params(params)?;
                    let (r, state) = self.ch.send(req).await;
                    self.ch.publish_if_changed(&self.ctx, state).await;
                    to_result(&r?)
                }
                other => Err(ServiceError::method_not_found(other)),
            }
        })
    }
}

#[cfg(test)]
mod tests;

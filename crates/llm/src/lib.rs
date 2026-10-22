//! llm 插件：提供 `llm` 服务 1.2。多个模型服务（OpenAI 兼容与 Anthropic 原生）、用途分配、
//! 真流式输出与取消、测试连接，并按天、模型、用途记录用量。

mod anthropic;
mod error;
mod openai;
mod providers;
mod stream;
mod usage;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plugin_sdk::contracts::llm::{
    ChatMessage, ChatRequest, ChatResponse, LlmCancel, ProviderId, Ready, StreamChatRequest,
    StreamChatResponse, TestRequest, TestResult, TokenUsage, UsageQuery, LLM_CANCEL, SERVICE,
};
use plugin_sdk::contracts::{parse_params, to_result};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "
CREATE TABLE usage (
    day TEXT NOT NULL,
    model TEXT NOT NULL,
    purpose TEXT NOT NULL,
    calls INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, model, purpose)
);
";
const SCHEMA_PROVIDERS: &str = "
CREATE TABLE providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    base_url TEXT NOT NULL,
    models TEXT NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0,
    last_test TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE assignments (
    purpose TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL
);
";
/// 测试连接时的输出上限，只要能得到回复即可。
const TEST_MAX_TOKENS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Openai,
    Anthropic,
}

impl Kind {
    pub(crate) fn parse(s: &str) -> Option<Kind> {
        match s {
            "openai" => Some(Kind::Openai),
            "anthropic" => Some(Kind::Anthropic),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Openai => "openai",
            Kind::Anthropic => "anthropic",
        }
    }
}

/// 旧的单服务配置：只用于首次迁移成默认服务；`timeout_secs` 仍对所有调用生效。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    provider: Kind,
    base_url: String,
    small_model: String,
    large_model: String,
    api_key: Option<String>,
    timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            provider: Kind::Openai,
            base_url: "https://token.sensenova.cn/v1".into(),
            small_model: "sensenova-6.8-flash-lite".into(),
            large_model: String::new(),
            api_key: None,
            timeout_secs: 120,
        }
    }
}

/// 一次调用的目标：服务地址、模型和密钥。
pub(crate) struct Target {
    pub kind: Kind,
    pub base_url: String,
    pub model: String,
    pub key: String,
}

/// 适配器的统一输出。
pub(crate) struct RawReply {
    pub content: String,
    pub model: String,
    pub usage: TokenUsage,
}

/// 适配器失败：带上已消耗的用量（有的话），便于记账。
pub(crate) struct Failure {
    pub error: ServiceError,
    pub usage: TokenUsage,
}

impl From<ServiceError> for Failure {
    fn from(error: ServiceError) -> Self {
        Failure {
            error,
            usage: TokenUsage::default(),
        }
    }
}

struct State {
    ctx: Context,
    cfg: Config,
    /// 进行中的流：stream_id -> 取消通知
    streams: Mutex<HashMap<String, Arc<Notify>>>,
}

impl State {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.timeout_secs.max(1))
    }
}

/// 流结束（含出错、被中止）时把它从进行中列表移除。
struct StreamGuard<'a> {
    state: &'a State,
    id: &'a str,
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut m) = self.state.streams.lock() {
            m.remove(self.id);
        }
    }
}

#[derive(Default)]
pub struct LlmPlugin;

impl Plugin for LlmPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = if ctx.config().is_null() {
                Config::default()
            } else {
                ctx.config_as()?
            };
            ctx.store().migrate(1, SCHEMA).await?;
            ctx.store().migrate(2, SCHEMA_PROVIDERS).await?;
            if let Err(e) = providers::migrate_legacy(&ctx, &cfg).await {
                tracing::warn!("llm：导入旧配置失败：{}", e.message);
            }
            let state = Arc::new(State {
                ctx: ctx.clone(),
                cfg,
                streams: Mutex::new(HashMap::new()),
            });
            let s = state.clone();
            ctx.subscribe(LLM_CANCEL, move |e| {
                let s = s.clone();
                async move {
                    if let Ok(c) = e.parse::<LlmCancel>() {
                        let n = s
                            .streams
                            .lock()
                            .ok()
                            .and_then(|m| m.get(&c.stream_id).cloned());
                        if let Some(n) = n {
                            n.notify_one();
                        }
                    }
                    Ok(())
                }
            })?;
            ctx.provide_fn(SERVICE, move |_caller, method, params| {
                let state = state.clone();
                async move { handle(&state, &method, params).await }
            })?;
            Ok(())
        })
    }
}

async fn handle(state: &State, method: &str, params: Value) -> Result<Value, ServiceError> {
    let ctx = &state.ctx;
    match method {
        "chat" => to_result(&chat(state, parse_params(params)?).await?),
        "chat_stream" => to_result(&chat_stream(state, parse_params(params)?).await?),
        "test" => to_result(&test(state, parse_params(params)?).await?),
        "usage" => {
            let q: UsageQuery = parse_params(params)?;
            to_result(&usage::list(ctx, &q).await?)
        }
        "ready" => to_result(&Ready {
            ready: providers::ready(ctx).await?,
        }),
        "providers" => to_result(&providers::list(ctx).await?),
        "provider_save" => to_result(&providers::save(ctx, parse_params(params)?).await?),
        "provider_delete" => {
            let p: ProviderId = parse_params(params)?;
            providers::delete(ctx, &p.id).await?;
            Ok(json!({}))
        }
        "set_default" => {
            let p: ProviderId = parse_params(params)?;
            providers::set_default(ctx, &p.id).await?;
            Ok(json!({}))
        }
        "assign" => {
            providers::assign(ctx, parse_params(params)?).await?;
            Ok(json!({}))
        }
        m => Err(ServiceError::method_not_found(m)),
    }
}

/// json 模式下整理并校验输出。
fn finish_json(req: &ChatRequest, mut r: RawReply) -> Result<RawReply, Failure> {
    if req.json {
        match clean_json(&r.content) {
            Some(c) => r.content = c,
            None => {
                return Err(Failure {
                    error: ServiceError::unavailable("模型返回的不是 JSON"),
                    usage: r.usage,
                })
            }
        }
    }
    Ok(r)
}

fn response(r: RawReply) -> ChatResponse {
    ChatResponse {
        content: r.content,
        model: r.model,
        usage: r.usage,
    }
}

async fn chat(state: &State, req: ChatRequest) -> Result<ChatResponse, ServiceError> {
    let ctx = &state.ctx;
    let (row, model) = providers::resolve(ctx, req.purpose.as_deref(), req.tier).await?;
    let target = providers::target(ctx, &row, model).await?;
    let result = stream::chat(ctx, &target, &req, state.timeout())
        .await
        .and_then(|r| finish_json(&req, r));

    let purpose = req.purpose.as_deref().filter(|p| !p.is_empty());
    match result {
        Ok(r) => {
            usage::record(ctx, &r.model, purpose, r.usage, false).await;
            Ok(response(r))
        }
        Err(f) => {
            usage::record(ctx, &target.model, purpose, f.usage, true).await;
            Err(f.error)
        }
    }
}

async fn chat_stream(
    state: &State,
    sreq: StreamChatRequest,
) -> Result<StreamChatResponse, ServiceError> {
    let ctx = &state.ctx;
    let req = &sreq.request;
    let id = sreq.stream_id.as_str();
    if id.is_empty() {
        return Err(ServiceError::invalid_params("stream_id 不能为空"));
    }
    // 先登记再发请求，取消事件不会落空
    let cancel = Arc::new(Notify::new());
    state
        .streams
        .lock()
        .map_err(|_| ServiceError::internal("流状态异常"))?
        .insert(id.to_string(), cancel.clone());
    let _guard = StreamGuard { state, id };

    let (row, model) = providers::resolve(ctx, req.purpose.as_deref(), req.tier).await?;
    let target = providers::target(ctx, &row, model).await?;
    let result = stream::run(ctx, &target, req, id, state.timeout(), &cancel)
        .await
        .and_then(|o| {
            if o.cancelled {
                Ok(o)
            } else {
                finish_json(req, o.reply).map(|reply| stream::StreamOutcome {
                    reply,
                    cancelled: false,
                })
            }
        });

    let purpose = req.purpose.as_deref().filter(|p| !p.is_empty());
    match result {
        Ok(o) => {
            usage::record(ctx, &o.reply.model, purpose, o.reply.usage, false).await;
            Ok(StreamChatResponse {
                response: response(o.reply),
                cancelled: o.cancelled,
            })
        }
        Err(f) => {
            usage::record(ctx, &target.model, purpose, f.usage, true).await;
            Err(f.error)
        }
    }
}

/// 测试连接：失败原因放进 `TestResult.message`，不作为服务错误返回；不记用量。
async fn test(state: &State, req: TestRequest) -> Result<TestResult, ServiceError> {
    let ctx = &state.ctx;
    let Some((row, model)) = providers::test_subject(ctx, req.provider_id.as_deref()).await? else {
        return Ok(TestResult {
            ok: false,
            message: "还没有配置模型服务".into(),
            latency_ms: 0,
        });
    };
    let started = Instant::now();
    let outcome = match providers::target(ctx, &row, model).await {
        Ok(t) => {
            let probe = ChatRequest {
                messages: vec![ChatMessage::user("hi")],
                tier: Default::default(),
                json: false,
                max_tokens: Some(TEST_MAX_TOKENS),
                temperature: None,
                purpose: None,
            };
            stream::chat(ctx, &t, &probe, state.timeout())
                .await
                .map(|r| r.model)
                .map_err(|f| f.error)
        }
        Err(e) => Err(e),
    };
    let result = TestResult {
        ok: outcome.is_ok(),
        message: match outcome {
            Ok(model) => model,
            Err(e) => e.message,
        },
        latency_ms: started.elapsed().as_millis() as u64,
    };
    if let Err(e) = providers::save_test(ctx, &row.id, &result).await {
        tracing::warn!("llm：保存测试结果失败：{}", e.message);
    }
    Ok(result)
}

/// 去掉 ``` 围栏和首尾空白；不是合法 JSON 时返回 None。
fn clean_json(s: &str) -> Option<String> {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // 去掉围栏后的语言标记（第一行）
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        t = rest.trim_end();
        t = t.strip_suffix("```").unwrap_or(t);
        t = t.trim();
    }
    serde_json::from_str::<Value>(t).ok()?;
    Some(t.to_string())
}

#[cfg(test)]
mod tests;

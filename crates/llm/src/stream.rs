//! 调用模型：一次性调用与按行解析 SSE 的真流式调用（可取消）。

use std::time::Duration;

use plugin_sdk::contracts::llm::{ChatRequest, LlmDelta, TokenUsage, LLM_DELTA};
use plugin_sdk::{Context, ServiceError};
use reqwest::{RequestBuilder, Response};
use serde_json::Value;
use tokio::sync::Notify;

use crate::{anthropic, error, openai, Failure, Kind, RawReply, Target};

/// 流式解析过程中累积的状态。
#[derive(Default)]
pub(crate) struct StreamState {
    pub model: Option<String>,
    pub usage: TokenUsage,
}

impl StreamState {
    /// 读取 `{input_tokens, output_tokens}` 形式的用量，缺的字段保持原值。
    pub fn read_usage(&mut self, u: &Value) {
        if let Some(n) = u["input_tokens"].as_u64() {
            self.usage.input_tokens = n;
        }
        if let Some(n) = u["output_tokens"].as_u64() {
            self.usage.output_tokens = n;
        }
    }
}

pub(crate) struct StreamOutcome {
    pub reply: RawReply,
    pub cancelled: bool,
}

fn build(
    ctx: &Context,
    t: &Target,
    req: &ChatRequest,
    stream: bool,
) -> Result<RequestBuilder, ServiceError> {
    match t.kind {
        Kind::Openai => openai::request(ctx, t, req, stream),
        Kind::Anthropic => anthropic::request(ctx, t, req, stream),
    }
}

fn parse(t: &Target, text: &str) -> Result<RawReply, Failure> {
    match t.kind {
        Kind::Openai => openai::parse(text, &t.model),
        Kind::Anthropic => anthropic::parse(text, &t.model),
    }
}

fn transport(e: &reqwest::Error) -> ServiceError {
    error::from_transport(e.is_timeout(), e.is_connect())
}

/// 一次性调用。
pub(crate) async fn chat(
    ctx: &Context,
    t: &Target,
    req: &ChatRequest,
    timeout: Duration,
) -> Result<RawReply, Failure> {
    let resp = build(ctx, t, req, false)?
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| transport(&e))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| transport(&e))?;
    if !(200..300).contains(&status) {
        return Err(error::from_status(status, &text, &t.key).into());
    }
    parse(t, &text)
}

async fn emit(ctx: &Context, stream_id: &str, text: &str) {
    let d = LlmDelta {
        stream_id: stream_id.to_string(),
        text: text.to_string(),
    };
    if let Err(e) = ctx.publish(LLM_DELTA, &d).await {
        tracing::warn!("llm：发出流式片段失败：{e}");
    }
}

fn cancelled_outcome(t: &Target, content: String, st: StreamState) -> StreamOutcome {
    StreamOutcome {
        reply: RawReply {
            content,
            model: st.model.unwrap_or_else(|| t.model.clone()),
            usage: st.usage,
        },
        cancelled: true,
    }
}

/// 流式调用：每段文本发一个 `llm.delta`；`cancel` 被通知时停止并返回已生成的部分。
pub(crate) async fn run(
    ctx: &Context,
    t: &Target,
    req: &ChatRequest,
    stream_id: &str,
    timeout: Duration,
    cancel: &Notify,
) -> Result<StreamOutcome, Failure> {
    let builder = build(ctx, t, req, true)?;
    let mut st = StreamState::default();
    let mut content = String::new();

    let sent = tokio::select! {
        _ = cancel.notified() => return Ok(cancelled_outcome(t, content, st)),
        r = tokio::time::timeout(timeout, builder.send()) => r,
    };
    let mut resp: Response = match sent {
        Err(_) => return Err(error::from_transport(true, false).into()),
        Ok(Err(e)) => return Err(transport(&e).into()),
        Ok(Ok(r)) => r,
    };
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        return Err(error::from_status(status, &text, &t.key).into());
    }
    // 不支持流式的接口直接回一个 JSON：退化为一次性输出
    let is_json = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if is_json {
        let text = tokio::select! {
            _ = cancel.notified() => return Ok(cancelled_outcome(t, content, st)),
            r = tokio::time::timeout(timeout, resp.text()) => match r {
                Err(_) => return Err(error::from_transport(true, false).into()),
                Ok(Err(e)) => return Err(transport(&e).into()),
                Ok(Ok(s)) => s,
            },
        };
        let reply = parse(t, &text)?;
        if !reply.content.is_empty() {
            emit(ctx, stream_id, &reply.content).await;
        }
        return Ok(StreamOutcome {
            reply,
            cancelled: false,
        });
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;
    while !done {
        let chunk = tokio::select! {
            _ = cancel.notified() => return Ok(cancelled_outcome(t, content, st)),
            r = tokio::time::timeout(timeout, resp.chunk()) => match r {
                Err(_) => return Err(Failure { error: error::from_transport(true, false), usage: st.usage }),
                Ok(Err(e)) => return Err(Failure { error: transport(&e), usage: st.usage }),
                Ok(Ok(c)) => c,
            },
        };
        match chunk {
            Some(c) => buf.extend_from_slice(&c),
            None => {
                // 流结束：处理没有换行结尾的最后一行
                if !buf.is_empty() {
                    buf.push(b'\n');
                }
                done = true;
            }
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                done = true;
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if v.get("error").is_some() || v["type"] == "error" {
                return Err(Failure {
                    error: error::from_stream_error(data, &t.key),
                    usage: st.usage,
                });
            }
            let text = match t.kind {
                Kind::Openai => openai::stream_event(&mut st, &v),
                Kind::Anthropic => anthropic::stream_event(&mut st, &v),
            };
            if let Some(text) = text {
                content.push_str(&text);
                emit(ctx, stream_id, &text).await;
            }
        }
    }
    Ok(StreamOutcome {
        reply: RawReply {
            content,
            model: st.model.unwrap_or_else(|| t.model.clone()),
            usage: st.usage,
        },
        cancelled: false,
    })
}

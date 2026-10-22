//! OpenAI 兼容适配器：`POST {base_url}/chat/completions`。

use plugin_sdk::contracts::llm::{ChatRequest, TokenUsage};
use plugin_sdk::{Context, ServiceError};
use reqwest::RequestBuilder;
use serde_json::{json, Value};

use crate::stream::StreamState;
use crate::{Failure, RawReply, Target};

pub(crate) fn request(
    ctx: &Context,
    t: &Target,
    req: &ChatRequest,
    stream: bool,
) -> Result<RequestBuilder, ServiceError> {
    let mut body = json!({ "model": t.model, "messages": req.messages });
    if req.json {
        body["response_format"] = json!({"type": "json_object"});
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if let Some(v) = req.temperature {
        body["temperature"] = json!(v);
    }
    if stream {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
    }
    let url = format!("{}/chat/completions", t.base_url.trim_end_matches('/'));
    let bytes = serde_json::to_vec(&body).map_err(ServiceError::internal)?;
    Ok(ctx
        .http()
        .post(url)
        .header("authorization", format!("Bearer {}", t.key))
        .header("content-type", "application/json")
        .body(bytes))
}

pub(crate) fn parse(text: &str, model: &str) -> Result<RawReply, Failure> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| ServiceError::unavailable("模型服务返回的响应无法解析"))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| ServiceError::unavailable("模型服务响应缺少内容"))?
        .to_string();
    Ok(RawReply {
        content,
        model: v["model"].as_str().unwrap_or(model).to_string(),
        usage: TokenUsage {
            input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

/// 处理一条流式 JSON 事件，返回其中的文本片段。
pub(crate) fn stream_event(st: &mut StreamState, v: &Value) -> Option<String> {
    if let Some(m) = v["model"].as_str().filter(|m| !m.is_empty()) {
        st.model = Some(m.to_string());
    }
    if v["usage"].is_object() {
        st.usage.input_tokens = v["usage"]["prompt_tokens"]
            .as_u64()
            .unwrap_or(st.usage.input_tokens);
        st.usage.output_tokens = v["usage"]["completion_tokens"]
            .as_u64()
            .unwrap_or(st.usage.output_tokens);
    }
    v["choices"][0]["delta"]["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

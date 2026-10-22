//! Anthropic 原生适配器：`POST {base_url}/v1/messages`。

use plugin_sdk::contracts::llm::{ChatMessage, ChatRequest, TokenUsage};
use plugin_sdk::{Context, ServiceError};
use reqwest::RequestBuilder;
use serde_json::{json, Value};

use crate::stream::StreamState;
use crate::{Failure, RawReply, Target};

const VERSION: &str = "2023-06-01";
const JSON_HINT: &str = "只输出一个 JSON 对象，不要其他文字";

pub(crate) fn request(
    ctx: &Context,
    t: &Target,
    req: &ChatRequest,
    stream: bool,
) -> Result<RequestBuilder, ServiceError> {
    let mut system: Vec<&str> = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_str())
        .collect();
    if req.json {
        system.push(JSON_HINT);
    }
    let messages: Vec<&ChatMessage> = req.messages.iter().filter(|m| m.role != "system").collect();
    let mut body = json!({
        "model": t.model,
        "max_tokens": req.max_tokens.unwrap_or(2048),
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if let Some(v) = req.temperature {
        body["temperature"] = json!(v);
    }
    if stream {
        body["stream"] = json!(true);
    }
    let url = format!("{}/v1/messages", t.base_url.trim_end_matches('/'));
    let bytes = serde_json::to_vec(&body).map_err(ServiceError::internal)?;
    Ok(ctx
        .http()
        .post(url)
        .header("x-api-key", &t.key)
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json")
        .body(bytes))
}

pub(crate) fn parse(text: &str, model: &str) -> Result<RawReply, Failure> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| ServiceError::unavailable("模型服务返回的响应无法解析"))?;
    let blocks = v["content"]
        .as_array()
        .ok_or_else(|| ServiceError::unavailable("模型服务响应缺少内容"))?;
    let content: String = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    Ok(RawReply {
        content,
        model: v["model"].as_str().unwrap_or(model).to_string(),
        usage: TokenUsage {
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
        },
    })
}

/// 处理一条流式 JSON 事件，返回其中的文本片段。
pub(crate) fn stream_event(st: &mut StreamState, v: &Value) -> Option<String> {
    match v["type"].as_str()? {
        "message_start" => {
            let m = &v["message"];
            if let Some(model) = m["model"].as_str().filter(|m| !m.is_empty()) {
                st.model = Some(model.to_string());
            }
            st.read_usage(&m["usage"]);
            None
        }
        "message_delta" => {
            st.read_usage(&v["usage"]);
            None
        }
        "content_block_delta" => {
            let d = &v["delta"];
            if d["type"] == "text_delta" {
                d["text"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            } else {
                None
            }
        }
        _ => None,
    }
}

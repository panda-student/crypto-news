use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util::{redact, SseEvent, SseParser};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("请求失败: {0}")]
    Http(String),
    #[error("服务返回 HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("响应格式不符合预期: {0}")]
    Decode(String),
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub prompt: Option<u64>,
    pub completion: Option<u64>,
    pub total: Option<u64>,
}

impl Usage {
    fn from_value(v: &Value) -> Usage {
        Usage {
            prompt: v["prompt_tokens"].as_u64(),
            completion: v["completion_tokens"].as_u64(),
            total: v["total_tokens"].as_u64(),
        }
    }

    pub fn display(&self) -> String {
        match (self.prompt, self.completion, self.total) {
            (None, None, None) => "无".to_string(),
            (p, c, t) => format!("{}/{}/{}", fmt(p), fmt(c), fmt(t)),
        }
    }
}

fn fmt(v: Option<u64>) -> String {
    v.map_or("-".to_string(), |n| n.to_string())
}

pub struct ChatOutput {
    pub content: String,
    pub usage: Usage,
    pub elapsed: Duration,
}

pub struct StreamOutput {
    pub content: String,
    pub usage: Usage,
    pub first_token: Option<Duration>,
    pub elapsed: Duration,
    pub chunks: usize,
}

pub struct Llm {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

impl Llm {
    /// proxy 为 None 时显式不走任何代理（仅测试用）。
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: &str,
        proxy: Option<&str>,
    ) -> Result<Llm, LlmError> {
        let builder = reqwest::Client::builder().timeout(Duration::from_secs(120));
        let builder = match proxy {
            Some(p) => builder.proxy(
                reqwest::Proxy::all(p).map_err(|e| LlmError::Http(e.without_url().to_string()))?,
            ),
            None => builder.no_proxy(),
        };
        let http = builder
            .build()
            .map_err(|e| LlmError::Http(e.without_url().to_string()))?;
        Ok(Llm {
            http,
            endpoint: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key: api_key.to_string(),
        })
    }

    /// 脱敏后的请求描述，可安全打印。
    pub fn describe_request(&self) -> String {
        format!("POST {} (Authorization: Bearer ***)", self.endpoint)
    }

    fn clean(&self, text: &str) -> String {
        redact(text, &self.api_key)
    }

    fn body(&self, user: &str, json_mode: bool, stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": user}],
            "temperature": 0.2,
        });
        if json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        if stream {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }
        body
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| LlmError::Http(self.clean(&e.without_url().to_string())))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let body: String = self.clean(&text).chars().take(500).collect();
            return Err(LlmError::Status { status, body });
        }
        Ok(resp)
    }

    pub async fn chat(&self, user: &str, json_mode: bool) -> Result<ChatOutput, LlmError> {
        let start = Instant::now();
        let resp = self.send(&self.body(user, json_mode, false)).await?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| LlmError::Decode(self.clean(&e.without_url().to_string())))?;
        let elapsed = start.elapsed();
        let content = v["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| LlmError::Decode("缺少 choices[0].message.content".into()))?
            .to_string();
        Ok(ChatOutput {
            content,
            usage: Usage::from_value(&v["usage"]),
            elapsed,
        })
    }

    pub async fn chat_stream(&self, user: &str) -> Result<StreamOutput, LlmError> {
        let start = Instant::now();
        let mut resp = self.send(&self.body(user, false, true)).await?;
        let mut parser = SseParser::default();
        let mut out = StreamOutput {
            content: String::new(),
            usage: Usage::default(),
            first_token: None,
            elapsed: Duration::ZERO,
            chunks: 0,
        };
        'outer: while let Some(bytes) = resp
            .chunk()
            .await
            .map_err(|e| LlmError::Http(self.clean(&e.without_url().to_string())))?
        {
            for ev in parser.feed(&bytes) {
                match ev {
                    SseEvent::Done => break 'outer,
                    SseEvent::Data(d) => {
                        let Ok(v) = serde_json::from_str::<Value>(&d) else {
                            continue;
                        };
                        if v["usage"].is_object() {
                            out.usage = Usage::from_value(&v["usage"]);
                        }
                        if let Some(text) = v["choices"][0]["delta"]["content"].as_str() {
                            if !text.is_empty() {
                                out.first_token.get_or_insert_with(|| start.elapsed());
                                out.content.push_str(text);
                                out.chunks += 1;
                            }
                        }
                    }
                }
            }
        }
        out.elapsed = start.elapsed();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn llm(server: &MockServer) -> Llm {
        Llm::new(&server.uri(), "m", "sk-secret", None).unwrap()
    }

    #[tokio::test]
    async fn chat_parses_content_and_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sk-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "你好"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            })))
            .mount(&server)
            .await;
        let out = llm(&server).chat("hi", false).await.unwrap();
        assert_eq!(out.content, "你好");
        assert_eq!(out.usage.total, Some(5));
    }

    #[tokio::test]
    async fn error_body_is_redacted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key sk-secret"))
            .mount(&server)
            .await;
        let err = llm(&server)
            .chat("hi", true)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("401"));
        assert!(!err.contains("sk-secret"));
    }

    #[tokio::test]
    async fn stream_collects_deltas() {
        let server = MockServer::start().await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n\
                    data: [DONE]\n\n";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let out = llm(&server).chat_stream("hi").await.unwrap();
        assert_eq!(out.content, "你好");
        assert_eq!(out.chunks, 2);
        assert!(out.first_token.is_some());
        assert_eq!(out.usage.total, Some(3));
    }
}

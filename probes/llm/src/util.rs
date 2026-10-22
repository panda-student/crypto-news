//! 无网络的纯函数：脱敏、JSON 提取、SSE 解析。

use serde_json::Value;

/// 把文本里出现的密钥替换成 ***。
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum JsonError {
    #[error("文本里没有成对的 {{ }}")]
    NoBraces,
    #[error("JSON 解析失败: {0}")]
    Invalid(String),
}

/// 去掉代码块标记，取第一个 `{` 到最后一个 `}` 解析。
pub fn extract_json(text: &str) -> Result<Value, JsonError> {
    let cleaned: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");
    let start = cleaned.find('{').ok_or(JsonError::NoBraces)?;
    let end = cleaned.rfind('}').ok_or(JsonError::NoBraces)?;
    if end < start {
        return Err(JsonError::NoBraces);
    }
    serde_json::from_str(&cleaned[start..=end]).map_err(|e| JsonError::Invalid(e.to_string()))
}

#[derive(Debug, PartialEq)]
pub enum SseEvent {
    Data(String),
    Done,
}

/// 增量 SSE 解析器：按字节喂入，按行切分，只关心 `data:` 行。
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(rest) = line.strip_prefix("data:") {
                let data = rest.trim_start();
                if data == "[DONE]" {
                    out.push(SseEvent::Done);
                } else if !data.is_empty() {
                    out.push(SseEvent::Data(data.to_string()));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_replaces_all() {
        assert_eq!(redact("Bearer sk-1 and sk-1", "sk-1"), "Bearer *** and ***");
        assert_eq!(redact("abc", ""), "abc");
    }

    #[test]
    fn json_in_code_fence() {
        let v = extract_json("```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn json_with_surrounding_text() {
        let v = extract_json("好的，结果如下：{\"a\": {\"b\": 2}} 以上。").unwrap();
        assert_eq!(v["a"]["b"], 2);
    }

    #[test]
    fn json_invalid() {
        assert_eq!(extract_json("没有 json"), Err(JsonError::NoBraces));
        assert!(matches!(extract_json("{a: 1}"), Err(JsonError::Invalid(_))));
    }

    #[test]
    fn sse_split_chunks_and_multibyte() {
        let mut p = SseParser::default();
        let full = "data: {\"x\":\"中\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        // 在多字节字符中间切开
        let cut = full.iter().position(|b| *b == 0xE4).unwrap() + 1;
        assert!(p.feed(&full[..cut]).is_empty());
        let ev = p.feed(&full[cut..]);
        assert_eq!(
            ev,
            vec![SseEvent::Data("{\"x\":\"中\"}".into()), SseEvent::Done]
        );
    }

    #[test]
    fn sse_ignores_comments_and_other_fields() {
        let mut p = SseParser::default();
        let ev = p.feed(b": ping\nevent: x\ndata:{\"a\":1}\n");
        assert_eq!(ev, vec![SseEvent::Data("{\"a\":1}".into())]);
    }
}

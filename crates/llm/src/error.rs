//! 适配器共用的错误映射。错误信息不含密钥、请求头；响应体只取前 200 字符。

use plugin_sdk::ServiceError;

/// 把非 2xx 响应映射成服务错误。
pub(crate) fn from_status(status: u16, body: &str, key: &str) -> ServiceError {
    let msg = format!("模型服务返回 HTTP {status}：{}", snippet(body, key));
    if status == 429 {
        ServiceError::rate_limited(msg)
    } else if status == 401 || status == 403 {
        ServiceError::credential_expired(msg)
    } else if status == 402 || body.contains("insufficient_quota") {
        ServiceError::quota_exhausted(msg)
    } else {
        ServiceError::unavailable(msg)
    }
}

/// 流式响应中途出现的错误事件。
pub(crate) fn from_stream_error(body: &str, key: &str) -> ServiceError {
    let msg = format!("模型服务返回错误：{}", snippet(body, key));
    if body.contains("insufficient_quota") {
        ServiceError::quota_exhausted(msg)
    } else if body.contains("rate_limit") {
        ServiceError::rate_limited(msg)
    } else {
        ServiceError::unavailable(msg)
    }
}

/// 网络错误只保留类别，reqwest 的错误文本可能带完整 URL。
pub(crate) fn from_transport(timeout: bool, connect: bool) -> ServiceError {
    if timeout {
        ServiceError::unavailable("请求模型服务超时")
    } else if connect {
        ServiceError::unavailable("无法连接模型服务")
    } else {
        ServiceError::unavailable("请求模型服务失败")
    }
}

fn snippet(body: &str, key: &str) -> String {
    let body = if key.is_empty() {
        body.to_string()
    } else {
        body.replace(key, "***")
    };
    body.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(200)
        .collect()
}

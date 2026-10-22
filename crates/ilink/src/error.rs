//! 错误类型：只做分类，不做重试。

/// 协议错误码：凭证（登录状态）失效。
pub const CODE_SESSION_EXPIRED: i32 = -14;
/// 协议错误码：配额用完或被限流。
pub const CODE_RATE_LIMITED: i32 = -2;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 网络错误：连接、超时、TLS、代理等（不含 URL，避免泄露查询参数）。
    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),
    /// HTTP 状态码不是 2xx，且响应体里没有可识别的协议错误码。
    #[error("HTTP status {status}")]
    HttpStatus { status: u16 },
    /// 协议错误码（ret / errcode 非 0），不属于下面两类专门的错误。
    #[error("protocol error {errcode}: {message}")]
    Api { errcode: i32, message: String },
    /// 凭证已过期（-14）：需要重新登录，或会话凭证（context_token）已失效。
    #[error("credential expired ({errcode}): {message}")]
    CredentialExpired { errcode: i32, message: String },
    /// 配额用完或被限流（-2）。什么时候重试由调用方决定。
    #[error("quota or rate limit ({errcode}): {message}")]
    RateLimited { errcode: i32, message: String },
    /// 响应不是预期的 JSON 结构。
    #[error("decode error: {0}")]
    Decode(String),
}

impl Error {
    pub(crate) fn network(e: reqwest::Error) -> Self {
        Error::Network(e.without_url())
    }

    /// 由协议错误码构造对应类别。
    pub(crate) fn from_code(errcode: i32, message: String) -> Self {
        match errcode {
            CODE_SESSION_EXPIRED => Error::CredentialExpired { errcode, message },
            CODE_RATE_LIMITED => Error::RateLimited { errcode, message },
            _ => Error::Api { errcode, message },
        }
    }

    /// 协议错误码（仅协议类错误有）。
    pub fn errcode(&self) -> Option<i32> {
        match self {
            Error::Api { errcode, .. }
            | Error::CredentialExpired { errcode, .. }
            | Error::RateLimited { errcode, .. } => Some(*errcode),
            _ => None,
        }
    }

    /// HTTP 状态码（仅 `HttpStatus` 有）。
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Error::HttpStatus { status } => Some(*status),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

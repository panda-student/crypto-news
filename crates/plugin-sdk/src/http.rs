use std::time::Duration;

use crate::PluginError;

/// 构建强制走代理的 HTTP 客户端：rustls，超时 30 秒。没有不带代理的版本。
pub fn build_http_client(proxy: &str) -> Result<reqwest::Client, PluginError> {
    let proxy = proxy.trim();
    if proxy.is_empty() {
        return Err(PluginError::new("未配置代理：所有外部请求必须走代理"));
    }
    let p = reqwest::Proxy::all(proxy).map_err(|_| PluginError::new("代理地址格式错误"))?;
    reqwest::Client::builder()
        .proxy(p)
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| PluginError::new(describe_error(&e)))
}

/// 描述 reqwest 错误，不带 URL（URL 里可能有密钥）。
pub(crate) fn describe_error(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "超时"
    } else if e.is_connect() {
        "连接失败"
    } else if e.is_decode() {
        "响应解析失败"
    } else if e.is_status() {
        "HTTP 状态错误"
    } else if e.is_builder() {
        "请求构建失败"
    } else {
        "请求失败"
    };
    match e.status() {
        Some(s) => format!("HTTP {kind}（{s}）"),
        None => format!("HTTP {kind}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_proxy() {
        assert!(build_http_client("").is_err());
        assert!(build_http_client("http://127.0.0.1:7892").is_ok());
    }
}

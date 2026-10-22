//! OKX 公告信源：提供 `source` 1.0，信源 ID 为 `okx-ann:all`。

use std::sync::Arc;

use plugin_sdk::contracts::source::{
    truncate_raw, FetchParams, FetchResult, Item, SourceInfo, SourceList,
};
use plugin_sdk::contracts::{format_rfc3339, parse_params, to_result};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

pub const DEFAULT_URL: &str = "https://www.okx.com/api/v5/support/announcements?page=1";
const SOURCE_ID: &str = "okx-ann:all";
const DEFAULT_INTERVAL: u64 = 120;

#[derive(Debug, Deserialize, Default)]
struct Config {
    interval_secs: Option<u64>,
}

pub struct OkxAnnPlugin {
    url: String,
}

impl OkxAnnPlugin {
    pub fn new() -> Self {
        Self::with_url(DEFAULT_URL)
    }

    /// 替换请求地址（完整 URL，含查询参数），测试指向 wiremock。
    pub fn with_url(url: impl Into<String>) -> Self {
        OkxAnnPlugin { url: url.into() }
    }
}

impl Default for OkxAnnPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OkxAnnPlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = ctx.config_as()?;
            let interval = cfg.interval_secs.unwrap_or(DEFAULT_INTERVAL).max(1);
            let url: Arc<str> = Arc::from(self.url.as_str());
            let http = ctx.http().clone();
            ctx.provide_fn("source", move |_caller, method, params| {
                let url = url.clone();
                let http = http.clone();
                async move {
                    match method.as_str() {
                        "list" => to_result(&SourceList {
                            sources: vec![SourceInfo {
                                id: SOURCE_ID.into(),
                                name: "OKX 公告".into(),
                                interval_secs: interval,
                            }],
                        }),
                        "fetch" => {
                            let p: FetchParams = parse_params(params)?;
                            if p.id != SOURCE_ID {
                                return Err(ServiceError::invalid_params(format!(
                                    "未知信源：{}",
                                    p.id
                                )));
                            }
                            let items = fetch(&http, &url).await?;
                            to_result(&FetchResult {
                                items,
                                cursor: None,
                            })
                        }
                        m => Err(ServiceError::method_not_found(m)),
                    }
                }
            })?;
            Ok(())
        })
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<Item>, ServiceError> {
    let resp = http.get(url).send().await.map_err(|e| {
        // 不输出 reqwest 错误本身，避免带出完整 URL
        let kind = if e.is_timeout() {
            "超时"
        } else if e.is_connect() {
            "连接失败"
        } else {
            "请求失败"
        };
        tracing::warn!("OKX 公告{kind}");
        ServiceError::unavailable(format!("OKX 公告{kind}"))
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ServiceError::unavailable(format!(
            "OKX 公告 HTTP {}",
            status.as_u16()
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|_| ServiceError::unavailable("OKX 公告读取响应失败"))?;
    parse(&body)
}

/// 解析 OKX 公告响应。
pub fn parse(body: &[u8]) -> Result<Vec<Item>, ServiceError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| ServiceError::unavailable("OKX 公告响应不是有效 JSON"))?;
    if v["code"].as_str() != Some("0") {
        return Err(ServiceError::unavailable(format!(
            "OKX 公告返回错误码 {}",
            v["code"].as_str().unwrap_or("?")
        )));
    }
    let pages = v["data"]
        .as_array()
        .ok_or_else(|| ServiceError::unavailable("OKX 公告响应缺少 data"))?;
    let mut items = Vec::new();
    for d in pages
        .iter()
        .flat_map(|p| p["details"].as_array().into_iter().flatten())
    {
        let Some(url) = d["url"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let published_at = d["pTime"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|ms| OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).ok())
            .map(format_rfc3339);
        items.push(Item {
            external_id: None,
            url: url.to_string(),
            title: d["title"].as_str().unwrap_or_default().to_string(),
            summary: d["annType"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(String::from),
            published_at,
            raw: truncate_raw(d.to_string()),
        });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FIXTURE: &str = r#"{"code":"0","msg":"","data":[{"details":[
{"annType":"announcements-api","title":"Old","url":"https://www.okx.com/help/old","pTime":"1790157613684"},
{"annType":"","title":"Newest","url":"https://www.okx.com/help/new","pTime":"1790758418444"},
{"title":"NoUrl","url":"","pTime":"1790758418444"}
],"page":"1","totalPage":"3"}]}"#;

    #[test]
    fn parses_fixture() {
        let items = parse(FIXTURE.as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].external_id, None);
        assert_eq!(items[0].title, "Old");
        assert_eq!(items[0].summary.as_deref(), Some("announcements-api"));
        assert_eq!(
            items[0].published_at.as_deref(),
            Some("2026-09-23T10:00:13Z")
        );
        assert_eq!(items[1].summary, None);
        let raw: Value = serde_json::from_str(&items[0].raw).unwrap();
        assert_eq!(raw["title"], "Old");
    }

    #[test]
    fn rejects_bad_code() {
        let e = parse(br#"{"code":"50011","msg":"x"}"#).unwrap_err();
        assert_eq!(e.code, ServiceError::UNAVAILABLE);
        assert!(parse(b"<html>").is_err());
    }

    async fn setup(server: &MockServer, config: Value) -> Arc<TestHub> {
        let hub = TestHub::new();
        let ctx = hub.context("okx-ann", config, test_http_client());
        let plugin = OkxAnnPlugin::with_url(format!(
            "{}/api/v5/support/announcements?page=1",
            server.uri()
        ));
        plugin.start(ctx).await.unwrap();
        hub
    }

    #[tokio::test]
    async fn list_and_fetch_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/support/announcements"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .mount(&server)
            .await;
        let hub = setup(&server, json!({ "interval_secs": 30 })).await;
        let l = hub.call("source", "list", json!({})).await.unwrap();
        assert_eq!(l["sources"][0]["id"], "okx-ann:all");
        assert_eq!(l["sources"][0]["interval_secs"], 30);
        let r = hub
            .call("source", "fetch", json!({ "id": "okx-ann:all" }))
            .await
            .unwrap();
        assert_eq!(r["items"].as_array().unwrap().len(), 2);
        let e = hub
            .call("source", "fetch", json!({ "id": "other" }))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn default_interval() {
        let server = MockServer::start().await;
        let hub = setup(&server, json!({})).await;
        let l = hub.call("source", "list", json!({})).await.unwrap();
        assert_eq!(l["sources"][0]["interval_secs"], 120);
    }

    #[tokio::test]
    async fn code_nonzero_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"code":"50011","msg":"busy"}"#),
            )
            .mount(&server)
            .await;
        let hub = setup(&server, json!({})).await;
        let e = hub
            .call("source", "fetch", json!({ "id": "okx-ann:all" }))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::UNAVAILABLE);
        assert!(!e.message.contains("http"));
    }

    #[tokio::test]
    async fn http_500_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let hub = setup(&server, json!({})).await;
        let e = hub
            .call("source", "fetch", json!({ "id": "okx-ann:all" }))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::UNAVAILABLE);
        assert!(!e.message.contains("http"));
        assert!(e.message.contains("500"));
    }
}

//! Binance 公告信源：提供 `source` 1.0，每个公告分类一个信源 `binance-ann:<catalogId>`。

use std::sync::Arc;

use plugin_sdk::contracts::source::{
    truncate_raw, FetchParams, FetchResult, Item, SourceInfo, SourceList, SERVICE,
};
use plugin_sdk::contracts::{format_rfc3339, parse_params, to_result};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

const DEFAULT_BASE: &str = "https://www.binance.com";
const LIST_PATH: &str = "/bapi/composite/v1/public/cms/article/list/query";
const ARTICLE_BASE: &str = "https://www.binance.com/en/support/announcement/";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; crypto-news)";
const NEW_LISTING: i64 = 48;

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    interval_secs: u64,
    catalog_ids: Vec<i64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_secs: 120,
            catalog_ids: vec![NEW_LISTING],
        }
    }
}

pub struct BinanceAnnPlugin {
    base: String,
}

impl BinanceAnnPlugin {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE)
    }

    /// 替换接口根地址（测试指向 wiremock）。
    pub fn with_base(base: impl Into<String>) -> Self {
        BinanceAnnPlugin {
            base: base.into().trim_end_matches('/').to_string(),
        }
    }
}

impl Default for BinanceAnnPlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn source_id(catalog: i64) -> String {
    format!("binance-ann:{catalog}")
}

fn source_name(catalog: i64) -> String {
    if catalog == NEW_LISTING {
        "Binance 新币上线".to_string()
    } else {
        format!("Binance 公告 {catalog}")
    }
}

/// 解析响应体；错误消息不含 URL。
pub fn parse_articles(body: &[u8]) -> Result<Vec<Item>, ServiceError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| ServiceError::unavailable("Binance 响应不是有效 JSON"))?;
    if v["code"].as_str() != Some("000000") {
        return Err(ServiceError::unavailable("Binance 响应 code 不是 000000"));
    }
    let catalogs = v["data"]["catalogs"]
        .as_array()
        .ok_or_else(|| ServiceError::unavailable("Binance 响应缺少 data.catalogs"))?;
    let mut items = Vec::new();
    for a in catalogs
        .iter()
        .flat_map(|c| c["articles"].as_array().into_iter().flatten())
    {
        let code = a["code"].as_str().unwrap_or_default();
        let title = a["title"].as_str().unwrap_or_default();
        if code.is_empty() || title.is_empty() {
            continue;
        }
        let external_id = match &a["id"] {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        let published_at = a["releaseDate"]
            .as_i64()
            .and_then(|ms| time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).ok())
            .map(format_rfc3339);
        items.push(Item {
            external_id,
            url: format!("{ARTICLE_BASE}{code}"),
            title: title.to_string(),
            summary: None,
            published_at,
            raw: truncate_raw(a.to_string()),
        });
    }
    Ok(items)
}

async fn fetch_catalog(ctx: &Context, base: &str, catalog: i64) -> Result<Vec<Item>, ServiceError> {
    let url = format!("{base}{LIST_PATH}?type=1&pageNo=1&pageSize=20&catalogId={catalog}");
    let resp = ctx
        .http()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|_| ServiceError::unavailable("Binance 请求失败"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ServiceError::unavailable(format!(
            "Binance 返回 HTTP {}",
            status.as_u16()
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|_| ServiceError::unavailable("Binance 读取响应失败"))?;
    parse_articles(&body)
}

impl Plugin for BinanceAnnPlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = ctx.config_as()?;
            let catalogs = Arc::new(cfg.catalog_ids);
            let interval = cfg.interval_secs;
            let base = self.base.clone();
            let handle = ctx.clone();
            tracing::info!(count = catalogs.len(), "binance-ann 启动");
            ctx.provide_fn(SERVICE, move |_caller, method, params| {
                let catalogs = catalogs.clone();
                let base = base.clone();
                let ctx = handle.clone();
                async move {
                    match method.as_str() {
                        "list" => to_result(&SourceList {
                            sources: catalogs
                                .iter()
                                .map(|&c| SourceInfo {
                                    id: source_id(c),
                                    name: source_name(c),
                                    interval_secs: interval,
                                })
                                .collect(),
                        }),
                        "fetch" => {
                            let p: FetchParams = parse_params(params)?;
                            let catalog = catalogs
                                .iter()
                                .copied()
                                .find(|&c| source_id(c) == p.id)
                                .ok_or_else(|| {
                                    ServiceError::invalid_params(format!("未知信源：{}", p.id))
                                })?;
                            let items = fetch_catalog(&ctx, &base, catalog).await?;
                            to_result(&FetchResult {
                                items,
                                cursor: None,
                            })
                        }
                        other => Err(ServiceError::method_not_found(other)),
                    }
                }
            })?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FIXTURE: &str = r#"{"code":"000000","message":null,"data":{"catalogs":[{"catalogId":48,"articles":[
{"id":286557,"code":"1dfe10f0","title":"Older","type":1,"releaseDate":1791862201158},
{"id":287560,"code":"e52ef133","title":"Newest","type":1,"releaseDate":1791948608368}]}]},"success":true}"#;

    async fn setup(server: &MockServer, config: Value) -> Arc<TestHub> {
        let hub = TestHub::new();
        let ctx = hub.context("binance-ann", config, test_http_client());
        BinanceAnnPlugin::with_base(server.uri())
            .start(ctx)
            .await
            .unwrap();
        hub
    }

    #[test]
    fn manifest_parses() {
        let m = plugin_sdk::Manifest::parse(BinanceAnnPlugin::new().manifest()).unwrap();
        assert_eq!(m.name, "binance-ann");
        assert_eq!(m.provides["source"], "1.0");
    }

    #[test]
    fn parses_fixture() {
        let items = parse_articles(FIXTURE.as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        let i = &items[1];
        assert_eq!(i.external_id.as_deref(), Some("287560"));
        assert_eq!(
            i.url,
            "https://www.binance.com/en/support/announcement/e52ef133"
        );
        assert_eq!(i.title, "Newest");
        assert_eq!(i.published_at.as_deref(), Some("2026-10-14T03:30:08Z"));
        assert!(i.raw.contains("\"code\":\"e52ef133\""));
    }

    #[test]
    fn rejects_bad_bodies() {
        assert!(parse_articles(b"<html></html>").is_err());
        assert!(parse_articles(br#"{"code":"100002","data":null}"#).is_err());
        assert!(parse_articles(br#"{"code":"000000","data":{}}"#).is_err());
    }

    #[tokio::test]
    async fn list_uses_config() {
        let server = MockServer::start().await;
        let hub = setup(
            &server,
            json!({"interval_secs": 60, "catalog_ids": [48, 161]}),
        )
        .await;
        let r = hub.call(SERVICE, "list", json!({})).await.unwrap();
        let l: SourceList = serde_json::from_value(r).unwrap();
        assert_eq!(l.sources.len(), 2);
        assert_eq!(l.sources[0].id, "binance-ann:48");
        assert_eq!(l.sources[0].name, "Binance 新币上线");
        assert_eq!(l.sources[1].name, "Binance 公告 161");
        assert_eq!(l.sources[1].interval_secs, 60);
    }

    #[tokio::test]
    async fn list_defaults() {
        let server = MockServer::start().await;
        let hub = setup(&server, json!({})).await;
        let r = hub.call(SERVICE, "list", json!({})).await.unwrap();
        let l: SourceList = serde_json::from_value(r).unwrap();
        assert_eq!(l.sources.len(), 1);
        assert_eq!(l.sources[0].interval_secs, 120);
    }

    #[tokio::test]
    async fn fetch_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(LIST_PATH))
            .and(query_param("catalogId", "48"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .mount(&server)
            .await;
        let hub = setup(&server, json!({})).await;
        let r = hub
            .call(SERVICE, "fetch", json!({"id": "binance-ann:48"}))
            .await
            .unwrap();
        let f: FetchResult = serde_json::from_value(r).unwrap();
        assert_eq!(f.items.len(), 2);
    }

    #[tokio::test]
    async fn fetch_failures_are_unavailable_without_url() {
        for resp in [
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_body_string(r#"{"code":"100002","data":null}"#),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(resp)
                .mount(&server)
                .await;
            let hub = setup(&server, json!({})).await;
            let e = hub
                .call(SERVICE, "fetch", json!({"id": "binance-ann:48"}))
                .await
                .unwrap_err();
            assert_eq!(e.code, ServiceError::UNAVAILABLE);
            assert!(!e.message.contains("http"), "{}", e.message);
            assert!(!e.message.contains("catalogId"), "{}", e.message);
        }
    }

    #[tokio::test]
    async fn fetch_unknown_source() {
        let server = MockServer::start().await;
        let hub = setup(&server, json!({})).await;
        let e = hub
            .call(SERVICE, "fetch", json!({"id": "binance-ann:999"}))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    }
}

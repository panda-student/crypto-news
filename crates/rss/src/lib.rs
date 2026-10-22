//! RSS 信源插件：默认带 SEC、CFTC、美联储，订阅存在数据库里，可增删改与暂停。
//! 提供 `source` 1.0 与 `feed-admin` 1.0。

mod admin;
mod parse;

use std::sync::Arc;
use std::time::Duration;

use plugin_sdk::contracts::source::{
    FetchParams, FetchResult, SourceInfo, SourceList, FEED_ADMIN_SERVICE, SERVICE as SOURCE_SERVICE,
};
use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::contracts::{parse_params, to_result};
use plugin_sdk::{BoxFuture, Caller, Context, Plugin, PluginError, ServiceError, ServiceHandler};
use serde::Deserialize;
use serde_json::Value;

pub use parse::parse_feed;

const MANIFEST: &str = include_str!("../plugin.toml");
const DEFAULT_INTERVAL: u64 = 300;
const SCHEMA: &str = "
CREATE TABLE feeds (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    url TEXT NOT NULL UNIQUE,
    paused INTEGER NOT NULL DEFAULT 0,
    builtin INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE TABLE feed_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";
const SCHEMA_V2: &str = "ALTER TABLE feeds ADD COLUMN interval_secs INTEGER;";

/// 一个 RSS 源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub id: String,
    pub name: String,
    pub url: String,
    /// 自带的抓取间隔（秒）；空则用全局默认
    pub interval_secs: Option<u64>,
}

impl Feed {
    pub fn new(id: &str, name: &str, url: &str) -> Self {
        Feed {
            id: id.into(),
            name: name.into(),
            url: url.into(),
            interval_secs: None,
        }
    }

    pub fn with_interval(mut self, secs: u64) -> Self {
        self.interval_secs = Some(secs);
        self
    }
}

const MEDIA_INTERVAL: u64 = 300;
const BLOG_INTERVAL: u64 = 1800;

/// 内置信源：监管机构 8 个、媒体与博客 8 个。
pub fn default_feeds() -> Vec<Feed> {
    vec![
        Feed::new(
            "rss:sec-press",
            "SEC 新闻稿",
            "https://www.sec.gov/news/pressreleases.rss",
        ),
        Feed::new(
            "rss:sec-litigation",
            "SEC 诉讼公告",
            "https://www.sec.gov/enforcement-litigation/litigation-releases/rss",
        ),
        Feed::new(
            "rss:sec-admin",
            "SEC 行政处罚",
            "https://www.sec.gov/enforcement-litigation/administrative-proceedings/rss",
        ),
        Feed::new(
            "rss:cftc-general",
            "CFTC 新闻稿",
            "https://www.cftc.gov/RSS/RSSGP/rssgp.xml",
        ),
        Feed::new(
            "rss:cftc-enforcement",
            "CFTC 执法新闻稿",
            "https://www.cftc.gov/RSS/RSSENF/rssenf.xml",
        ),
        Feed::new(
            "rss:cftc-speeches",
            "CFTC 讲话与证词",
            "https://www.cftc.gov/RSS/RSSST/rssst.xml",
        ),
        Feed::new(
            "rss:fed-press",
            "美联储 新闻稿",
            "https://www.federalreserve.gov/feeds/press_all.xml",
        ),
        Feed::new(
            "rss:fed-speeches",
            "美联储 讲话与证词",
            "https://www.federalreserve.gov/feeds/speeches_and_testimony.xml",
        ),
        Feed::new(
            "rss:coindesk",
            "CoinDesk",
            "https://www.coindesk.com/arc/outboundfeeds/rss",
        )
        .with_interval(MEDIA_INTERVAL),
        Feed::new(
            "rss:cointelegraph",
            "Cointelegraph",
            "https://cointelegraph.com/rss",
        )
        .with_interval(MEDIA_INTERVAL),
        Feed::new("rss:decrypt", "Decrypt", "https://decrypt.co/feed")
            .with_interval(MEDIA_INTERVAL),
        Feed::new(
            "rss:theblock",
            "The Block",
            "https://www.theblock.co/rss.xml",
        )
        .with_interval(MEDIA_INTERVAL),
        Feed::new("rss:panews", "PANews", "https://www.panewslab.com/rss.xml")
            .with_interval(MEDIA_INTERVAL),
        Feed::new("rss:wublock", "吴说", "https://www.wublock123.com/feed")
            .with_interval(MEDIA_INTERVAL),
        Feed::new(
            "rss:bitcoinmagazine",
            "Bitcoin Magazine",
            "https://www.bitcoinmagazine.com/.rss/full/",
        )
        .with_interval(MEDIA_INTERVAL),
        Feed::new(
            "rss:ethereum-blog",
            "Ethereum 博客",
            "https://blog.ethereum.org/feed.xml",
        )
        .with_interval(BLOG_INTERVAL),
    ]
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    contact_email: String,
    #[serde(default)]
    interval_secs: Option<u64>,
}

pub struct RssPlugin {
    /// 首次启动时写入数据库的内置订阅
    seed: Vec<Feed>,
    retry_delays: Vec<Duration>,
}

impl Default for RssPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl RssPlugin {
    pub fn new() -> Self {
        Self::with_feeds(
            default_feeds(),
            vec![Duration::from_secs(2), Duration::from_secs(4)],
        )
    }

    /// 替换首次播种的订阅清单与重试间隔（每个间隔对应一次重试），测试用。
    pub fn with_feeds(seed: Vec<Feed>, retry_delays: Vec<Duration>) -> Self {
        RssPlugin { seed, retry_delays }
    }
}

impl Plugin for RssPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = ctx.config_as()?;
            let email = cfg.contact_email.trim();
            // 联系邮箱可选：缺省时 User-Agent 不带邮箱
            let ua = if email.is_empty() {
                "crypto-news/0.1".to_string()
            } else {
                format!("crypto-news/0.1 ({email})")
            };
            let store = ctx.store();
            store.migrate(1, SCHEMA).await?;
            store.migrate(2, SCHEMA_V2).await?;
            admin::seed(&store, &self.seed).await?;
            let handler = Arc::new(Handler {
                store: store.clone(),
                retry_delays: self.retry_delays.clone(),
                ua,
                has_email: !email.is_empty(),
                interval_secs: cfg.interval_secs.unwrap_or(DEFAULT_INTERVAL).max(1),
                http: ctx.http().clone(),
            });
            ctx.provide(SOURCE_SERVICE, handler)?;
            ctx.provide(FEED_ADMIN_SERVICE, Arc::new(admin::Admin { store }))
        })
    }
}

struct Handler {
    store: StoreClient,
    retry_delays: Vec<Duration>,
    ua: String,
    has_email: bool,
    interval_secs: u64,
    http: reqwest::Client,
}

impl Handler {
    /// 未暂停的订阅。
    async fn list(&self) -> Result<SourceList, ServiceError> {
        let feeds = admin::list_feeds_with_interval(&self.store).await?;
        Ok(SourceList {
            sources: feeds
                .into_iter()
                .filter(|(f, _)| !f.paused)
                .map(|(f, iv)| SourceInfo {
                    id: f.id,
                    name: f.name,
                    interval_secs: iv.map_or(self.interval_secs, |v| v.max(1)),
                })
                .collect(),
        })
    }

    async fn download(&self, feed: &Feed) -> Result<Vec<u8>, ServiceError> {
        let mut attempt = 0;
        loop {
            let resp = self
                .http
                .get(&feed.url)
                .header("User-Agent", &self.ua)
                .send()
                .await
                .map_err(|e| {
                    ServiceError::unavailable(format!("{} 请求失败：{}", feed.id, brief(&e)))
                })?;
            let status = resp.status();
            if status.is_success() {
                let body = resp.bytes().await.map_err(|e| {
                    ServiceError::unavailable(format!("{} 读取响应失败：{}", feed.id, brief(&e)))
                })?;
                return Ok(body.to_vec());
            }
            let code = status.as_u16();
            let retryable = code == 403 || code == 429 || status.is_server_error();
            match self.retry_delays.get(attempt) {
                Some(d) if retryable => {
                    tracing::warn!(source = %feed.id, status = code, "抓取被拒，稍后重试");
                    tokio::time::sleep(*d).await;
                    attempt += 1;
                }
                _ => {
                    return Err(ServiceError::unavailable(format!(
                        "{} 返回 HTTP {code}",
                        feed.id
                    )))
                }
            }
        }
    }

    async fn fetch(&self, p: FetchParams) -> Result<FetchResult, ServiceError> {
        // 暂停或已删除的订阅不抓取，返回空结果
        let feed = admin::list_feeds(&self.store)
            .await?
            .into_iter()
            .find(|f| f.id == p.id && !f.paused);
        let Some(feed) = feed else {
            return Ok(FetchResult {
                items: vec![],
                cursor: p.cursor,
            });
        };
        let feed = Feed::new(&feed.id, &feed.name, &feed.url);
        if !self.has_email && is_sec(&feed.url) {
            return Err(ServiceError::unavailable(
                "SEC 要求填写联系邮箱：请在 设置 → 插件 → rss 的配置里填 contact_email",
            ));
        }
        let body = self.download(&feed).await?;
        let items = parse_feed(&body)
            .map_err(|e| ServiceError::internal(format!("{} 解析失败：{e}", feed.id)))?;
        let cursor = items
            .iter()
            .filter_map(|i| i.published_at.clone())
            .max()
            .or(p.cursor);
        Ok(FetchResult { items, cursor })
    }
}

/// 是否 sec.gov 域名（含子域名）。
fn is_sec(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .and_then(|a| a.rsplit('@').next())
        .and_then(|a| a.split(':').next())
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "sec.gov" || host.ends_with(".sec.gov")
}

/// reqwest 错误的简述，不含 URL。
fn brief(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "超时"
    } else if e.is_connect() {
        "连接失败"
    } else {
        "网络错误"
    }
}

impl ServiceHandler for Handler {
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            match method {
                "list" => to_result(&self.list().await?),
                "fetch" => to_result(&self.fetch(parse_params(params)?).await?),
                _ => Err(ServiceError::method_not_found(method)),
            }
        })
    }
}

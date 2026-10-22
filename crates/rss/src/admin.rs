//! `feed-admin` 服务与订阅表的读写。

use plugin_sdk::contracts::now_rfc3339;
use plugin_sdk::contracts::source::{FeedInfo, FeedInput, FeedList};
use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::contracts::{parse_params, to_result};
use plugin_sdk::{BoxFuture, Caller, PluginError, ServiceError, ServiceHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::Feed;

/// 旧版只有这一个"整个清单已播种"标记。
const LEGACY_SEEDED_KEY: &str = "seeded";
/// 旧版清单里的内置订阅 ID；只有旧标记时视为已播种。
const LEGACY_IDS: [&str; 8] = [
    "rss:sec-press",
    "rss:sec-litigation",
    "rss:sec-admin",
    "rss:cftc-general",
    "rss:cftc-enforcement",
    "rss:cftc-speeches",
    "rss:fed-press",
    "rss:fed-speeches",
];

#[derive(Deserialize)]
struct Row {
    id: String,
    name: String,
    url: String,
    paused: i64,
    builtin: i64,
    interval_secs: Option<i64>,
}

impl From<Row> for FeedInfo {
    fn from(r: Row) -> Self {
        FeedInfo {
            id: r.id,
            name: r.name,
            url: r.url,
            paused: r.paused != 0,
            builtin: r.builtin != 0,
        }
    }
}

/// 订阅及其自带的抓取间隔（空则用全局默认）。
pub async fn list_feeds_with_interval(
    store: &StoreClient,
) -> Result<Vec<(FeedInfo, Option<u64>)>, ServiceError> {
    let rows: Vec<Row> = store
        .query_as(
            "SELECT id, name, url, paused, builtin, interval_secs FROM feeds ORDER BY created_at, id",
            vec![],
        )
        .await
        .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let iv = r.interval_secs.and_then(|v| u64::try_from(v).ok());
            (r.into(), iv)
        })
        .collect())
}

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读写订阅失败：{}", e.message))
}

pub async fn list_feeds(store: &StoreClient) -> Result<Vec<FeedInfo>, ServiceError> {
    Ok(list_feeds_with_interval(store)
        .await?
        .into_iter()
        .map(|(f, _)| f)
        .collect())
}

/// 每次启动把还没播种过的内置订阅写进 `feeds`，并按订阅 ID 记一条已播种。
/// 用户删掉的内置订阅因为已记过，不会写回。
pub async fn seed(store: &StoreClient, seed: &[Feed]) -> Result<(), PluginError> {
    let legacy = !store
        .query(
            "SELECT value FROM feed_meta WHERE key = ?",
            vec![json!(LEGACY_SEEDED_KEY)],
        )
        .await?
        .is_empty();
    if legacy {
        for id in LEGACY_IDS {
            mark_seeded(store, id).await?;
        }
    }
    let now = now_rfc3339();
    for f in seed {
        let key = format!("seeded:{}", f.id);
        let done = store
            .query(
                "SELECT value FROM feed_meta WHERE key = ?",
                vec![json!(key)],
            )
            .await?;
        if !done.is_empty() {
            continue;
        }
        store
            .execute(
                "INSERT OR IGNORE INTO feeds (id, name, url, paused, builtin, created_at, interval_secs) \
                 VALUES (?, ?, ?, 0, 1, ?, ?)",
                vec![
                    json!(f.id),
                    json!(f.name),
                    json!(f.url),
                    json!(now),
                    json!(f.interval_secs),
                ],
            )
            .await?;
        mark_seeded(store, &f.id).await?;
    }
    Ok(())
}

async fn mark_seeded(store: &StoreClient, id: &str) -> Result<(), PluginError> {
    store
        .execute(
            "INSERT OR REPLACE INTO feed_meta (key, value) VALUES (?, '1')",
            vec![json!(format!("seeded:{id}"))],
        )
        .await?;
    Ok(())
}

/// 只接受 http/https 且带主机名、不含空白的地址。
fn valid_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    match rest {
        Some(r) => !r.is_empty() && !r.starts_with('/') && !url.chars().any(char::is_whitespace),
        None => false,
    }
}

/// 由 url 生成短标识（FNV-1a 64 位的高 32 位，8 位十六进制）。
fn short_id(url: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("rss:{:08x}", h >> 32)
}

pub struct Admin {
    pub store: StoreClient,
}

impl Admin {
    async fn find(&self, id: &str) -> Result<FeedInfo, ServiceError> {
        list_feeds(&self.store)
            .await?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| ServiceError::invalid_params(format!("未知订阅：{id}")))
    }

    async fn save(&self, input: FeedInput) -> Result<FeedInfo, ServiceError> {
        let name = input.name.trim().to_string();
        let url = input.url.trim().to_string();
        if name.is_empty() {
            return Err(ServiceError::invalid_params("名称不能为空"));
        }
        if !valid_url(&url) {
            return Err(ServiceError::invalid_params("地址只接受 http/https"));
        }
        let feeds = list_feeds(&self.store).await?;
        let id = input.id.filter(|s| !s.is_empty());
        if feeds
            .iter()
            .any(|f| f.url == url && Some(&f.id) != id.as_ref())
        {
            return Err(ServiceError::invalid_params("该地址已订阅"));
        }
        match id {
            Some(id) => {
                if !feeds.iter().any(|f| f.id == id) {
                    return Err(ServiceError::invalid_params(format!("未知订阅：{id}")));
                }
                self.store
                    .execute(
                        "UPDATE feeds SET name = ?, url = ? WHERE id = ?",
                        vec![json!(name), json!(url), json!(id)],
                    )
                    .await
                    .map_err(db_err)?;
                self.find(&id).await
            }
            None => {
                let base = short_id(&url);
                let mut id = base.clone();
                let mut n = 2;
                while feeds.iter().any(|f| f.id == id) {
                    id = format!("{base}-{n}");
                    n += 1;
                }
                self.store
                    .execute(
                        "INSERT INTO feeds (id, name, url, paused, builtin, created_at) \
                         VALUES (?, ?, ?, 0, 0, ?)",
                        vec![json!(id), json!(name), json!(url), json!(now_rfc3339())],
                    )
                    .await
                    .map_err(db_err)?;
                self.find(&id).await
            }
        }
    }

    async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        let r = self
            .store
            .execute("DELETE FROM feeds WHERE id = ?", vec![json!(id)])
            .await
            .map_err(db_err)?;
        if r.rows_affected == 0 {
            return Err(ServiceError::invalid_params(format!("未知订阅：{id}")));
        }
        Ok(())
    }

    async fn set_paused(&self, id: &str, paused: bool) -> Result<FeedInfo, ServiceError> {
        self.find(id).await?;
        self.store
            .execute(
                "UPDATE feeds SET paused = ? WHERE id = ?",
                vec![json!(i64::from(paused)), json!(id)],
            )
            .await
            .map_err(db_err)?;
        self.find(id).await
    }
}

#[derive(Deserialize)]
struct IdParams {
    id: String,
}

#[derive(Deserialize)]
struct PausedParams {
    id: String,
    paused: bool,
}

impl ServiceHandler for Admin {
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            match method {
                "list" => to_result(&FeedList {
                    feeds: list_feeds(&self.store).await?,
                }),
                "save" => to_result(&self.save(parse_params(params)?).await?),
                "delete" => {
                    let p: IdParams = parse_params(params)?;
                    self.delete(&p.id).await?;
                    Ok(json!({ "ok": true }))
                }
                "set_paused" => {
                    let p: PausedParams = parse_params(params)?;
                    to_result(&self.set_paused(&p.id, p.paused).await?)
                }
                _ => Err(ServiceError::method_not_found(method)),
            }
        })
    }
}

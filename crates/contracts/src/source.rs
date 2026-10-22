//! 信源相关服务：`source` 1.0（信源插件提供）、`source-status` 1.1 与 `item-query` 1.0（collect 提供）、`feed-admin` 1.0（rss 提供）。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "source";
pub const STATUS_SERVICE: &str = "source-status";
pub const ITEM_QUERY_SERVICE: &str = "item-query";

/// `raw` 字段的上限。
pub const RAW_MAX_BYTES: usize = 64 * 1024;

/// 信源抓到的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// 为空时用 url 去重
    #[serde(default)]
    pub external_id: Option<String>,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    /// RFC3339 UTC
    #[serde(default)]
    pub published_at: Option<String>,
    /// 原始 JSON 或文本，最多 64 KB
    #[serde(default)]
    pub raw: String,
}

/// 把 `raw` 截到 64 KB 以内（按字符边界）。
pub fn truncate_raw(mut s: String) -> String {
    if s.len() > RAW_MAX_BYTES {
        let mut end = RAW_MAX_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    /// 全局唯一，格式为 `插件名:子名`
    pub id: String,
    pub name: String,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceList {
    pub sources: Vec<SourceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchParams {
    pub id: String,
    /// 上次返回的游标，第一次为空
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    pub items: Vec<Item>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `source` 服务的类型化客户端。
#[derive(Clone)]
pub struct SourceClient(pub Service);

impl SourceClient {
    pub async fn list(&self) -> Result<Vec<SourceInfo>, ServiceError> {
        let r: SourceList = call_typed(&*self.0, "list", &json!({})).await?;
        Ok(r.sources)
    }
    pub async fn fetch(
        &self,
        id: &str,
        cursor: Option<String>,
    ) -> Result<FetchResult, ServiceError> {
        call_typed(
            &*self.0,
            "fetch",
            &FetchParams {
                id: id.to_string(),
                cursor,
            },
        )
        .await
    }
}

/// 信源状态（`source-status` 的 `list`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub id: String,
    pub name: String,
    /// ok / failing / unknown
    pub state: String,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: i64,
    pub total_items: i64,
    pub interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatusList {
    pub sources: Vec<SourceStatus>,
}

#[derive(Clone)]
pub struct SourceStatusClient(pub Service);

impl SourceStatusClient {
    pub async fn list(&self) -> Result<Vec<SourceStatus>, ServiceError> {
        let r: SourceStatusList = call_typed(&*self.0, "list", &json!({})).await?;
        Ok(r.sources)
    }
}

/// 已入库的消息（`item-query` 的返回）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredItem {
    pub id: i64,
    pub source_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    /// 入库时间，RFC3339 UTC
    pub collected_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemQuery {
    /// Cursor: records older than this item (collected_at, id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<i64>,
    /// 入库时间下限（含），RFC3339
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// 入库时间上限（不含），RFC3339
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// 标题或摘要包含的文字
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// 缺省 100，最大 500
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemList {
    pub items: Vec<StoredItem>,
}

/// `item-query` 1.0：`get {ids}` → `{items}`；`list ItemQuery` → `{items}`，按入库时间倒序。
#[derive(Clone)]
pub struct ItemQueryClient(pub Service);

impl ItemQueryClient {
    pub async fn get(&self, ids: &[i64]) -> Result<Vec<StoredItem>, ServiceError> {
        let r: ItemList = call_typed(&*self.0, "get", &json!({ "ids": ids })).await?;
        Ok(r.items)
    }
    pub async fn list(&self, q: &ItemQuery) -> Result<Vec<StoredItem>, ServiceError> {
        let r: ItemList = call_typed(&*self.0, "list", q).await?;
        Ok(r.items)
    }
    /// 取原文（`raw`），不存在时为 None。
    pub async fn raw(&self, id: i64) -> Result<Option<String>, ServiceError> {
        let r: Value = self.0.call("raw", json!({ "id": id })).await?;
        Ok(r.get("raw").and_then(Value::as_str).map(str::to_string))
    }
    /// 删除消息，返回删除条数。只允许 maintenance 调用（`delete {ids}` → `{deleted}`）。
    pub async fn delete(&self, ids: &[i64]) -> Result<u64, ServiceError> {
        let r: Value = self.0.call("delete", json!({ "ids": ids })).await?;
        Ok(r.get("deleted").and_then(Value::as_u64).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_raw_keeps_char_boundary() {
        let s = "汉".repeat(RAW_MAX_BYTES);
        let t = truncate_raw(s);
        assert!(t.len() <= RAW_MAX_BYTES);
        assert!(t.chars().all(|c| c == '汉'));
    }

    #[test]
    fn item_defaults() {
        let i: Item = serde_json::from_value(json!({"url":"u","title":"t"})).unwrap();
        assert!(i.external_id.is_none());
        assert_eq!(i.raw, "");
    }
}

impl SourceStatusClient {
    /// 1.1：让一个信源立即重新抓取（`retry {id}` → `{ok}`），不等间隔到期。
    pub async fn retry(&self, id: &str) -> Result<(), ServiceError> {
        self.0.call("retry", json!({ "id": id })).await?;
        Ok(())
    }
}

/// RSS 订阅管理服务，由 rss 插件提供。
pub const FEED_ADMIN_SERVICE: &str = "feed-admin";

/// 一个 RSS 订阅。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedInfo {
    /// 信源 ID，格式 `rss:<子名>`，与 source-status 里的 id 一致
    pub id: String,
    pub name: String,
    pub url: String,
    /// 暂停的订阅不出现在 `source.list` 里，也不抓取
    pub paused: bool,
    /// 内置订阅（首次启动时写入的默认清单）
    pub builtin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedList {
    pub feeds: Vec<FeedInfo>,
}

/// 新建或修改订阅。`id` 为空表示新建，由插件生成。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedInput {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    /// 只接受 http/https
    pub url: String,
}

/// `feed-admin` 1.0：`list` → `{feeds}`；`save FeedInput` → FeedInfo；
/// `delete {id}` → `{ok}`；`set_paused {id, paused}` → FeedInfo。
#[derive(Clone)]
pub struct FeedAdminClient(pub Service);

impl FeedAdminClient {
    pub async fn list(&self) -> Result<Vec<FeedInfo>, ServiceError> {
        let r: FeedList = call_typed(&*self.0, "list", &json!({})).await?;
        Ok(r.feeds)
    }
    pub async fn save(&self, input: &FeedInput) -> Result<FeedInfo, ServiceError> {
        call_typed(&*self.0, "save", input).await
    }
    pub async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        self.0.call("delete", json!({ "id": id })).await?;
        Ok(())
    }
    pub async fn set_paused(&self, id: &str, paused: bool) -> Result<FeedInfo, ServiceError> {
        call_typed(
            &*self.0,
            "set_paused",
            &json!({ "id": id, "paused": paused }),
        )
        .await
    }
}

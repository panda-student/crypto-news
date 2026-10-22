//! 关注账号表 `x_accounts` 与元数据表 `x_meta` 的读写。

use plugin_sdk::contracts::now_rfc3339;
use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::{PluginError, ServiceError};
use serde::Deserialize;
use serde_json::json;

pub const SCHEMA: &str = "
CREATE TABLE x_accounts (
    handle TEXT PRIMARY KEY,
    display TEXT NOT NULL,
    paused INTEGER NOT NULL DEFAULT 0,
    builtin INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE TABLE x_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// 默认关注的账号。
pub const DEFAULT_HANDLES: [&str; 9] = [
    "VitalikButerin",
    "cz_binance",
    "brian_armstrong",
    "saylor",
    "haydenzadams",
    "binance",
    "okx",
    "coinbase",
    "SECGov",
];

/// 已经播种过的内置账号（JSON 数组，小写）。
const SEEDED_KEY: &str = "seeded_builtin";
/// 是否已登录（"1"/"0"）。
pub const LOGGED_IN_KEY: &str = "logged_in";

/// 一条关注记录。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Account {
    /// 小写
    pub handle: String,
    /// 原样大小写
    pub display: String,
    pub paused: bool,
    pub builtin: bool,
}

#[derive(Deserialize)]
struct Row {
    handle: String,
    display: String,
    paused: i64,
    builtin: i64,
}

/// 校验并去掉前导 @，返回原样大小写的用户名。
pub fn normalize_handle(input: &str) -> Result<String, ServiceError> {
    let s = input.trim();
    let s = s.strip_prefix('@').unwrap_or(s);
    let ok =
        (1..=15).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        Ok(s.to_string())
    } else {
        Err(ServiceError::invalid_params(
            "账号名只能是 1 到 15 位字母、数字或下划线",
        ))
    }
}

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读写 X 账号失败：{}", e.message))
}

pub async fn list(store: &StoreClient) -> Result<Vec<Account>, ServiceError> {
    let rows: Vec<Row> = store
        .query_as(
            "SELECT handle, display, paused, builtin FROM x_accounts ORDER BY created_at, handle",
            vec![],
        )
        .await
        .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| Account {
            handle: r.handle,
            display: r.display,
            paused: r.paused != 0,
            builtin: r.builtin != 0,
        })
        .collect())
}

pub async fn find(store: &StoreClient, handle: &str) -> Result<Account, ServiceError> {
    let h = handle.trim().trim_start_matches('@').to_ascii_lowercase();
    list(store)
        .await?
        .into_iter()
        .find(|a| a.handle == h)
        .ok_or_else(|| ServiceError::invalid_params(format!("未关注账号：{handle}")))
}

pub async fn add(store: &StoreClient, handle: &str) -> Result<Account, ServiceError> {
    let display = normalize_handle(handle)?;
    let lower = display.to_ascii_lowercase();
    if list(store).await?.iter().any(|a| a.handle == lower) {
        return Err(ServiceError::invalid_params("该账号已在关注列表中"));
    }
    store
        .execute(
            "INSERT INTO x_accounts (handle, display, paused, builtin, created_at) \
             VALUES (?, ?, 0, 0, ?)",
            vec![json!(lower), json!(display), json!(now_rfc3339())],
        )
        .await
        .map_err(db_err)?;
    find(store, &lower).await
}

pub async fn delete(store: &StoreClient, handle: &str) -> Result<(), ServiceError> {
    let a = find(store, handle).await?;
    store
        .execute(
            "DELETE FROM x_accounts WHERE handle = ?",
            vec![json!(a.handle)],
        )
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn set_paused(
    store: &StoreClient,
    handle: &str,
    paused: bool,
) -> Result<Account, ServiceError> {
    let a = find(store, handle).await?;
    store
        .execute(
            "UPDATE x_accounts SET paused = ? WHERE handle = ?",
            vec![json!(i64::from(paused)), json!(a.handle)],
        )
        .await
        .map_err(db_err)?;
    find(store, &a.handle).await
}

pub async fn meta_get(store: &StoreClient, key: &str) -> Result<Option<String>, ServiceError> {
    #[derive(Deserialize)]
    struct V {
        value: String,
    }
    let rows: Vec<V> = store
        .query_as("SELECT value FROM x_meta WHERE key = ?", vec![json!(key)])
        .await
        .map_err(db_err)?;
    Ok(rows.into_iter().next().map(|v| v.value))
}

pub async fn meta_set(store: &StoreClient, key: &str, value: &str) -> Result<(), ServiceError> {
    store
        .execute(
            "INSERT OR REPLACE INTO x_meta (key, value) VALUES (?, ?)",
            vec![json!(key), json!(value)],
        )
        .await
        .map_err(db_err)?;
    Ok(())
}

/// 增量播种：只写入还没播种过的内置账号；用户删掉的内置账号不会写回，新版本新增的默认账号会补上。
pub async fn seed(store: &StoreClient, defaults: &[&str]) -> Result<(), PluginError> {
    let mut seeded: Vec<String> = meta_get(store, SEEDED_KEY)
        .await?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let now = now_rfc3339();
    let mut changed = false;
    for d in defaults {
        let lower = d.to_ascii_lowercase();
        if seeded.contains(&lower) {
            continue;
        }
        store
            .execute(
                "INSERT OR IGNORE INTO x_accounts (handle, display, paused, builtin, created_at) \
                 VALUES (?, ?, 0, 1, ?)",
                vec![json!(lower), json!(d), json!(now)],
            )
            .await?;
        seeded.push(lower);
        changed = true;
    }
    if changed {
        meta_set(store, SEEDED_KEY, &serde_json::to_string(&seeded)?).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::TestHub;

    async fn store() -> StoreClient {
        let hub = TestHub::new();
        hub.with_store(store::open_memory().await.unwrap());
        let ctx = hub.context(
            "x-browser",
            json!({}),
            plugin_sdk::testing::test_http_client(),
        );
        let s = ctx.store();
        s.migrate(1, SCHEMA).await.unwrap();
        s
    }

    #[test]
    fn handle_validation() {
        assert_eq!(normalize_handle("@Binance").unwrap(), "Binance");
        assert_eq!(normalize_handle("  cz_binance ").unwrap(), "cz_binance");
        assert_eq!(normalize_handle("a".repeat(15).as_str()).unwrap().len(), 15);
        for bad in [
            "",
            "@",
            "a".repeat(16).as_str(),
            "a-b",
            "a b",
            "中文",
            "@@a",
            "a.b",
        ] {
            assert!(normalize_handle(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn seeds_defaults_and_adds_new_ones_incrementally() {
        let s = store().await;
        seed(&s, &DEFAULT_HANDLES).await.unwrap();
        let l = list(&s).await.unwrap();
        assert_eq!(l.len(), 9);
        assert!(l.iter().all(|a| a.builtin && !a.paused));
        assert!(l
            .iter()
            .any(|a| a.handle == "vitalikbuterin" && a.display == "VitalikButerin"));
        // 删掉一个内置账号，再播种不会写回
        delete(&s, "@Binance").await.unwrap();
        seed(&s, &DEFAULT_HANDLES).await.unwrap();
        assert_eq!(list(&s).await.unwrap().len(), 8);
        // 新版本新增默认账号：补上，旧的仍不写回
        let mut more = DEFAULT_HANDLES.to_vec();
        more.push("NewDefault");
        seed(&s, &more).await.unwrap();
        let l = list(&s).await.unwrap();
        assert_eq!(l.len(), 9);
        assert!(l.iter().any(|a| a.handle == "newdefault" && a.builtin));
        assert!(!l.iter().any(|a| a.handle == "binance"));
    }

    #[tokio::test]
    async fn add_pause_delete() {
        let s = store().await;
        let a = add(&s, "@Foo_1").await.unwrap();
        assert_eq!((a.handle.as_str(), a.display.as_str()), ("foo_1", "Foo_1"));
        assert!(!a.builtin);
        assert!(add(&s, "foo_1").await.is_err());
        assert!(add(&s, "bad name").await.is_err());
        assert!(set_paused(&s, "FOO_1", true).await.unwrap().paused);
        assert!(!set_paused(&s, "foo_1", false).await.unwrap().paused);
        delete(&s, "foo_1").await.unwrap();
        assert!(delete(&s, "foo_1").await.is_err());
    }
}

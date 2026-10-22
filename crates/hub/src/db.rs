//! hub 自己的表：插件授权与启停、事件日志、订阅游标、界面改过的插件配置、应用级设置。

use std::sync::Arc;

use contracts::store::{Row, SharedStore, Statement, TableAccess};
use contracts::{format_rfc3339, Event, ServiceError};
use serde_json::{json, Value};

use crate::{HubError, Inner, Kind, HUB};

/// 版本 2 包含版本 1 的全部表，旧库重跑时 IF NOT EXISTS 跳过已有的表。
const MIGRATION_V2: &str = "
CREATE TABLE IF NOT EXISTS plugins (
    name TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 1,
    granted_version TEXT,
    granted_permissions TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS event_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS event_log_kind ON event_log (kind, id);
CREATE TABLE IF NOT EXISTS event_cursors (
    plugin TEXT NOT NULL,
    kind TEXT NOT NULL,
    last_id INTEGER NOT NULL,
    PRIMARY KEY (plugin, kind)
);
CREATE TABLE IF NOT EXISTS config_overrides (
    name TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (name, key)
);
CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

fn store_err(e: ServiceError) -> HubError {
    HubError::Store(e.message)
}

fn now() -> String {
    format_rfc3339(time::OffsetDateTime::now_utc())
}

pub(crate) async fn migrate(inner: &Arc<Inner>) -> Result<(), HubError> {
    inner
        .store
        .migrate(TableAccess::All, HUB.into(), 2, MIGRATION_V2.into())
        .await
        .map_err(store_err)?;
    Ok(())
}

async fn query(inner: &Inner, sql: &str, params: Vec<Value>) -> Result<Vec<Row>, ServiceError> {
    inner
        .store
        .query(TableAccess::All, Statement::new(sql, params))
        .await
}

async fn execute(inner: &Inner, sql: &str, params: Vec<Value>) -> Result<i64, ServiceError> {
    let r = inner
        .store
        .execute(TableAccess::All, Statement::new(sql, params))
        .await?;
    Ok(r.last_insert_id)
}

/// 读取已保存的启停与授权。扩展插件只有版本和权限都和授权时一致才算已授权。
pub(crate) async fn load(inner: &Arc<Inner>) -> Result<(), HubError> {
    let rows = query(
        inner,
        "SELECT name, enabled, granted_version, granted_permissions FROM plugins",
        vec![],
    )
    .await
    .map_err(store_err)?;
    let mut st = inner.state.lock().unwrap();
    for r in rows {
        let Some(name) = r.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(e) = st.plugins.get_mut(name) else {
            continue;
        };
        e.enabled = r.get("enabled").and_then(Value::as_i64).unwrap_or(1) != 0;
        if e.kind == Kind::Extension {
            let v = r.get("granted_version").and_then(Value::as_str);
            let p = r.get("granted_permissions").and_then(Value::as_str);
            e.granted =
                v == Some(e.manifest.version.as_str()) && p == Some(e.permissions_json().as_str());
        }
    }
    Ok(())
}

/// 保存全部插件的启停与授权。未授权的扩展插件保留旧的授权记录。
pub(crate) async fn persist(inner: &Arc<Inner>) -> Result<(), ServiceError> {
    let stmts: Vec<Statement> = {
        let st = inner.state.lock().unwrap();
        let ts = now();
        st.plugins
            .values()
            .map(|e| {
                let (v, p) = if e.granted {
                    (json!(e.manifest.version), json!(e.permissions_json()))
                } else {
                    (Value::Null, Value::Null)
                };
                Statement::new(
                    "INSERT INTO plugins (name, enabled, granted_version, granted_permissions, updated_at)
                     VALUES (?, ?, ?, ?, ?)
                     ON CONFLICT(name) DO UPDATE SET
                       enabled = excluded.enabled,
                       granted_version = COALESCE(excluded.granted_version, plugins.granted_version),
                       granted_permissions = COALESCE(excluded.granted_permissions, plugins.granted_permissions),
                       updated_at = excluded.updated_at",
                    vec![
                        json!(e.manifest.name),
                        json!(e.enabled as i64),
                        v,
                        p,
                        json!(ts),
                    ],
                )
            })
            .collect()
    };
    if stmts.is_empty() {
        return Ok(());
    }
    inner.store.batch(TableAccess::All, stmts).await?;
    Ok(())
}

/// 新订阅从当前最大 ID 开始，不补投历史事件。
pub(crate) async fn init_cursors(inner: &Arc<Inner>) -> Result<(), HubError> {
    let pairs: Vec<(String, String)> = {
        let st = inner.state.lock().unwrap();
        let ephemeral: Vec<&String> = st
            .plugins
            .values()
            .flat_map(|e| e.manifest.events.ephemeral.iter())
            .collect();
        st.plugins
            .values()
            .flat_map(|e| {
                e.manifest
                    .events
                    .subscribe
                    .iter()
                    .filter(|k| !ephemeral.contains(k))
                    .map(|k| (e.manifest.name.clone(), k.clone()))
            })
            .collect()
    };
    for (plugin, kind) in pairs {
        execute(
            inner,
            "INSERT OR IGNORE INTO event_cursors (plugin, kind, last_id)
             VALUES (?, ?, (SELECT COALESCE(MAX(id), 0) FROM event_log))",
            vec![json!(plugin), json!(kind)],
        )
        .await
        .map_err(store_err)?;
    }
    Ok(())
}

pub(crate) async fn insert_event(inner: &Inner, ev: &Event) -> Result<i64, ServiceError> {
    execute(
        inner,
        "INSERT INTO event_log (kind, trace_id, source, created_at, payload) VALUES (?, ?, ?, ?, ?)",
        vec![
            json!(ev.kind),
            json!(ev.trace_id),
            json!(ev.source),
            json!(ev.created_at),
            json!(ev.payload.to_string()),
        ],
    )
    .await
}

/// 游标之后的一批事件。
pub(crate) async fn fetch_events(
    inner: &Inner,
    plugin: &str,
    kind: &str,
    limit: i64,
) -> Result<Vec<Event>, ServiceError> {
    let rows = query(
        inner,
        "SELECT id, kind, trace_id, source, created_at, payload FROM event_log
         WHERE kind = ? AND id > COALESCE(
           (SELECT last_id FROM event_cursors WHERE plugin = ? AND kind = ?), 0)
         ORDER BY id LIMIT ?",
        vec![json!(kind), json!(plugin), json!(kind), json!(limit)],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            Event {
                id: r.get("id").and_then(Value::as_i64).unwrap_or(0),
                kind: s("kind"),
                trace_id: s("trace_id"),
                source: s("source"),
                created_at: s("created_at"),
                payload: serde_json::from_str(&s("payload")).unwrap_or(Value::Null),
            }
        })
        .collect())
}

pub(crate) async fn set_cursor(
    inner: &Inner,
    plugin: &str,
    kind: &str,
    id: i64,
) -> Result<(), ServiceError> {
    execute(
        inner,
        "INSERT INTO event_cursors (plugin, kind, last_id) VALUES (?, ?, ?)
         ON CONFLICT(plugin, kind) DO UPDATE SET last_id = MAX(last_id, excluded.last_id)",
        vec![json!(plugin), json!(kind), json!(id)],
    )
    .await?;
    Ok(())
}

/// 删除超过保留期、且所有订阅者都已处理过的事件。
pub(crate) async fn cleanup(inner: &Inner) -> Result<(), ServiceError> {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(inner.retention_days);
    execute(
        inner,
        "DELETE FROM event_log WHERE created_at < ?
         AND id <= COALESCE((SELECT MIN(last_id) FROM event_cursors), id)",
        vec![json!(format_rfc3339(cutoff))],
    )
    .await?;
    Ok(())
}

/// 读取界面改过的插件配置：插件名 → {字段: 值}。
pub(crate) async fn load_overrides(inner: &Arc<Inner>) -> Result<(), HubError> {
    let rows = query(
        inner,
        "SELECT name, key, value FROM config_overrides",
        vec![],
    )
    .await
    .map_err(store_err)?;
    let mut ov = inner.overrides.lock().unwrap();
    for r in rows {
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let Ok(v) = serde_json::from_str::<Value>(&s("value")) else {
            continue;
        };
        ov.entry(s("name")).or_default().insert(s("key"), v);
    }
    Ok(())
}

/// 写入或删除（值为 null）一个插件配置覆盖值。
pub(crate) async fn set_override(
    inner: &Inner,
    name: &str,
    key: &str,
    value: &Value,
) -> Result<(), ServiceError> {
    if value.is_null() {
        execute(
            inner,
            "DELETE FROM config_overrides WHERE name = ? AND key = ?",
            vec![json!(name), json!(key)],
        )
        .await?;
    } else {
        execute(
            inner,
            "INSERT INTO config_overrides (name, key, value, updated_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(name, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            vec![json!(name), json!(key), json!(value.to_string()), json!(now())],
        )
        .await?;
    }
    Ok(())
}

/// 读应用级设置；没设过时返回 None。
pub(crate) async fn get_app(store: &SharedStore, key: &str) -> Result<Option<Value>, ServiceError> {
    let rows = store
        .query(
            TableAccess::All,
            Statement::new(
                "SELECT value FROM app_settings WHERE key = ?",
                vec![json!(key)],
            ),
        )
        .await?;
    Ok(rows
        .first()
        .and_then(|r| r.get("value"))
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str(s).ok()))
}

/// 写应用级设置；值为 null 时删除。
pub(crate) async fn set_app(inner: &Inner, key: &str, value: &Value) -> Result<(), ServiceError> {
    if value.is_null() {
        execute(
            inner,
            "DELETE FROM app_settings WHERE key = ?",
            vec![json!(key)],
        )
        .await?;
    } else {
        execute(
            inner,
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            vec![json!(key), json!(value.to_string()), json!(now())],
        )
        .await?;
    }
    Ok(())
}

/// 应用启动前读设置用：先建表再读，不需要 hub 实例。
pub(crate) async fn read_app_setting(
    store: &SharedStore,
    key: &str,
) -> Result<Option<Value>, HubError> {
    store
        .migrate(TableAccess::All, HUB.into(), 2, MIGRATION_V2.into())
        .await
        .map_err(store_err)?;
    get_app(store, key).await.map_err(store_err)
}

//! `source-status` 与 `item-query` 服务。

use plugin_sdk::contracts::source::{
    ItemList, ItemQuery, SourceStatus, SourceStatusList, StoredItem,
};
use plugin_sdk::contracts::{parse_params, to_result, Caller};
use plugin_sdk::{Context, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;
const MAX_DELETE: usize = 1000;
const ITEM_COLS: &str = "id, source_id, url, title, summary, published_at, collected_at";

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读取数据失败：{}", e.message))
}

pub async fn status_call(
    ctx: &Context,
    wake: &tokio::sync::Notify,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    match method {
        "list" => {}
        "retry" => return retry(ctx, wake, params).await,
        _ => return Err(ServiceError::method_not_found(method)),
    }
    #[derive(Deserialize)]
    struct Row {
        id: String,
        name: String,
        interval_secs: u64,
        last_success_at: Option<String>,
        last_error: Option<String>,
        consecutive_failures: i64,
        total_items: i64,
    }
    let rows: Vec<Row> = ctx
        .store()
        .query_as(
            "SELECT id, name, interval_secs, last_success_at, last_error, \
             consecutive_failures, total_items FROM sources ORDER BY id",
            vec![],
        )
        .await
        .map_err(db_err)?;
    let sources = rows
        .into_iter()
        .map(|r| {
            let state = if r.consecutive_failures >= 3 {
                "failing"
            } else if r.last_success_at.is_some() {
                "ok"
            } else {
                "unknown"
            };
            SourceStatus {
                id: r.id,
                name: r.name,
                state: state.to_string(),
                last_success_at: r.last_success_at,
                last_error: r.last_error,
                consecutive_failures: r.consecutive_failures,
                total_items: r.total_items,
                interval_secs: r.interval_secs,
            }
        })
        .collect();
    to_result(&SourceStatusList { sources })
}

/// `retry {id}`：清掉上次尝试时间并唤醒调度循环，不等抓取完成。
async fn retry(
    ctx: &Context,
    wake: &tokio::sync::Notify,
    params: Value,
) -> Result<Value, ServiceError> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("缺少 id"))?;
    let r = ctx
        .store()
        .execute(
            "UPDATE sources SET last_attempt_at = NULL WHERE id = ?",
            vec![json!(id)],
        )
        .await
        .map_err(db_err)?;
    if r.rows_affected == 0 {
        return Err(ServiceError::invalid_params(format!("未知信源：{id}")));
    }
    wake.notify_one();
    Ok(json!({ "ok": true }))
}

pub async fn item_call(
    ctx: &Context,
    caller: &Caller,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    match method {
        "delete" => {
            if caller.plugin != "maintenance" {
                return Err(ServiceError::forbidden("只允许 maintenance 删除消息"));
            }
            #[derive(Deserialize)]
            struct P {
                ids: Vec<i64>,
            }
            let p: P = parse_params(params)?;
            if p.ids.len() > MAX_DELETE {
                return Err(ServiceError::invalid_params("ids 最多 1000 个"));
            }
            if p.ids.is_empty() {
                return Ok(json!({ "deleted": 0 }));
            }
            let marks = vec!["?"; p.ids.len()].join(",");
            let sql = format!("DELETE FROM raw_items WHERE id IN ({marks})");
            let r = ctx
                .store()
                .execute(&sql, p.ids.iter().map(|i| json!(i)).collect())
                .await
                .map_err(db_err)?;
            Ok(json!({ "deleted": r.rows_affected }))
        }
        "get" => {
            #[derive(Deserialize)]
            struct P {
                ids: Vec<i64>,
            }
            let p: P = parse_params(params)?;
            if p.ids.len() > MAX_LIMIT as usize {
                return Err(ServiceError::invalid_params("ids 最多 500 个"));
            }
            if p.ids.is_empty() {
                return to_result(&ItemList { items: vec![] });
            }
            let marks = vec!["?"; p.ids.len()].join(",");
            let sql =
                format!("SELECT {ITEM_COLS} FROM raw_items WHERE id IN ({marks}) ORDER BY id");
            let items: Vec<StoredItem> = ctx
                .store()
                .query_as(&sql, p.ids.iter().map(|i| json!(i)).collect())
                .await
                .map_err(db_err)?;
            to_result(&ItemList { items })
        }
        "list" => {
            let q: ItemQuery = parse_params(params)?;
            let mut conds: Vec<&str> = vec![];
            let mut args: Vec<Value> = vec![];
            if let Some(v) = q.since {
                conds.push("collected_at >= ?");
                args.push(json!(v));
            }
            if let Some(id) = q.before_id {
                conds.push(
                    "(collected_at, id) < (SELECT collected_at, id FROM raw_items WHERE id = ?)",
                );
                args.push(json!(id));
            }
            if let Some(v) = q.until {
                conds.push("collected_at < ?");
                args.push(json!(v));
            }
            if let Some(v) = q.source_id {
                conds.push("source_id = ?");
                args.push(json!(v));
            }
            if let Some(t) = q.text.filter(|t| !t.is_empty()) {
                conds.push("(title LIKE ? ESCAPE '\\' OR summary LIKE ? ESCAPE '\\')");
                let pat = format!("%{}%", escape_like(&t));
                args.push(json!(pat));
                args.push(json!(pat));
            }
            let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            let wh = if conds.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conds.join(" AND "))
            };
            let sql = format!(
                "SELECT {ITEM_COLS} FROM raw_items{wh} ORDER BY collected_at DESC, id DESC LIMIT {limit}"
            );
            let items: Vec<StoredItem> = ctx.store().query_as(&sql, args).await.map_err(db_err)?;
            to_result(&ItemList { items })
        }
        "raw" => {
            #[derive(Deserialize)]
            struct P {
                id: i64,
            }
            let p: P = parse_params(params)?;
            let rows = ctx
                .store()
                .query("SELECT raw FROM raw_items WHERE id = ?", vec![json!(p.id)])
                .await
                .map_err(db_err)?;
            let raw = rows.first().and_then(|r| r.get("raw")).cloned();
            Ok(json!({ "raw": raw.unwrap_or(Value::Null) }))
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

fn escape_like(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

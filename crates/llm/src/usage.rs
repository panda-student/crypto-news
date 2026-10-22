//! 用量表：每次调用 upsert 一行，按日期查询。

use plugin_sdk::contracts::llm::{TokenUsage, UsageList, UsageQuery, UsageRow};
use plugin_sdk::contracts::{format_rfc3339, now_rfc3339, parse_rfc3339};
use plugin_sdk::{Context, ServiceError};
use serde_json::json;

/// 记一次调用；记账失败只写日志，不影响调用结果。
pub(crate) async fn record(
    ctx: &Context,
    model: &str,
    purpose: Option<&str>,
    usage: TokenUsage,
    failed: bool,
) {
    let r = ctx
        .store()
        .execute(
            "INSERT INTO usage (day, model, purpose, calls, input_tokens, output_tokens, failures) \
             VALUES (?, ?, ?, 1, ?, ?, ?) \
             ON CONFLICT (day, model, purpose) DO UPDATE SET \
             calls = calls + 1, \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             failures = failures + excluded.failures",
            vec![
                json!(now_rfc3339()[..10]),
                json!(model),
                json!(purpose.unwrap_or("other")),
                json!(usage.input_tokens),
                json!(usage.output_tokens),
                json!(u64::from(failed)),
            ],
        )
        .await;
    if let Err(e) = r {
        tracing::warn!("llm：记录用量失败：{e}");
    }
}

fn day_of(s: &str) -> Result<String, ServiceError> {
    let t = parse_rfc3339(s)
        .ok_or_else(|| ServiceError::invalid_params(format!("时间格式应为 RFC3339：{s}")))?;
    Ok(format_rfc3339(t)[..10].to_string())
}

pub(crate) async fn list(ctx: &Context, q: &UsageQuery) -> Result<UsageList, ServiceError> {
    let mut sql = String::from(
        "SELECT day, model, purpose, calls, input_tokens, output_tokens, failures \
         FROM usage WHERE 1 = 1",
    );
    let mut args = Vec::new();
    if let Some(s) = &q.since {
        sql.push_str(" AND day >= ?");
        args.push(json!(day_of(s)?));
    }
    if let Some(u) = &q.until {
        sql.push_str(" AND day < ?");
        args.push(json!(day_of(u)?));
    }
    sql.push_str(" ORDER BY day DESC, model, purpose");
    let rows: Vec<UsageRow> = ctx.store().query_as(&sql, args).await?;
    Ok(UsageList { rows })
}

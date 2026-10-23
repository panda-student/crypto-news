//! rulebook 插件：保存和读取用户的分级规则手册，没有保存过时返回内置缺省规则。

mod default;

use plugin_sdk::contracts::rulebook::{Rulebook, SERVICE};
use plugin_sdk::contracts::{now_rfc3339, parse_params, to_result};
use plugin_sdk::{BoxFuture, Caller, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};

pub use default::DEFAULT_RULEBOOK;

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "CREATE TABLE rulebook (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    text TEXT NOT NULL,
    updated_at TEXT NOT NULL
);";
const MAX_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct RulebookPlugin;

impl Plugin for RulebookPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            ctx.store().migrate(1, SCHEMA).await?;
            let c = ctx.clone();
            ctx.provide_fn(SERVICE, move |caller, method, params| {
                let c = c.clone();
                async move { handle(&c, &caller, &method, params).await }
            })?;
            Ok(())
        })
    }
}

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读写规则手册失败：{}", e.message))
}

async fn handle(
    ctx: &Context,
    caller: &Caller,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    match method {
        "get" => to_result(&get(ctx).await?),
        "set" => {
            if caller.user_id != plugin_sdk::contracts::OWNER
                || !matches!(caller.plugin.as_str(), "web" | "operations")
            {
                return Err(ServiceError::forbidden(
                    "只有界面或已确认的应用操作能修改规则手册",
                ));
            }
            #[derive(Deserialize)]
            struct P {
                text: String,
            }
            let p: P = parse_params(params)?;
            to_result(&set(ctx, &p.text).await?)
        }
        m => Err(ServiceError::method_not_found(m)),
    }
}

async fn get(ctx: &Context) -> Result<Rulebook, ServiceError> {
    #[derive(Deserialize)]
    struct Row {
        text: String,
        updated_at: String,
    }
    let rows: Vec<Row> = ctx
        .store()
        .query_as("SELECT text, updated_at FROM rulebook WHERE id = 1", vec![])
        .await
        .map_err(db_err)?;
    Ok(match rows.into_iter().next() {
        Some(r) => Rulebook {
            text: r.text,
            updated_at: Some(r.updated_at),
        },
        None => Rulebook {
            text: DEFAULT_RULEBOOK.to_string(),
            updated_at: None,
        },
    })
}

async fn set(ctx: &Context, text: &str) -> Result<Rulebook, ServiceError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ServiceError::invalid_params("规则文本不能为空"));
    }
    if text.len() > MAX_BYTES {
        return Err(ServiceError::invalid_params("规则文本不能超过 64 KiB"));
    }
    let now = now_rfc3339();
    ctx.store()
        .execute(
            "INSERT INTO rulebook (id, text, updated_at) VALUES (1, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
            vec![json!(text), json!(now)],
        )
        .await
        .map_err(db_err)?;
    Ok(Rulebook {
        text: text.to_string(),
        updated_at: Some(now),
    })
}

#[cfg(test)]
mod tests;

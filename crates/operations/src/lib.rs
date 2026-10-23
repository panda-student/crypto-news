//! Application operations are a Hub capability, independent of HTTP, MCP and AI.
mod catalogue;
mod execute;
mod inspect;
#[cfg(test)]
mod tests;

use plugin_sdk::contracts::auth::AuthClient;
use plugin_sdk::contracts::operations::{Confirmation, PrepareRequest, SERVICE};
use plugin_sdk::contracts::store::Statement;
use plugin_sdk::contracts::{new_trace_id, parse_params, Caller, OWNER};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

const SCHEMA: &str = "CREATE TABLE operation_plans (id TEXT PRIMARY KEY, value TEXT NOT NULL, created_at INTEGER NOT NULL)";

pub struct OperationsPlugin;
impl Plugin for OperationsPlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }
    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            ctx.store().migrate(1, SCHEMA).await?;
            ctx.store().migrate(2,"CREATE INDEX operation_plans_recent ON operation_plans (created_at DESC,id DESC)").await?;
            let operations = Arc::new(Operations {
                ctx: ctx.clone(),
                gate: tokio::sync::Mutex::new(()),
                imported: tokio::sync::Mutex::new(false),
            });
            operations.recover_interrupted().await?;
            ctx.provide_fn(SERVICE, move |caller, method, params| {
                let operations = operations.clone();
                async move { operations.call(caller, &method, params).await }
            })?;
            Ok(())
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Plan {
    id: String,
    tool: String,
    arguments: Value,
    origin: String,
    session_id: Option<i64>,
    created_at: i64,
    status: String,
    result: Option<String>,
}
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
struct Operations {
    ctx: Context,
    // Covers state transition AND side effect. Running plans are never replayed after restart.
    gate: tokio::sync::Mutex<()>,
    imported: tokio::sync::Mutex<bool>,
}
#[derive(Deserialize)]
struct Row {
    value: String,
}

impl Operations {
    async fn recover_interrupted(&self) -> Result<(), ServiceError> {
        let rows: Vec<Row> = self
            .ctx
            .store()
            .query_as(
                "SELECT value FROM operation_plans WHERE value LIKE ?",
                vec![json!("%\"status\":\"running\"%")],
            )
            .await?;
        for row in rows {
            let mut plan: Plan = serde_json::from_str(&row.value)
                .map_err(|_| ServiceError::internal("计划恢复失败"))?;
            if plan.status == "running" {
                interrupt(&mut plan);
                self.save(&plan).await?;
            }
        }
        Ok(())
    }
    async fn call(
        &self,
        caller: Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        if caller.user_id != OWNER || !matches!(caller.plugin.as_str(), "web" | "agent") {
            return Err(ServiceError::forbidden("应用操作只接受界面与内部Agent调用"));
        }
        if matches!(method, "confirm" | "cancel" | "plans") && caller.plugin != "web" {
            return Err(ServiceError::forbidden(
                "AI不能确认或管理待执行计划，请在界面操作",
            ));
        }
        let action = if matches!(method, "catalogue" | "inspect" | "status" | "plans") {
            "web.view"
        } else {
            "web.manage"
        };
        self.authorize(action).await?;
        match method {
            "catalogue" => Ok(json!(catalogue::catalogue())),
            "inspect" => inspect::inspect(&self.ctx).await,
            "prepare" => {
                let req: PrepareRequest = parse_params(params)?;
                catalogue::validate(catalogue::spec(&req.tool)?, &req.arguments)?;
                let plan = Plan {
                    id: new_trace_id(),
                    tool: req.tool,
                    arguments: req.arguments,
                    origin: if caller.plugin == "agent" {
                        "internal"
                    } else {
                        "external"
                    }
                    .into(),
                    session_id: req.session_id,
                    created_at: now(),
                    status: "pending".into(),
                    result: None,
                };
                self.save(&plan).await?;
                Ok(view(&plan))
            }
            "status" => Ok(view(&self.load(id(&params)?).await?)),
            "plans" => {
                self.import_legacy().await?;
                let rows: Vec<Row> = self.ctx.store().query_as("SELECT value FROM operation_plans ORDER BY created_at DESC, id DESC LIMIT 100", vec![]).await?;
                let plans: Vec<Plan> = rows
                    .into_iter()
                    .map(|r| {
                        serde_json::from_str(&r.value)
                            .map_err(|_| ServiceError::internal("计划读取失败"))
                    })
                    .collect::<Result<_, _>>()?;
                Ok(
                    json!({"plans":plans.iter().map(view).collect::<Vec<_>>(),"tools":catalogue::TOOLS.len()+2}),
                )
            }
            "confirm" => {
                self.confirm(id(&params)?, parse_params(params.clone())?)
                    .await
            }
            "cancel" => {
                let _guard = self.gate.lock().await;
                let mut plan = self.load(id(&params)?).await?;
                if plan.status != "pending" {
                    return Err(ServiceError::invalid_params("计划已处理"));
                }
                plan.status = "cancelled".into();
                self.save(&plan).await?;
                Ok(view(&plan))
            }
            _ => Err(ServiceError::method_not_found(method)),
        }
    }
    async fn authorize(&self, action: &str) -> Result<(), ServiceError> {
        if AuthClient(self.ctx.service("auth")).check(action).await? {
            Ok(())
        } else {
            Err(ServiceError::forbidden("没有操作权限"))
        }
    }
    async fn save(&self, plan: &Plan) -> Result<(), ServiceError> {
        self.ctx.store().execute("INSERT INTO operation_plans (id,value,created_at) VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET value=excluded.value", vec![json!(plan.id),json!(serde_json::to_string(plan).map_err(ServiceError::internal)?),json!(plan.created_at)]).await?;
        Ok(())
    }
    async fn load(&self, id: &str) -> Result<Plan, ServiceError> {
        let read = || self.ctx.store();
        let mut rows: Vec<Row> = read()
            .query_as(
                "SELECT value FROM operation_plans WHERE id=?",
                vec![json!(id)],
            )
            .await?;
        if rows.is_empty() {
            self.import_legacy().await?;
            rows = read()
                .query_as(
                    "SELECT value FROM operation_plans WHERE id=?",
                    vec![json!(id)],
                )
                .await?;
        }
        let row = rows
            .pop()
            .ok_or_else(|| ServiceError::invalid_params("计划不存在"))?;
        serde_json::from_str(&row.value).map_err(|_| ServiceError::internal("计划读取失败"))
    }
    async fn import_legacy(&self) -> Result<(), ServiceError> {
        let mut imported = self.imported.lock().await;
        if *imported {
            return Ok(());
        }
        // Ask the owner to export its data; never access web's tables directly.
        let legacy = self
            .ctx
            .service("workspace-state")
            .call("legacy_plans", json!({}))
            .await?;
        let mut plans: Vec<Plan> = serde_json::from_value(legacy["plans"].clone())
            .map_err(|_| ServiceError::internal("旧计划迁移失败"))?;
        for plan in &mut plans {
            if plan.status == "running" {
                interrupt(plan);
            }
        }
        let statements = plans
            .iter()
            .map(|p| {
                Ok(Statement::new(
                    "INSERT OR IGNORE INTO operation_plans (id,value,created_at) VALUES (?,?,?)",
                    vec![
                        json!(p.id),
                        json!(serde_json::to_string(p).map_err(ServiceError::internal)?),
                        json!(p.created_at),
                    ],
                ))
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        for chunk in statements.chunks(100) {
            self.ctx.store().batch(chunk.to_vec()).await?;
        }
        *imported = true;
        Ok(())
    }
    async fn confirm(&self, id: &str, input: Confirmation) -> Result<Value, ServiceError> {
        let _guard = self.gate.lock().await;
        let mut p = self.load(id).await?;
        if p.status != "pending" || now() - p.created_at >= 3600 {
            return Err(ServiceError::invalid_params(
                "计划已处理或已过期，请重新发起",
            ));
        }
        let spec = catalogue::spec(&p.tool)?;
        catalogue::validate(spec, &p.arguments)?;
        let action = match p.tool.as_str() {
            "rules_save" => "rules.write",
            "secret_save" | "secret_delete" => "secrets.write",
            "item_read" | "item_star" | "items_read_all" | "item_level" => "items.write",
            "channel_test" | "channel_bind" => "channels.write",
            "session_rename" | "session_delete" => "chat.write",
            _ => "web.manage",
        };
        self.authorize(action).await?;
        if !spec.secret.is_empty()
            && matches!(p.tool.as_str(), "secret_save" | "webhook_save")
            && input
                .secrets
                .get(spec.secret)
                .is_none_or(|v| v.trim().is_empty())
        {
            return Err(ServiceError::invalid_params("请填写安全输入框"));
        }
        if input.secrets.keys().any(|k| k != spec.secret) {
            return Err(ServiceError::invalid_params("不支持的安全输入字段"));
        }
        p.status = "running".into();
        self.save(&p).await?;
        let result = execute::execute(&self.ctx, &p.tool, p.arguments.clone(), input.secrets).await;
        p.status = if result.is_ok() {
            "completed"
        } else {
            "failed"
        }
        .into();
        // Domain failures must not leak secrets via diagnostics or persisted plans.
        p.result = Some(
            if result.is_ok() {
                "应用已处理该操作，请核对对应设置。"
            } else {
                "应用校验未通过或服务不可用，请检查对应设置；未自动重试。"
            }
            .into(),
        );
        self.save(&p).await?;
        let mut response = view(&p);
        if p.tool == "channel_bind" {
            if let Ok(value) = result {
                response["binding"] = value;
            }
        }
        Ok(response)
    }
}
fn id(params: &Value) -> Result<&str, ServiceError> {
    params["id"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 100
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .ok_or_else(|| ServiceError::invalid_params("无效计划编号"))
}
fn view(p: &Plan) -> Value {
    let spec = catalogue::spec(&p.tool).ok();
    json!({"id":p.id,"tool":p.tool,"title":spec.map(|s|s.title),"arguments":p.arguments,"status":if p.status=="pending"&&now()-p.created_at>=3600{"expired"}else{&p.status},"result":p.result,"origin":p.origin,"secret":spec.map(|s|s.secret).unwrap_or("")})
}

fn interrupt(plan: &mut Plan) {
    plan.status = "interrupted".into();
    plan.result = Some("上次操作被中断，结果待核对；为避免重复修改，不会自动重试。".into());
}

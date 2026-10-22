//! `store` 服务 1.0 与存储后端接口。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{call_typed, BoxFuture, Service, ServiceError};

pub const SERVICE: &str = "store";

/// 一次调用可访问的表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableAccess {
    /// 不受限，只有 hub 使用
    All,
    /// 只能访问这些表（插件清单 `tables`）
    Only(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

impl Statement {
    pub fn new(sql: impl Into<String>, params: Vec<Value>) -> Self {
        Statement {
            sql: sql.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecResult {
    pub rows_affected: u64,
    pub last_insert_id: i64,
}

pub type Row = Map<String, Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rows {
    pub rows: Vec<Row>,
}

/// 存储后端。由 store 插件实现，app 打开后交给 hub；hub 把它登记成 `store` 服务。
///
/// 参数只支持 JSON 的 null、布尔、整数、浮点数和字符串；越权访问返回 -32001。
pub trait StoreBackend: Send + Sync {
    /// 执行迁移。`owner` 是插件名，每个插件独立记录版本；同一版本只执行一次。
    /// 返回本次是否真的执行了。`sql` 可以包含多条语句。
    fn migrate(
        &self,
        access: TableAccess,
        owner: String,
        version: i64,
        sql: String,
    ) -> BoxFuture<'_, Result<bool, ServiceError>>;

    fn execute(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> BoxFuture<'_, Result<ExecResult, ServiceError>>;

    fn query(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> BoxFuture<'_, Result<Vec<Row>, ServiceError>>;

    /// 在一个事务里依次执行。
    fn batch(
        &self,
        access: TableAccess,
        stmts: Vec<Statement>,
    ) -> BoxFuture<'_, Result<Vec<ExecResult>, ServiceError>>;

    /// 把整个数据库一致地备份到 `path`（不存在的新文件），返回字节数。
    /// 缺省不支持。
    fn backup(&self, path: String) -> BoxFuture<'_, Result<u64, ServiceError>> {
        let _ = path;
        Box::pin(async { Err(ServiceError::unavailable("存储后端不支持备份")) })
    }
}

/// 只有这个插件能调用 `store.backup`。
pub const BACKUP_PLUGIN: &str = "maintenance";

pub type SharedStore = Arc<dyn StoreBackend>;

/// `store` 服务的类型化客户端。
#[derive(Clone)]
pub struct StoreClient(pub Service);

impl StoreClient {
    pub async fn migrate(&self, version: i64, sql: &str) -> Result<bool, ServiceError> {
        let r: Value = call_typed(
            &*self.0,
            "migrate",
            &json!({ "version": version, "sql": sql }),
        )
        .await?;
        Ok(r.get("applied").and_then(Value::as_bool).unwrap_or(false))
    }

    pub async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<ExecResult, ServiceError> {
        call_typed(&*self.0, "execute", &Statement::new(sql, params)).await
    }

    pub async fn query(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>, ServiceError> {
        let r: Rows = call_typed(&*self.0, "query", &Statement::new(sql, params)).await?;
        Ok(r.rows)
    }

    /// 查询并把每行转换成类型 `T`。
    pub async fn query_as<T: serde::de::DeserializeOwned>(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<T>, ServiceError> {
        self.query(sql, params)
            .await?
            .into_iter()
            .map(|r| {
                serde_json::from_value(Value::Object(r))
                    .map_err(|e| ServiceError::internal(format!("行格式不符：{e}")))
            })
            .collect()
    }

    pub async fn batch(&self, stmts: Vec<Statement>) -> Result<Vec<ExecResult>, ServiceError> {
        #[derive(Deserialize)]
        struct R {
            results: Vec<ExecResult>,
        }
        let r: R = call_typed(&*self.0, "batch", &json!({ "statements": stmts })).await?;
        Ok(r.results)
    }

    /// 备份整个数据库到 `path`，返回字节数。只有 maintenance 插件能调用。
    pub async fn backup(&self, path: &str) -> Result<u64, ServiceError> {
        let r: Value = self.0.call("backup", json!({ "path": path })).await?;
        Ok(r.get("bytes").and_then(Value::as_u64).unwrap_or(0))
    }
}

/// 把 `store` 服务调用转给后端：hub 和测试都用它。
pub async fn dispatch_store_call(
    backend: &dyn StoreBackend,
    access: TableAccess,
    owner: &str,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    match method {
        "migrate" => {
            #[derive(Deserialize)]
            struct P {
                version: i64,
                sql: String,
            }
            let p: P = crate::parse_params(params)?;
            let applied = backend
                .migrate(access, owner.to_string(), p.version, p.sql)
                .await?;
            Ok(json!({ "applied": applied }))
        }
        "execute" => {
            let s: Statement = crate::parse_params(params)?;
            crate::to_result(&backend.execute(access, s).await?)
        }
        "query" => {
            let s: Statement = crate::parse_params(params)?;
            Ok(json!({ "rows": backend.query(access, s).await? }))
        }
        "batch" => {
            #[derive(Deserialize)]
            struct P {
                statements: Vec<Statement>,
            }
            let p: P = crate::parse_params(params)?;
            Ok(json!({ "results": backend.batch(access, p.statements).await? }))
        }
        "backup" => {
            if owner != BACKUP_PLUGIN {
                return Err(ServiceError::forbidden("只有 maintenance 能备份数据库"));
            }
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let p: P = crate::parse_params(params)?;
            Ok(json!({ "bytes": backend.backup(p.path).await? }))
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

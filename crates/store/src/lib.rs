//! SQLite 存储后端。
//!
//! - 只用运行时查询；WAL、busy_timeout 5 秒、外键开。
//! - 表访问限制用 SQLite authorizer 实现，每次操作在取到的连接上设置、结束时清除。
//! - query 结果里的 BLOB 列不支持（没有 base64 依赖），遇到时返回 -32002 错误。

mod authorizer;

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use contracts::store::{ExecResult, Row, Statement, StoreBackend, TableAccess};
use contracts::{BoxFuture, ServiceError};
use serde_json::{Number, Value};
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Column, Connection, Row as _, Sqlite, SqlitePool, TypeInfo, ValueRef};

use authorizer::AuthGuard;

const MIGRATIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS _migrations (\
    owner TEXT NOT NULL, version INTEGER NOT NULL, applied_at TEXT NOT NULL, \
    PRIMARY KEY (owner, version))";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("打开数据库失败：{0}")]
    Open(String),
}

pub struct SqliteStore {
    pool: SqlitePool,
}

/// 打开（必要时创建）文件数据库。
pub async fn open(path: &Path) -> Result<Arc<SqliteStore>, StoreError> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    build(opts, 4).await
}

/// 内存数据库，给测试用。只有一个连接，数据随进程结束消失。
pub async fn open_memory() -> Result<Arc<SqliteStore>, StoreError> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(|e| StoreError::Open(e.to_string()))?;
    build(opts, 1).await
}

async fn build(opts: SqliteConnectOptions, max: u32) -> Result<Arc<SqliteStore>, StoreError> {
    let opts = opts
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(max)
        // 内存库的数据绑在连接上，连接不能被回收
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .map_err(|e| StoreError::Open(e.to_string()))?;
    sqlx::query(MIGRATIONS_DDL)
        .execute(&pool)
        .await
        .map_err(|e| StoreError::Open(e.to_string()))?;
    Ok(Arc::new(SqliteStore { pool }))
}

fn map_err(e: sqlx::Error) -> ServiceError {
    if let sqlx::Error::Database(d) = &e {
        // SQLITE_AUTH：被 authorizer 拒绝
        if d.code().as_deref() == Some("23") {
            return ServiceError::forbidden("无权访问该表或执行该操作");
        }
        return ServiceError::internal(format!("数据库错误：{}", d.message()));
    }
    ServiceError::internal(format!("数据库错误：{e}"))
}

type Query<'q> = sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments<'q>>;

fn bind_params(stmt: &Statement) -> Result<Query<'_>, ServiceError> {
    let mut q = sqlx::query(&stmt.sql).persistent(false);
    for p in &stmt.params {
        q = match p {
            Value::Null => q.bind(None::<i64>),
            Value::Bool(b) => q.bind(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    q.bind(i)
                } else if n.is_u64() {
                    return Err(ServiceError::invalid_params("整数超出 i64 范围"));
                } else {
                    q.bind(n.as_f64().unwrap_or(0.0))
                }
            }
            Value::String(s) => q.bind(s.as_str()),
            _ => {
                return Err(ServiceError::invalid_params(
                    "参数只支持 null、布尔、数字和字符串",
                ))
            }
        };
    }
    Ok(q)
}

fn convert_row(row: &sqlx::sqlite::SqliteRow) -> Result<Row, ServiceError> {
    let mut out = Row::new();
    for (i, col) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(i).map_err(map_err)?;
        let v = if raw.is_null() {
            Value::Null
        } else {
            let ty = raw.type_info().name().to_ascii_uppercase();
            match ty.as_str() {
                "INTEGER" | "INT" | "INT4" | "INT8" | "BIGINT" | "BOOLEAN" => {
                    Value::from(row.try_get::<i64, _>(i).map_err(map_err)?)
                }
                "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" => {
                    let f = row.try_get::<f64, _>(i).map_err(map_err)?;
                    Number::from_f64(f).map_or(Value::Null, Value::Number)
                }
                "BLOB" => return Err(ServiceError::internal("查询结果含 BLOB 列，暂不支持")),
                _ => Value::String(row.try_get::<String, _>(i).map_err(map_err)?),
            }
        };
        out.insert(col.name().to_string(), v);
    }
    Ok(out)
}

fn limit(access: &TableAccess) -> Option<&[String]> {
    match access {
        TableAccess::All => None,
        TableAccess::Only(t) => Some(t),
    }
}

impl SqliteStore {
    /// 用 `VACUUM INTO` 一致地复制整个库到新文件。目标已存在时拒绝，不覆盖。
    async fn do_backup(&self, path: String) -> Result<u64, ServiceError> {
        if path.is_empty() {
            return Err(ServiceError::invalid_params("备份路径不能为空"));
        }
        if Path::new(&path).exists() {
            return Err(ServiceError::invalid_params("备份目标文件已存在"));
        }
        let mut conn = self.conn().await?;
        sqlx::query("VACUUM INTO ?")
            .bind(&path)
            .execute(&mut *conn)
            .await
            .map_err(map_err)?;
        std::fs::metadata(&path)
            .map(|m| m.len())
            .map_err(|e| ServiceError::internal(format!("读取备份文件失败：{e}")))
    }

    async fn conn(&self) -> Result<PoolConnection<Sqlite>, ServiceError> {
        self.pool.acquire().await.map_err(map_err)
    }

    /// 在连接上按访问范围设置 authorizer；返回的守卫析构时清除。
    async fn guard(
        conn: &mut PoolConnection<Sqlite>,
        access: &TableAccess,
    ) -> Result<Option<AuthGuard>, ServiceError> {
        let Some(tables) = limit(access) else {
            return Ok(None);
        };
        let mut h = conn.lock_handle().await.map_err(map_err)?;
        // SAFETY：句柄属于该连接；守卫总是在连接归还连接池前析构
        Ok(Some(unsafe {
            AuthGuard::install(h.as_raw_handle().as_ptr(), tables)
        }))
    }

    async fn do_execute(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> Result<ExecResult, ServiceError> {
        let mut conn = self.conn().await?;
        let _g = Self::guard(&mut conn, &access).await?;
        run_exec(&mut conn, &stmt).await
    }

    async fn do_query(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> Result<Vec<Row>, ServiceError> {
        let mut conn = self.conn().await?;
        let _g = Self::guard(&mut conn, &access).await?;
        let rows = bind_params(&stmt)?
            .fetch_all(&mut *conn)
            .await
            .map_err(map_err)?;
        rows.iter().map(convert_row).collect()
    }

    async fn do_batch(
        &self,
        access: TableAccess,
        stmts: Vec<Statement>,
    ) -> Result<Vec<ExecResult>, ServiceError> {
        let mut conn = self.conn().await?;
        let _g = Self::guard(&mut conn, &access).await?;
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await.map_err(map_err)?;
        let mut out = Vec::with_capacity(stmts.len());
        for s in &stmts {
            match run_exec(&mut tx, s).await {
                Ok(r) => out.push(r),
                Err(e) => {
                    let _ = tx.rollback().await;
                    return Err(e);
                }
            }
        }
        tx.commit().await.map_err(map_err)?;
        Ok(out)
    }

    async fn do_migrate(
        &self,
        access: TableAccess,
        owner: String,
        version: i64,
        sql: String,
    ) -> Result<bool, ServiceError> {
        let mut conn = self.conn().await?;
        let raw = SendPtr({
            let mut h = conn.lock_handle().await.map_err(map_err)?;
            h.as_raw_handle().as_ptr()
        });
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await.map_err(map_err)?;
        let res = migrate_in_tx(&mut tx, raw, &access, &owner, version, &sql).await;
        match res {
            Ok(true) => {
                tx.commit().await.map_err(map_err)?;
                Ok(true)
            }
            other => {
                let _ = tx.rollback().await;
                other
            }
        }
    }
}

/// 连接句柄指针，只在持有该连接的任务里使用。
struct SendPtr(*mut libsqlite3_sys::sqlite3);
unsafe impl Send for SendPtr {}

async fn run_exec(
    conn: &mut sqlx::SqliteConnection,
    stmt: &Statement,
) -> Result<ExecResult, ServiceError> {
    let r = bind_params(stmt)?.execute(conn).await.map_err(map_err)?;
    Ok(ExecResult {
        rows_affected: r.rows_affected(),
        last_insert_id: r.last_insert_rowid(),
    })
}

async fn migrate_in_tx(
    tx: &mut sqlx::SqliteConnection,
    raw: SendPtr,
    access: &TableAccess,
    owner: &str,
    version: i64,
    sql: &str,
) -> Result<bool, ServiceError> {
    // 迁移记录表只由这里访问，此时没有设置 authorizer
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _migrations WHERE owner = ?")
            .persistent(false)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await
            .map_err(map_err)?;
    // 已执行过的版本（含低于最大版本的）视为幂等无操作，允许插件每次启动重复提交
    let done: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM _migrations WHERE owner = ? AND version = ?")
            .persistent(false)
            .bind(owner)
            .bind(version)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_err)?;
    if done.is_some() {
        return Ok(false);
    }
    match max {
        Some(m) if version == m => return Ok(false),
        Some(m) if version < m => {
            return Err(ServiceError::invalid_params(
                "迁移版本必须大于已执行的最大版本",
            ))
        }
        _ => {}
    }
    {
        // SAFETY：raw 是 tx 所属连接的句柄，守卫在本块结束时清除
        let _g = limit(access).map(|t| unsafe { AuthGuard::install(raw.0, t) });
        // 迁移 SQL 可含多条语句，不绑定参数
        sqlx::query(sql)
            .persistent(false)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
    }
    sqlx::query("INSERT INTO _migrations (owner, version, applied_at) VALUES (?, ?, ?)")
        .persistent(false)
        .bind(owner)
        .bind(version)
        .bind(contracts::now_rfc3339())
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;
    Ok(true)
}

impl StoreBackend for SqliteStore {
    fn migrate(
        &self,
        access: TableAccess,
        owner: String,
        version: i64,
        sql: String,
    ) -> BoxFuture<'_, Result<bool, ServiceError>> {
        Box::pin(self.do_migrate(access, owner, version, sql))
    }

    fn execute(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> BoxFuture<'_, Result<ExecResult, ServiceError>> {
        Box::pin(self.do_execute(access, stmt))
    }

    fn query(
        &self,
        access: TableAccess,
        stmt: Statement,
    ) -> BoxFuture<'_, Result<Vec<Row>, ServiceError>> {
        Box::pin(self.do_query(access, stmt))
    }

    fn batch(
        &self,
        access: TableAccess,
        stmts: Vec<Statement>,
    ) -> BoxFuture<'_, Result<Vec<ExecResult>, ServiceError>> {
        Box::pin(self.do_batch(access, stmts))
    }

    fn backup(&self, path: String) -> BoxFuture<'_, Result<u64, ServiceError>> {
        Box::pin(self.do_backup(path))
    }
}

#[cfg(test)]
mod tests;

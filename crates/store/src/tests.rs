use contracts::store::{Statement, StoreBackend, TableAccess};
use contracts::ServiceError;
use serde_json::{json, Value};

use super::*;

fn only(t: &[&str]) -> TableAccess {
    TableAccess::Only(t.iter().map(|s| s.to_string()).collect())
}

fn st(sql: &str, params: Vec<Value>) -> Statement {
    Statement::new(sql, params)
}

async fn count(s: &SqliteStore, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) AS n FROM {table}");
    let rows = s.query(TableAccess::All, st(&sql, vec![])).await.unwrap();
    rows[0]["n"].as_i64().unwrap()
}

#[tokio::test]
async fn migrate_once_and_per_owner() {
    let s = open_memory().await.unwrap();
    let a = only(&["a"]);
    let sql = "CREATE TABLE a (id INTEGER PRIMARY KEY);".to_string();
    assert!(s
        .migrate(a.clone(), "p1".into(), 1, sql.clone())
        .await
        .unwrap());
    // 相同版本不再执行（否则建表会失败）
    assert!(!s
        .migrate(a.clone(), "p1".into(), 1, sql.clone())
        .await
        .unwrap());
    // 不同 owner 互不影响
    let b = only(&["b"]);
    assert!(s
        .migrate(b, "p2".into(), 1, "CREATE TABLE b (id INTEGER);".into())
        .await
        .unwrap());
    assert!(s
        .migrate(
            a.clone(),
            "p1".into(),
            3,
            "CREATE TABLE a2x (x INTEGER);".into()
        )
        .await
        .is_err()); // a2x 不在允许范围
    assert!(s
        .migrate(a.clone(), "p1".into(), 2, "CREATE INDEX i ON a(id);".into())
        .await
        .unwrap());
    let e = s
        .migrate(a, "p1".into(), 0, "SELECT 1".into())
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
}

#[tokio::test]
async fn migrate_resubmitting_applied_versions_is_noop() {
    let s = open_memory().await.unwrap();
    let a = only(&["a", "a2"]);
    let v1 = "CREATE TABLE a (id INTEGER PRIMARY KEY);".to_string();
    let v2 = "CREATE TABLE a2 (id INTEGER PRIMARY KEY);".to_string();
    assert!(s
        .migrate(a.clone(), "p".into(), 1, v1.clone())
        .await
        .unwrap());
    assert!(s
        .migrate(a.clone(), "p".into(), 2, v2.clone())
        .await
        .unwrap());
    // 插件重启：按顺序再提交 1、2，都不重复执行
    assert!(!s.migrate(a.clone(), "p".into(), 1, v1).await.unwrap());
    assert!(!s.migrate(a.clone(), "p".into(), 2, v2).await.unwrap());
    // 从未执行过、且比最大版本小的新版本仍然报错
    let e = s
        .migrate(a, "p".into(), 0, "SELECT 1".into())
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
}

#[tokio::test]
async fn failed_migration_rolls_back() {
    let s = open_memory().await.unwrap();
    let a = only(&["a", "c"]);
    let sql =
        "CREATE TABLE a (id INTEGER); CREATE TABLE c (id INTEGER); INSERT INTO nope VALUES (1);";
    assert!(s
        .migrate(a.clone(), "p".into(), 1, sql.into())
        .await
        .is_err());
    let rows = s
        .query(
            TableAccess::All,
            st(
                "SELECT name FROM sqlite_master WHERE name IN ('a','c')",
                vec![],
            ),
        )
        .await
        .unwrap();
    assert!(rows.is_empty());
    // 版本没被记录，可以重新执行
    assert!(s
        .migrate(a, "p".into(), 1, "CREATE TABLE a (id INTEGER);".into())
        .await
        .unwrap());
}

#[tokio::test]
async fn table_access_limits() {
    let s = open_memory().await.unwrap();
    s.migrate(
        TableAccess::All,
        "t".into(),
        1,
        "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT); CREATE TABLE b (id INTEGER);"
            .into(),
    )
    .await
    .unwrap();
    let a = only(&["a"]);
    // a 可读写
    let r = s
        .execute(
            a.clone(),
            st("INSERT INTO a (v) VALUES (?)", vec![json!("x")]),
        )
        .await
        .unwrap();
    assert_eq!(r.rows_affected, 1);
    assert_eq!(r.last_insert_id, 1);
    assert_eq!(
        s.query(a.clone(), st("SELECT v FROM a", vec![]))
            .await
            .unwrap()[0]["v"],
        "x"
    );
    // 越权
    let denied = [
        "SELECT * FROM b",
        "INSERT INTO b VALUES (1)",
        "CREATE TABLE b2 (id INTEGER)",
        "DROP TABLE b",
        "ATTACH DATABASE ':memory:' AS z",
        "PRAGMA foreign_keys = OFF",
        "PRAGMA journal_mode = DELETE",
        "SELECT * FROM _migrations",
        "SELECT * FROM a, b",
    ];
    for sql in denied {
        let e = s.execute(a.clone(), st(sql, vec![])).await.unwrap_err();
        assert_eq!(e.code, ServiceError::FORBIDDEN, "{sql}");
        let e = s.query(a.clone(), st(sql, vec![])).await.unwrap_err();
        assert_eq!(e.code, ServiceError::FORBIDDEN, "{sql}");
    }
    let e = s
        .migrate(
            a.clone(),
            "t".into(),
            2,
            "CREATE TABLE b3 (id INTEGER)".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::FORBIDDEN);
    // authorizer 已清除：All 仍可访问 b，且同一条 SQL 不会命中缓存绕过
    assert_eq!(count(&s, "b").await, 0);
    assert!(s.query(a, st("SELECT * FROM b", vec![])).await.is_err());
}

#[tokio::test]
async fn params_roundtrip() {
    let s = open_memory().await.unwrap();
    s.execute(
        TableAccess::All,
        st(
            "CREATE TABLE t (i INTEGER, r REAL, s TEXT, n TEXT, b INTEGER)",
            vec![],
        ),
    )
    .await
    .unwrap();
    s.execute(
        TableAccess::All,
        st(
            "INSERT INTO t VALUES (?, ?, ?, ?, ?)",
            vec![
                json!(-7),
                json!(1.5),
                json!("你好"),
                Value::Null,
                json!(true),
            ],
        ),
    )
    .await
    .unwrap();
    let rows = s
        .query(
            TableAccess::All,
            st("SELECT * FROM t WHERE i = ?", vec![json!(-7)]),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["i"], json!(-7));
    assert_eq!(r["r"], json!(1.5));
    assert_eq!(r["s"], json!("你好"));
    assert_eq!(r["n"], Value::Null);
    assert_eq!(r["b"], json!(1));
    for bad in [json!([1]), json!({"a": 1}), json!(u64::MAX)] {
        let e = s
            .execute(TableAccess::All, st("SELECT ?", vec![bad]))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    }
}

#[tokio::test]
async fn batch_rolls_back() {
    let s = open_memory().await.unwrap();
    s.execute(
        TableAccess::All,
        st("CREATE TABLE t (id INTEGER PRIMARY KEY)", vec![]),
    )
    .await
    .unwrap();
    let ok = s
        .batch(
            TableAccess::All,
            vec![
                st("INSERT INTO t VALUES (1)", vec![]),
                st("INSERT INTO t VALUES (2)", vec![]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(ok.len(), 2);
    let e = s
        .batch(
            TableAccess::All,
            vec![
                st("INSERT INTO t VALUES (3)", vec![]),
                st("INSERT INTO t VALUES (1)", vec![]),
            ],
        )
        .await;
    assert!(e.is_err());
    assert_eq!(count(&s, "t").await, 2);
}

#[tokio::test]
async fn file_db_persists() {
    let dir = std::env::temp_dir().join(format!("store-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    {
        let s = open(&path).await.unwrap();
        assert!(s
            .migrate(
                only(&["t"]),
                "p".into(),
                1,
                "CREATE TABLE t (v TEXT);".into()
            )
            .await
            .unwrap());
        s.execute(
            only(&["t"]),
            st("INSERT INTO t VALUES (?)", vec![json!("keep")]),
        )
        .await
        .unwrap();
    }
    let s = open(&path).await.unwrap();
    let rows = s
        .query(only(&["t"]), st("SELECT v FROM t", vec![]))
        .await
        .unwrap();
    assert_eq!(rows[0]["v"], "keep");
    // 迁移记录也在
    assert!(!s
        .migrate(
            only(&["t"]),
            "p".into(),
            1,
            "CREATE TABLE t (v TEXT);".into()
        )
        .await
        .unwrap());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn backup_copies_whole_db_and_refuses_overwrite() {
    let dir = std::env::temp_dir().join(format!("store-backup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = open(&dir.join("db.sqlite")).await.unwrap();
    s.migrate(
        only(&["t"]),
        "p".into(),
        1,
        "CREATE TABLE t (v TEXT);".into(),
    )
    .await
    .unwrap();
    s.execute(only(&["t"]), st("INSERT INTO t VALUES ('b')", vec![]))
        .await
        .unwrap();

    let target = dir.join("backup.sqlite");
    let target_s = target.to_string_lossy().to_string();
    let bytes = s.backup(target_s.clone()).await.unwrap();
    assert!(bytes > 0);
    assert_eq!(bytes, std::fs::metadata(&target).unwrap().len());

    let copy = open(&target).await.unwrap();
    let rows = copy
        .query(only(&["t"]), st("SELECT v FROM t", vec![]))
        .await
        .unwrap();
    assert_eq!(rows[0]["v"], "b");
    drop(copy);

    let e = s.backup(target_s).await.unwrap_err();
    assert_eq!(e.code, ServiceError::INVALID_PARAMS);
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_migrate_and_batch_on_file_db() {
    let dir = std::env::temp_dir().join(format!("store-conc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.db");
    let s = open(&path).await.unwrap();
    let mut hs = Vec::new();
    for i in 0..12 {
        let s = s.clone();
        hs.push(tokio::spawn(async move {
            let t = format!("t{i}");
            let a = only(&[&t]);
            s.migrate(
                a.clone(),
                format!("p{i}"),
                1,
                format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, v INTEGER);"),
            )
            .await?;
            let stmts = (0..20)
                .map(|n| st(&format!("INSERT INTO {t} (v) VALUES (?)"), vec![json!(n)]))
                .collect();
            s.batch(a, stmts).await
        }));
    }
    for h in hs {
        h.await.unwrap().unwrap();
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

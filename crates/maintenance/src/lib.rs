//! 维护插件：每天在设定时间备份数据库（保留最近 N 份），并删除过期的归档级消息。
//!
//! 每次 `minute` tick（以及启动时）检查今天是否已做；做过的日期记在内存，
//! 启动后第一次检查若备份目录已有今天的文件，也视为已做。各步骤失败只记日志。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plugin_sdk::contracts::events::{TimerTick, TIMER_TICK};
use plugin_sdk::contracts::screen::ScreeningClient;
use plugin_sdk::contracts::source::{ItemQueryClient, ITEM_QUERY_SERVICE};
use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::contracts::{format_rfc3339, BoxFuture};
use plugin_sdk::{Context, Plugin, PluginError};
use serde::Deserialize;
use time::{Date, Duration, OffsetDateTime, UtcOffset};
use tokio::sync::Mutex;

const BATCH: i64 = 1000;
const TICK_NAME: &str = "minute";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    backup_keep: usize,
    archive_days: i64,
    run_at: String,
    utc_offset: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backup_keep: 7,
            archive_days: 90,
            run_at: "04:00".into(),
            utc_offset: "+08:00".into(),
        }
    }
}

fn parse_offset(s: &str) -> Result<UtcOffset, PluginError> {
    let bad = || PluginError::new(format!("utc_offset 格式错误：{s}，应形如 +08:00"));
    let b = s.as_bytes();
    if b.len() != 6 || !(b[0] == b'+' || b[0] == b'-') || b[3] != b':' {
        return Err(bad());
    }
    let h: i8 = s[1..3].parse().map_err(|_| bad())?;
    let m: i8 = s[4..6].parse().map_err(|_| bad())?;
    if m > 59 {
        return Err(bad());
    }
    let sign = if b[0] == b'-' { -1 } else { 1 };
    UtcOffset::from_hms(sign * h, sign * m, 0).map_err(|_| bad())
}

fn parse_hhmm(s: &str) -> Result<(u8, u8), PluginError> {
    let bad = || PluginError::new(format!("run_at 格式错误：{s}，应形如 04:00"));
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return Err(bad());
    }
    let h: u8 = s[0..2].parse().map_err(|_| bad())?;
    let m: u8 = s[3..5].parse().map_err(|_| bad())?;
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok((h, m))
}

/// 备份文件名 `app-YYYYMMDD.db`。
pub fn backup_file_name(date: Date) -> String {
    format!(
        "app-{:04}{:02}{:02}.db",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn is_backup_name(name: &str) -> bool {
    name.len() == 15
        && name.starts_with("app-")
        && name.ends_with(".db")
        && name[4..12].bytes().all(|b| b.is_ascii_digit())
}

/// 今天是否该做：本地时间已过 `run_at`，且内存里没记过今天、目录里也没有今天的备份文件。
pub fn should_run(
    now_local: OffsetDateTime,
    run_at: (u8, u8),
    last_done: Option<Date>,
    today_file_exists: bool,
) -> bool {
    let today = now_local.date();
    if last_done == Some(today) || today_file_exists {
        return false;
    }
    (now_local.hour(), now_local.minute()) >= run_at
}

/// 需要删除的旧备份：只看符合命名的文件，保留日期最新的 `keep` 份，其余返回。
pub fn backups_to_delete(names: &[String], keep: usize) -> Vec<String> {
    let mut v: Vec<&String> = names.iter().filter(|n| is_backup_name(n)).collect();
    v.sort();
    let cut = v.len().saturating_sub(keep);
    v[..cut].iter().map(|s| (*s).clone()).collect()
}

struct Maintenance {
    store: StoreClient,
    screening: ScreeningClient,
    items: ItemQueryClient,
    backup_dir: PathBuf,
    keep: usize,
    archive_days: i64,
    run_at: (u8, u8),
    offset: UtcOffset,
    last_done: Mutex<Option<Date>>,
}

impl Maintenance {
    /// 检查并在需要时执行一次；返回是否执行了。
    async fn check(&self, now: OffsetDateTime) -> bool {
        // 持锁贯穿整个执行，避免并发 tick 重复做
        let mut last = self.last_done.lock().await;
        let local = now.to_offset(self.offset);
        let exists = self
            .backup_dir
            .join(backup_file_name(local.date()))
            .exists();
        if !should_run(local, self.run_at, *last, exists) {
            if exists {
                *last = Some(local.date());
            }
            return false;
        }
        *last = Some(local.date());
        self.backup(local.date()).await;
        self.cleanup(now).await;
        true
    }

    async fn backup(&self, date: Date) {
        if let Err(e) = std::fs::create_dir_all(&self.backup_dir) {
            tracing::error!("创建备份目录失败：{e}");
            return;
        }
        let path = self.backup_dir.join(backup_file_name(date));
        match self.store.backup(&path.to_string_lossy()).await {
            Ok(bytes) => tracing::info!("数据库已备份，{bytes} 字节"),
            Err(e) => {
                tracing::error!("数据库备份失败：{e}");
                return;
            }
        }
        rotate(&self.backup_dir, self.keep);
    }

    async fn cleanup(&self, now: OffsetDateTime) {
        let before = format_rfc3339(now - Duration::days(self.archive_days));
        let mut total = 0u64;
        let mut prev_first: Option<i64> = None;
        loop {
            let ids = match self.screening.archived_before(&before, BATCH).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("查询归档消息失败：{e}");
                    break;
                }
            };
            if ids.is_empty() {
                break;
            }
            // 同一批连续出现说明上一批没删掉，停止以免死循环
            if prev_first == Some(ids[0]) {
                tracing::error!("归档清理没有进展，停止");
                break;
            }
            prev_first = Some(ids[0]);
            if let Err(e) = self.items.delete(&ids).await {
                tracing::error!("删除归档消息失败：{e}");
                break;
            }
            match self.screening.delete(&ids).await {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::error!("删除初筛记录失败：{e}");
                    break;
                }
            }
        }
        tracing::info!("归档清理完成，删除初筛记录 {total} 条");
    }
}

fn rotate(dir: &Path, keep: usize) {
    let names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(e) => {
            tracing::error!("读取备份目录失败：{e}");
            return;
        }
    };
    for n in backups_to_delete(&names, keep) {
        if let Err(e) = std::fs::remove_file(dir.join(&n)) {
            tracing::error!("删除旧备份 {n} 失败：{e}");
        }
    }
}

#[derive(Default)]
pub struct MaintenancePlugin;

impl Plugin for MaintenancePlugin {
    fn manifest(&self) -> &str {
        include_str!("../plugin.toml")
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = if ctx.config().is_null() {
                Config::default()
            } else {
                ctx.config_as()?
            };
            if cfg.backup_keep == 0 {
                return Err(PluginError::new("backup_keep 不能为 0"));
            }
            if cfg.archive_days < 1 {
                return Err(PluginError::new("archive_days 不能小于 1"));
            }
            let m = Arc::new(Maintenance {
                store: ctx.store(),
                screening: ScreeningClient(ctx.service(plugin_sdk::contracts::screen::SERVICE)),
                items: ItemQueryClient(ctx.service(ITEM_QUERY_SERVICE)),
                backup_dir: ctx.data_dir().join("backups"),
                keep: cfg.backup_keep,
                archive_days: cfg.archive_days,
                run_at: parse_hhmm(&cfg.run_at)?,
                offset: parse_offset(&cfg.utc_offset)?,
                last_done: Mutex::new(None),
            });
            let m2 = m.clone();
            ctx.subscribe(TIMER_TICK, move |e| {
                let m = m2.clone();
                async move {
                    let tick: TimerTick = serde_json::from_value(e.payload)?;
                    if tick.name == TICK_NAME {
                        m.check(OffsetDateTime::now_utc()).await;
                    }
                    Ok(())
                }
            })?;
            ctx.spawn("maintenance-startup", async move {
                m.check(OffsetDateTime::now_utc()).await;
                Ok::<(), PluginError>(())
            });
            tracing::info!("maintenance 已启动");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::{parse_rfc3339, ServiceError};
    use plugin_sdk::testing::{temp_dir, test_http_client, TestHub};
    use plugin_sdk::Manifest;
    use serde_json::{json, Value};
    use std::sync::Mutex as StdMutex;

    fn utc(s: &str) -> OffsetDateTime {
        parse_rfc3339(s).unwrap()
    }

    fn off8() -> UtcOffset {
        parse_offset("+08:00").unwrap()
    }

    fn local(s: &str) -> OffsetDateTime {
        utc(s).to_offset(off8())
    }

    #[test]
    fn manifest_parses() {
        let m = Manifest::parse(MaintenancePlugin.manifest()).unwrap();
        assert_eq!(m.name, "maintenance");
    }

    #[test]
    fn file_name_format() {
        let d = local("2024-03-05T02:00:00Z").date();
        assert_eq!(backup_file_name(d), "app-20240305.db");
    }

    #[test]
    fn should_run_cases() {
        let run_at = (4, 0);
        // 本地 03:59 还没到
        assert!(!should_run(
            local("2024-01-01T19:59:00Z"),
            run_at,
            None,
            false
        ));
        // 本地 04:00 到点
        let now = local("2024-01-01T20:00:00Z");
        assert_eq!(now.date().day(), 2);
        assert!(should_run(now, run_at, None, false));
        // 已做记录
        assert!(!should_run(now, run_at, Some(now.date()), false));
        // 昨天做的，今天仍该做
        let yesterday = now.date().previous_day();
        assert!(should_run(now, run_at, yesterday, false));
        // 目录里已有今天的文件
        assert!(!should_run(now, run_at, None, true));
        // 晚些时候启动也补做
        assert!(should_run(
            local("2024-01-02T10:00:00Z"),
            run_at,
            None,
            false
        ));
    }

    #[test]
    fn rotation_keeps_newest_and_ignores_others() {
        let mut names: Vec<String> = (1..=9).map(|d| format!("app-2024010{d}.db")).collect();
        names.push("notes.txt".into());
        names.push("app-2024.db".into());
        names.push("app-20240101.db.bak".into());
        let del = backups_to_delete(&names, 7);
        assert_eq!(del, vec!["app-20240101.db", "app-20240102.db"]);
        assert!(backups_to_delete(&names, 20).is_empty());
    }

    #[test]
    fn rotate_on_disk() {
        let dir = temp_dir("maint-rotate");
        for d in 1..=9 {
            std::fs::write(dir.join(format!("app-2024010{d}.db")), b"x").unwrap();
        }
        std::fs::write(dir.join("other.txt"), b"x").unwrap();
        rotate(&dir, 7);
        assert!(!dir.join("app-20240101.db").exists());
        assert!(!dir.join("app-20240102.db").exists());
        assert!(dir.join("app-20240103.db").exists());
        assert!(dir.join("other.txt").exists());
    }

    type Log = Arc<StdMutex<Vec<String>>>;

    struct Fixture {
        _hub: Arc<TestHub>,
        m: Maintenance,
        log: Log,
        dir: PathBuf,
    }

    /// `batches`：archived_before 依次返回的批次；`fail`：让指定调用失败
    /// （"backup" / "items.delete" / "screen.delete"）。
    fn fixture(name: &str, batches: Vec<Vec<i64>>, fail: &'static str) -> Fixture {
        let hub = TestHub::new();
        let log: Log = Arc::default();
        let dir = temp_dir(name).join("backups");
        let queue = Arc::new(StdMutex::new(batches));

        let l = log.clone();
        hub.register_fn("store", "store", move |method, params| {
            let l = l.clone();
            async move {
                assert_eq!(method, "backup");
                if fail == "backup" {
                    return Err(ServiceError::unavailable("x"));
                }
                let path = params["path"].as_str().unwrap().to_string();
                std::fs::write(&path, b"db").unwrap();
                l.lock().unwrap().push(format!("backup {path}"));
                Ok(json!({ "bytes": 2 }))
            }
        });
        let l = log.clone();
        hub.register_fn("collect", "item-query", move |method, params| {
            let l = l.clone();
            async move {
                assert_eq!(method, "delete");
                if fail == "items.delete" {
                    return Err(ServiceError::unavailable("x"));
                }
                l.lock()
                    .unwrap()
                    .push(format!("items.delete {}", params["ids"]));
                Ok(json!({ "deleted": params["ids"].as_array().unwrap().len() }))
            }
        });
        let l = log.clone();
        hub.register_fn("screen", "screening", move |method, params| {
            let l = l.clone();
            let queue = queue.clone();
            async move {
                match method.as_str() {
                    "archived_before" => {
                        l.lock().unwrap().push(format!(
                            "archived_before {} {}",
                            params["before"].as_str().unwrap(),
                            params["limit"]
                        ));
                        let mut q = queue.lock().unwrap();
                        let ids = if q.is_empty() { vec![] } else { q.remove(0) };
                        Ok(json!({ "ids": ids }))
                    }
                    "delete" => {
                        if fail == "screen.delete" {
                            return Err(ServiceError::unavailable("x"));
                        }
                        l.lock()
                            .unwrap()
                            .push(format!("screen.delete {}", params["ids"]));
                        Ok(json!({ "deleted": params["ids"].as_array().unwrap().len() }))
                    }
                    other => panic!("意外方法 {other}"),
                }
            }
        });

        let ctx = hub.context("maintenance", Value::Null, test_http_client());
        let m = Maintenance {
            store: ctx.store(),
            screening: ScreeningClient(ctx.service("screening")),
            items: ItemQueryClient(ctx.service("item-query")),
            backup_dir: dir.clone(),
            keep: 7,
            archive_days: 90,
            run_at: (4, 0),
            offset: off8(),
            last_done: Mutex::new(None),
        };
        Fixture {
            _hub: hub,
            m,
            log,
            dir,
        }
    }

    #[tokio::test]
    async fn full_run_backup_and_cleanup_in_order() {
        let f = fixture("maint-full", vec![vec![1, 2], vec![3]], "");
        // 本地 2024-01-02 05:00
        let now = utc("2024-01-01T21:00:00Z");
        assert!(f.m.check(now).await);
        let log = f.log.lock().unwrap().clone();
        assert!(log[0].starts_with("backup "));
        assert!(log[0].ends_with("app-20240102.db"));
        assert!(f.dir.join("app-20240102.db").exists());
        // before = now − 90 天
        assert_eq!(log[1], "archived_before 2023-10-03T21:00:00Z 1000");
        assert_eq!(log[2], "items.delete [1,2]");
        assert_eq!(log[3], "screen.delete [1,2]");
        assert_eq!(log[4], "archived_before 2023-10-03T21:00:00Z 1000");
        assert_eq!(log[5], "items.delete [3]");
        assert_eq!(log[6], "screen.delete [3]");
        assert_eq!(log[7], "archived_before 2023-10-03T21:00:00Z 1000");
        assert_eq!(log.len(), 8);
        // 同一天不再做
        assert!(!f.m.check(now + Duration::minutes(1)).await);
    }

    #[tokio::test]
    async fn existing_today_file_counts_as_done() {
        let f = fixture("maint-exist", vec![vec![1]], "");
        std::fs::create_dir_all(&f.dir).unwrap();
        std::fs::write(f.dir.join("app-20240102.db"), b"x").unwrap();
        assert!(!f.m.check(utc("2024-01-01T21:00:00Z")).await);
        assert!(f.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn backup_failure_does_not_stop_cleanup() {
        let f = fixture("maint-bf", vec![vec![1]], "backup");
        assert!(f.m.check(utc("2024-01-01T21:00:00Z")).await);
        let log = f.log.lock().unwrap().clone();
        assert!(log.iter().any(|l| l == "screen.delete [1]"));
    }

    #[tokio::test]
    async fn delete_failure_is_logged_not_panicking() {
        let f = fixture("maint-df", vec![vec![1], vec![2]], "items.delete");
        assert!(f.m.check(utc("2024-01-01T21:00:00Z")).await);
        // item-query.delete 失败：不再调 screening.delete
        assert!(!f
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("screen.delete")));
        let f = fixture("maint-sf", vec![vec![1], vec![2]], "screen.delete");
        assert!(f.m.check(utc("2024-01-01T21:00:00Z")).await);
    }

    #[tokio::test]
    async fn before_run_time_does_nothing() {
        let f = fixture("maint-early", vec![vec![1]], "");
        // 本地 03:00
        assert!(!f.m.check(utc("2024-01-01T19:00:00Z")).await);
        assert!(f.log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plugin_start_subscribes_to_minute_tick() {
        let hub = TestHub::new();
        let ctx = hub.context("maintenance", Value::Null, test_http_client());
        MaintenancePlugin.start(ctx).await.unwrap();
        hub.deliver(
            TIMER_TICK,
            json!({"name": "other", "at": "2024-01-01T00:00:00Z"}),
        )
        .await
        .unwrap();
        hub.stop().await;
    }
}

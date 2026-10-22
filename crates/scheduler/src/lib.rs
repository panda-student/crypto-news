//! 定时器插件：按配置发出 `timer.tick`（ephemeral）事件。
//!
//! 电脑休眠导致错过的触发不补发（补发由 dispatch 负责）。

use std::collections::BTreeMap;
use std::time::Duration;

use plugin_sdk::contracts::events::{TimerTick, TIMER_TICK};
use plugin_sdk::contracts::{format_rfc3339, BoxFuture};
use plugin_sdk::{Context, Plugin, PluginError};
use serde::Deserialize;
use time::{OffsetDateTime, Time, UtcOffset};

const DEFAULT_OFFSET: &str = "+08:00";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TimerCfg {
    name: String,
    #[serde(default)]
    every_secs: Option<u64>,
    #[serde(default)]
    daily: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    utc_offset: Option<String>,
    #[serde(default)]
    timers: Vec<TimerCfg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Schedule {
    Every(u64),
    Daily { hour: u8, minute: u8 },
}

/// 解析 `+08:00` / `-05:30` 形式的偏移。
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

/// 解析 `HH:MM`。
fn parse_hhmm(s: &str) -> Result<(u8, u8), PluginError> {
    let bad = || PluginError::new(format!("daily 格式错误：{s}，应形如 08:00"));
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

/// 下一次触发时刻（UTC）：严格晚于 `now` 的、`offset` 本地时间 `hour:minute` 对应的时刻。
/// 恰好等于 `now` 时取次日，避免触发后立刻重复触发。
pub fn next_daily(now: OffsetDateTime, offset: UtcOffset, hour: u8, minute: u8) -> OffsetDateTime {
    let local = now.to_offset(offset);
    let t = Time::from_hms(hour, minute, 0).expect("hour/minute 已校验");
    let today = local.replace_time(t);
    let next = if today > local {
        today
    } else {
        // 日期溢出（9999 年末）时退回 today，实际不会出现
        local
            .date()
            .next_day()
            .map(|d| local.replace_date(d).replace_time(t))
            .unwrap_or(today)
    };
    next.to_offset(UtcOffset::UTC)
}

fn default_timers() -> Vec<TimerCfg> {
    vec![
        TimerCfg {
            name: "minute".into(),
            every_secs: Some(60),
            daily: None,
        },
        TimerCfg {
            name: "daily-report".into(),
            every_secs: None,
            daily: Some("08:00".into()),
        },
    ]
}

fn resolve(cfg: Config) -> Result<(UtcOffset, Vec<(String, Schedule)>), PluginError> {
    let offset = parse_offset(cfg.utc_offset.as_deref().unwrap_or(DEFAULT_OFFSET))?;
    let mut merged: BTreeMap<String, TimerCfg> = default_timers()
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    for t in cfg.timers {
        merged.insert(t.name.clone(), t);
    }
    let mut out = Vec::new();
    for (name, t) in merged {
        if name.is_empty() {
            return Err(PluginError::new("定时器名不能为空"));
        }
        let sched = match (t.every_secs, t.daily.as_deref()) {
            (Some(0), _) => {
                return Err(PluginError::new(format!(
                    "定时器 {name}：every_secs 不能为 0"
                )))
            }
            (Some(n), None) => Schedule::Every(n),
            (None, Some(d)) => {
                let (hour, minute) = parse_hhmm(d)?;
                Schedule::Daily { hour, minute }
            }
            _ => {
                return Err(PluginError::new(format!(
                    "定时器 {name}：every_secs 与 daily 必须二选一"
                )))
            }
        };
        out.push((name, sched));
    }
    Ok((offset, out))
}

async fn emit(ctx: &Context, name: &str) -> Result<(), PluginError> {
    let tick = TimerTick {
        name: name.to_string(),
        at: format_rfc3339(OffsetDateTime::now_utc()),
    };
    ctx.publish(TIMER_TICK, &tick).await?;
    Ok(())
}

async fn run_every(ctx: Context, name: String, secs: u64) -> Result<(), PluginError> {
    let period = Duration::from_secs(secs);
    let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    // 错过不补发
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        iv.tick().await;
        emit(&ctx, &name).await?;
    }
}

async fn run_daily(
    ctx: Context,
    name: String,
    offset: UtcOffset,
    hour: u8,
    minute: u8,
) -> Result<(), PluginError> {
    loop {
        let now = OffsetDateTime::now_utc();
        let next = next_daily(now, offset, hour, minute);
        let wait = (next - now).max(time::Duration::ZERO);
        tokio::time::sleep(Duration::from_secs_f64(wait.as_seconds_f64())).await;
        emit(&ctx, &name).await?;
        // 防止提前醒来导致同一时刻重复触发
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
}

#[derive(Default)]
pub struct SchedulerPlugin;

impl Plugin for SchedulerPlugin {
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
            let (offset, timers) = resolve(cfg)?;
            for (name, sched) in timers {
                let c = ctx.clone();
                let task = format!("timer-{name}");
                match sched {
                    Schedule::Every(secs) => ctx.spawn(&task, run_every(c, name, secs)),
                    Schedule::Daily { hour, minute } => {
                        ctx.spawn(&task, run_daily(c, name, offset, hour, minute))
                    }
                }
            }
            tracing::info!("scheduler 已启动");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::parse_rfc3339;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use plugin_sdk::Manifest;
    use serde_json::json;

    fn utc(s: &str) -> OffsetDateTime {
        parse_rfc3339(s).unwrap()
    }

    async fn advance_by(step: Duration, n: u32) {
        for _ in 0..n {
            tokio::time::advance(step).await;
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
        }
    }

    fn off8() -> UtcOffset {
        parse_offset("+08:00").unwrap()
    }

    #[test]
    fn daily_later_today() {
        // 本地 2024-01-01 07:00，当天 08:00 本地 = 00:00 UTC
        let now = utc("2023-12-31T23:00:00Z");
        assert_eq!(next_daily(now, off8(), 8, 0), utc("2024-01-01T00:00:00Z"));
    }

    #[test]
    fn daily_next_day() {
        let now = utc("2024-01-01T01:00:00Z");
        assert_eq!(next_daily(now, off8(), 8, 0), utc("2024-01-02T00:00:00Z"));
    }

    #[test]
    fn daily_exactly_equal_goes_next_day() {
        let now = utc("2024-01-01T00:00:00Z");
        assert_eq!(next_daily(now, off8(), 8, 0), utc("2024-01-02T00:00:00Z"));
    }

    #[test]
    fn daily_crosses_day_and_negative_offset() {
        let o = parse_offset("-05:00").unwrap();
        // 本地 3/9 22:00 -> 本地 23:30 = 次日 UTC 04:30
        assert_eq!(
            next_daily(utc("2024-03-10T03:00:00Z"), o, 23, 30),
            utc("2024-03-10T04:30:00Z")
        );
        // 本地 1/31 00:00 -> 2/1 本地 00:00
        assert_eq!(
            next_daily(utc("2024-01-31T05:00:00Z"), o, 0, 0),
            utc("2024-02-01T05:00:00Z")
        );
    }

    #[test]
    fn parse_errors() {
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("8:00").is_err());
        assert!(parse_hhmm("08:60").is_err());
        assert_eq!(parse_hhmm("08:05").unwrap(), (8, 5));
        assert!(parse_offset("0800").is_err());
        assert!(parse_offset("+08:00").is_ok());
    }

    #[test]
    fn manifest_parses() {
        let m = Manifest::parse(SchedulerPlugin.manifest()).unwrap();
        assert_eq!(m.name, "scheduler");
        assert_eq!(m.events.ephemeral, vec!["timer.tick".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn every_secs_emits() {
        let hub = TestHub::new();
        let ctx = hub.context(
            "scheduler",
            json!({"timers": [{"name": "fast", "every_secs": 5}]}),
            test_http_client(),
        );
        SchedulerPlugin.start(ctx).await.unwrap();
        tokio::task::yield_now().await;
        advance_by(Duration::from_secs(1), 16).await;
        let ticks = hub.published(TIMER_TICK);
        let fast = ticks.iter().filter(|e| e.payload["name"] == "fast").count();
        assert_eq!(fast, 3);
        assert!(ticks[0].payload["at"].as_str().unwrap().ends_with('Z'));
        hub.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn same_name_overrides_default() {
        let hub = TestHub::new();
        let ctx = hub.context(
            "scheduler",
            json!({"timers": [{"name": "minute", "every_secs": 10}]}),
            test_http_client(),
        );
        SchedulerPlugin.start(ctx).await.unwrap();
        tokio::task::yield_now().await;
        advance_by(Duration::from_secs(1), 21).await;
        let n = hub
            .published(TIMER_TICK)
            .iter()
            .filter(|e| e.payload["name"] == "minute")
            .count();
        assert_eq!(n, 2);
        hub.stop().await;
    }

    #[tokio::test]
    async fn bad_config_fails_start() {
        for cfg in [
            json!({"timers": [{"name": "a", "every_secs": 0}]}),
            json!({"timers": [{"name": "a", "daily": "25:00"}]}),
            json!({"timers": [{"name": "a"}]}),
            json!({"timers": [{"name": "a", "every_secs": 1, "daily": "08:00"}]}),
            json!({"utc_offset": "abc"}),
        ] {
            let hub = TestHub::new();
            let ctx = hub.context("scheduler", cfg, test_http_client());
            assert!(SchedulerPlugin.start(ctx).await.is_err());
        }
    }
}

//! 时间工具：所有时间以 UTC 记录。

use std::time::{SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 毫秒时间戳转 RFC 3339（UTC）。
pub fn format_ms(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("invalid-ts({ms})"))
}

pub fn now_rfc3339() -> String {
    format_ms(now_ms())
}

/// 把秒数写成「X 小时 Y 分 Z 秒」。
pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        format!("{h} 小时 {m} 分 {s} 秒")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_epoch() {
        assert_eq!(format_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_ms(1_500), "1970-01-01T00:00:01.5Z");
    }

    #[test]
    fn human() {
        assert_eq!(human_duration(5), "5 秒");
        assert_eq!(human_duration(65), "1 分 5 秒");
        assert_eq!(human_duration(3_725), "1 小时 2 分 5 秒");
        assert_eq!(human_duration(-3), "0 秒");
    }
}

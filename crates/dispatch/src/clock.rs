//! 时区、免打扰、日报时段判断。纯函数，时间一律作为参数传入。

use time::{Duration, OffsetDateTime, Time, UtcOffset};

/// 解析 `+08:00` / `-05:30` 形式的偏移。
pub fn parse_offset(s: &str) -> Result<UtcOffset, String> {
    let bad = || format!("utc_offset 格式错误：{s}，应形如 +08:00");
    let s = s.trim();
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
pub fn parse_hhmm(s: &str) -> Result<(u8, u8), String> {
    let bad = || format!("时间格式错误：{s}，应形如 08:00");
    let s = s.trim();
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

/// 免打扰时段，本地时间，可以跨午夜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: (u8, u8),
    pub end: (u8, u8),
}

/// 解析 `23:00-07:00`；空串表示不启用。起止相同也视为不启用。
pub fn parse_quiet(s: &str) -> Result<Option<QuietHours>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| format!("quiet_hours 格式错误：{s}，应形如 23:00-07:00"))?;
    let start = parse_hhmm(a)?;
    let end = parse_hhmm(b)?;
    if start == end {
        return Ok(None);
    }
    Ok(Some(QuietHours { start, end }))
}

fn minutes((h, m): (u8, u8)) -> u32 {
    h as u32 * 60 + m as u32
}

fn local_minutes(local: OffsetDateTime) -> u32 {
    local.hour() as u32 * 60 + local.minute() as u32
}

/// 现在处于免打扰时段时，返回时段结束的时刻（UTC）；否则返回 None。
pub fn quiet_end(now: OffsetDateTime, offset: UtcOffset, q: &QuietHours) -> Option<OffsetDateTime> {
    let local = now.to_offset(offset);
    let cur = local_minutes(local);
    let (s, e) = (minutes(q.start), minutes(q.end));
    let inside = if s < e {
        cur >= s && cur < e
    } else {
        cur >= s || cur < e
    };
    if !inside {
        return None;
    }
    let t = Time::from_hms(q.end.0, q.end.1, 0).ok()?;
    let mut end = local.replace_time(t);
    if end <= local {
        end += Duration::days(1);
    }
    Some(end.to_offset(UtcOffset::UTC))
}

/// 本地时间是否已过今天的日报时刻（含恰好到点）。
pub fn past_digest_time(now: OffsetDateTime, offset: UtcOffset, slot: (u8, u8)) -> bool {
    local_minutes(now.to_offset(offset)) >= minutes(slot)
}

/// 本地日期 `YYYY-MM-DD`。
pub fn local_date(now: OffsetDateTime, offset: UtcOffset) -> String {
    now.to_offset(offset).date().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::parse_rfc3339;

    fn t(s: &str) -> OffsetDateTime {
        parse_rfc3339(s).unwrap()
    }
    fn cn() -> UtcOffset {
        parse_offset("+08:00").unwrap()
    }

    #[test]
    fn offset_parsing() {
        assert_eq!(
            parse_offset("+08:00").unwrap(),
            UtcOffset::from_hms(8, 0, 0).unwrap()
        );
        assert_eq!(
            parse_offset("-05:30").unwrap(),
            UtcOffset::from_hms(-5, -30, 0).unwrap()
        );
        assert!(parse_offset("8:00").is_err());
        assert!(parse_offset("+08:60").is_err());
        assert!(parse_offset("+8").is_err());
    }

    #[test]
    fn quiet_parsing() {
        assert_eq!(parse_quiet("").unwrap(), None);
        assert_eq!(parse_quiet("07:00-07:00").unwrap(), None);
        let q = parse_quiet("23:00-07:00").unwrap().unwrap();
        assert_eq!((q.start, q.end), ((23, 0), (7, 0)));
        assert!(parse_quiet("23:00").is_err());
        assert!(parse_quiet("25:00-07:00").is_err());
    }

    #[test]
    fn quiet_across_midnight() {
        let q = parse_quiet("23:00-07:00").unwrap().unwrap();
        // 本地 23:30（UTC 15:30）：结束于次日本地 07:00 = UTC 23:00
        assert_eq!(
            quiet_end(t("2026-10-16T15:30:00Z"), cn(), &q),
            Some(t("2026-10-16T23:00:00Z"))
        );
        // 本地 02:00（UTC 前一天 18:00）：结束于当天本地 07:00 = UTC 前一天 23:00
        assert_eq!(
            quiet_end(t("2026-10-15T18:00:00Z"), cn(), &q),
            Some(t("2026-10-15T23:00:00Z"))
        );
        // 本地 07:00 整点已经结束
        assert_eq!(quiet_end(t("2026-10-15T23:00:00Z"), cn(), &q), None);
        // 本地 12:00
        assert_eq!(quiet_end(t("2026-10-16T04:00:00Z"), cn(), &q), None);
        // 本地 23:00 整点开始
        assert!(quiet_end(t("2026-10-16T15:00:00Z"), cn(), &q).is_some());
    }

    #[test]
    fn quiet_same_day() {
        let q = parse_quiet("13:00-14:30").unwrap().unwrap();
        assert_eq!(
            quiet_end(t("2026-10-16T05:10:00Z"), cn(), &q),
            Some(t("2026-10-16T06:30:00Z"))
        );
        assert_eq!(quiet_end(t("2026-10-16T06:30:00Z"), cn(), &q), None);
        assert_eq!(quiet_end(t("2026-10-16T04:59:00Z"), cn(), &q), None);
    }

    #[test]
    fn digest_time_and_date() {
        // 本地 07:59 / 08:00
        assert!(!past_digest_time(t("2026-10-15T23:59:00Z"), cn(), (8, 0)));
        assert!(past_digest_time(t("2026-10-16T00:00:00Z"), cn(), (8, 0)));
        assert!(past_digest_time(t("2026-10-16T10:00:00Z"), cn(), (8, 0)));
        // 本地日期跨 UTC 日期
        assert_eq!(local_date(t("2026-10-15T20:00:00Z"), cn()), "2026-10-16");
        assert_eq!(local_date(t("2026-10-15T10:00:00Z"), cn()), "2026-10-15");
    }
}

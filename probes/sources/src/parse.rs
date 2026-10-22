//! 信源响应解析：纯函数，不访问网络。

use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("RSS/Atom 解析失败: {0}")]
    Feed(String),
    #[error("JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("JSON 结构不符: {0}")]
    Shape(&'static str),
    #[error("时间戳越界: {0}")]
    Time(#[from] time::error::ComponentRange),
}

/// 一个信源解析后的摘要。
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub latest: Option<(OffsetDateTime, String)>,
    /// 去重标识的说明，如 "有 guid/id (2/2)"。
    pub id_note: String,
    /// 用到的字段名。
    pub fields: Vec<String>,
}

fn from_millis(ms: i64) -> Result<OffsetDateTime, ParseError> {
    Ok(OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))?)
}

fn keep_latest(slot: &mut Option<(OffsetDateTime, String)>, t: OffsetDateTime, title: String) {
    if slot.as_ref().is_none_or(|(old, _)| t > *old) {
        *slot = Some((t, title));
    }
}

/// 解析 RSS/Atom。guid 数量通过原文统计，因为 feed-rs 在缺失时会自造 ID。
pub fn parse_feed(body: &[u8]) -> Result<Summary, ParseError> {
    let feed = feed_rs::parser::parse(body).map_err(|e| ParseError::Feed(e.to_string()))?;
    let mut latest = None;
    for e in &feed.entries {
        let title = e
            .title
            .as_ref()
            .map(|t| t.content.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        if let Some(t) = e.published.or(e.updated) {
            keep_latest(
                &mut latest,
                OffsetDateTime::from_unix_timestamp(t.timestamp())?,
                title,
            );
        }
    }
    let text = String::from_utf8_lossy(body);
    let guids = text.matches("<guid").count() + text.matches("<id>").count().saturating_sub(1);
    let n = feed.entries.len();
    Ok(Summary {
        count: n,
        latest,
        id_note: if guids >= n && n > 0 {
            format!("有 guid/id ({guids}/{n})")
        } else {
            format!("guid 不全 ({guids}/{n})，用 link")
        },
        fields: ["title", "link", "guid", "pubDate", "description"]
            .map(String::from)
            .to_vec(),
    })
}

/// OKX `/api/v5/support/announcements`：`data[].details[]`，每条只有 title/url/pTime，没有独立 ID。
pub fn parse_okx(body: &[u8]) -> Result<Summary, ParseError> {
    let v: Value = serde_json::from_slice(body)?;
    if v["code"].as_str() != Some("0") {
        return Err(ParseError::Shape("code 不是 \"0\""));
    }
    let pages = v["data"]
        .as_array()
        .ok_or(ParseError::Shape("缺少 data 数组"))?;
    let mut count = 0;
    let mut latest = None;
    let mut fields = Vec::new();
    for item in pages
        .iter()
        .flat_map(|p| p["details"].as_array().into_iter().flatten())
    {
        count += 1;
        if fields.is_empty() {
            if let Some(o) = item.as_object() {
                fields = o.keys().cloned().collect();
            }
        }
        let ms: i64 = item["pTime"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or(ParseError::Shape("pTime 缺失"))?;
        keep_latest(
            &mut latest,
            from_millis(ms)?,
            item["title"].as_str().unwrap_or_default().to_string(),
        );
    }
    Ok(Summary {
        count,
        latest,
        id_note: "无 ID 字段，用 url 去重".into(),
        fields,
    })
}

/// Binance bapi `article/list/query`：`data.catalogs[].articles[]`，有数字 `id` 和 `code`。
pub fn parse_binance(body: &[u8]) -> Result<Summary, ParseError> {
    let v: Value = serde_json::from_slice(body)?;
    if v["code"].as_str() != Some("000000") {
        return Err(ParseError::Shape("code 不是 \"000000\""));
    }
    let cats = v["data"]["catalogs"]
        .as_array()
        .ok_or(ParseError::Shape("缺少 data.catalogs"))?;
    let mut count = 0;
    let mut ids = 0;
    let mut latest = None;
    let mut fields = Vec::new();
    for a in cats
        .iter()
        .flat_map(|c| c["articles"].as_array().into_iter().flatten())
    {
        count += 1;
        if a["id"].is_number() {
            ids += 1;
        }
        if fields.is_empty() {
            if let Some(o) = a.as_object() {
                fields = o.keys().cloned().collect();
            }
        }
        let ms = a["releaseDate"]
            .as_i64()
            .ok_or(ParseError::Shape("releaseDate 缺失"))?;
        keep_latest(
            &mut latest,
            from_millis(ms)?,
            a["title"].as_str().unwrap_or_default().to_string(),
        );
    }
    Ok(Summary {
        count,
        latest,
        id_note: format!("有数字 id ({ids}/{count})，另有 code"),
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn feed_picks_latest_and_counts_guid() {
        let s = parse_feed(include_bytes!("../fixtures/sample.rss")).unwrap();
        assert_eq!(s.count, 2);
        let (t, title) = s.latest.unwrap();
        assert_eq!(t, datetime!(2026-10-14 14:30:47 UTC));
        assert_eq!(title, "Newest item");
        assert!(s.id_note.contains("2/2"));
    }

    #[test]
    fn okx_parses_ptime() {
        let s = parse_okx(include_bytes!("../fixtures/okx.json")).unwrap();
        assert_eq!(s.count, 2);
        let (t, title) = s.latest.unwrap();
        assert_eq!(t.unix_timestamp(), 1790758418);
        assert_eq!(title, "Newest");
        assert!(s.fields.contains(&"url".to_string()));
    }

    #[test]
    fn binance_parses_ids() {
        let s = parse_binance(include_bytes!("../fixtures/binance.json")).unwrap();
        assert_eq!(s.count, 2);
        let (t, title) = s.latest.unwrap();
        assert_eq!(t.unix_timestamp(), 1791948608);
        assert_eq!(title, "Newest");
        assert!(s.id_note.contains("2/2"));
        assert!(s.fields.contains(&"id".to_string()));
    }

    #[test]
    fn html_is_rejected() {
        assert!(parse_binance(b"<html></html>").is_err());
        assert!(parse_okx(b"{\"code\":\"1\"}").is_err());
    }
}

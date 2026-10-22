//! RSS/Atom 解析：纯函数，不访问网络。

use feed_rs::parser::Builder;
use plugin_sdk::contracts::format_rfc3339;
use plugin_sdk::contracts::source::{truncate_raw, Item};
use serde_json::json;
use time::OffsetDateTime;

const SUMMARY_MAX_CHARS: usize = 500;

/// 解析一个 feed 的响应体。feed-rs 在缺少 guid/id 时会自造 ID，
/// 这里让生成器返回空串，再把空串当作"没有"。
pub fn parse_feed(body: &[u8]) -> Result<Vec<Item>, String> {
    let feed = Builder::new()
        .id_generator(|_, _, _| String::new())
        .build()
        .parse(body)
        .map_err(|e| e.to_string())?;
    Ok(feed
        .entries
        .iter()
        .filter_map(|e| {
            let url = e.links.first().map(|l| l.href.trim().to_string())?;
            if url.is_empty() {
                return None;
            }
            let title = e
                .title
                .as_ref()
                .map(|t| squash(&t.content))
                .unwrap_or_default();
            let external_id = Some(e.id.trim().to_string()).filter(|s| !s.is_empty());
            let summary_src = e
                .summary
                .as_ref()
                .map(|t| t.content.as_str())
                .or_else(|| e.content.as_ref().and_then(|c| c.body.as_deref()));
            let summary = summary_src
                .map(|s| truncate_chars(&strip_html(s), SUMMARY_MAX_CHARS))
                .filter(|s| !s.is_empty());
            let published_at = e
                .published
                .or(e.updated)
                .and_then(|t| OffsetDateTime::from_unix_timestamp(t.timestamp()).ok())
                .map(format_rfc3339);
            let raw = truncate_raw(
                json!({
                    "title": title,
                    "link": url,
                    "guid": external_id,
                    "date": published_at,
                    "summary": summary,
                })
                .to_string(),
            );
            Some(Item {
                external_id,
                url,
                title,
                summary,
                published_at,
                raw,
            })
        })
        .collect())
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 去掉 HTML 标签、解码常见实体、压缩空白。
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    squash(&out)
}

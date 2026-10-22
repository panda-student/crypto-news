//! 解析 X 账号页面的 HTML，纯函数，不碰浏览器和网络。

use plugin_sdk::contracts::source::{truncate_raw, Item};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// 页面上的一条推文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tweet {
    pub status_id: String,
    pub url: String,
    pub text: String,
    /// RFC3339 UTC
    pub published_at: Option<String>,
    pub retweet: bool,
    pub pinned: bool,
}

/// 标题取正文的前多少个字符。
pub const TITLE_CHARS: usize = 120;
/// 第一次抓取最多返回的条数。
pub const FIRST_FETCH_LIMIT: usize = 20;

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("固定选择器有效")
}

/// 元素内的文字，表情图片用它的 alt。
fn text_of(el: ElementRef<'_>) -> String {
    let mut out = String::new();
    for n in el.descendants() {
        match n.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) if e.name() == "img" => {
                if let Some(alt) = e.attr("alt") {
                    out.push_str(alt);
                }
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

/// `/user/status/123456?x=1` 取出 `123456`。
fn status_id_of(href: &str) -> Option<String> {
    let rest = href.split("/status/").nth(1)?;
    let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!id.is_empty()).then_some(id)
}

fn absolute(href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!("https://x.com/{}", href.trim_start_matches('/'))
    }
}

fn to_utc(dt: &str) -> Option<String> {
    let t = OffsetDateTime::parse(dt.trim(), &Rfc3339).ok()?;
    t.to_offset(time::UtcOffset::UTC)
        .replace_nanosecond(0)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

/// 解析页面里的全部推文，保持页面顺序；取不到状态链接的跳过。
pub fn parse_timeline(html: &str) -> Vec<Tweet> {
    let doc = Html::parse_document(html);
    let article = sel(r#"article[data-testid="tweet"]"#);
    let social = sel(r#"[data-testid="socialContext"]"#);
    let body = sel(r#"[data-testid="tweetText"]"#);
    let time_sel = sel("time[datetime]");
    let link = sel(r#"a[href*="/status/"]"#);
    let mut out = Vec::new();
    for a in doc.select(&article) {
        let ctx = a
            .select(&social)
            .map(text_of)
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let pinned = ctx.contains("pinned") || ctx.contains("置顶");
        let retweet = ctx.contains("reposted")
            || ctx.contains("retweeted")
            || ctx.contains("转推")
            || ctx.contains("转发");
        let time_el = a.select(&time_sel).next();
        // 优先用包着时间的链接，其次取第一个状态链接
        let href = time_el
            .and_then(|t| t.parent())
            .and_then(ElementRef::wrap)
            .and_then(|p| p.value().attr("href").map(str::to_string))
            .filter(|h| h.contains("/status/"))
            .or_else(|| {
                a.select(&link)
                    .next()
                    .and_then(|l| l.value().attr("href").map(str::to_string))
            });
        let Some(href) = href else { continue };
        let Some(status_id) = status_id_of(&href) else {
            continue;
        };
        let text = a.select(&body).next().map(text_of).unwrap_or_default();
        let published_at = time_el
            .and_then(|t| t.value().attr("datetime"))
            .and_then(to_utc);
        let path = href.split(['?', '#']).next().unwrap_or(&href);
        out.push(Tweet {
            status_id,
            url: absolute(path),
            text,
            published_at,
            retweet,
            pinned,
        });
    }
    out
}

/// 页面看起来是登录墙（出现登录入口）。
pub fn looks_logged_out(html: &str) -> bool {
    html.contains(r#"data-testid="loginButton""#) || html.contains(r#"href="/i/flow/login""#)
}

/// 页面提示账号不存在或被封禁。
pub fn looks_missing_account(html: &str) -> bool {
    html.contains("This account doesn")
        || html.contains("账号不存在")
        || html.contains("帐号不存在")
}

/// 状态 ID 是纯数字，按（位数，字典序）比较大小。
fn key(id: &str) -> (usize, &str) {
    (id.len(), id)
}

fn title_of(t: &Tweet) -> String {
    let body: String = if t.text.is_empty() {
        "（无文字内容）".to_string()
    } else {
        t.text.chars().take(TITLE_CHARS).collect()
    };
    if t.retweet {
        format!("转推：{body}")
    } else {
        body
    }
}

/// 把推文变成消息：跳过置顶；只留比游标新的；第一次最多 20 条。
/// 返回按状态 ID 从旧到新排列的消息，以及新游标（最新的状态 ID，没有新消息时保持原游标）。
pub fn build_items(tweets: &[Tweet], cursor: Option<&str>) -> (Vec<Item>, Option<String>) {
    let mut fresh: Vec<&Tweet> = tweets
        .iter()
        .filter(|t| !t.pinned)
        .filter(|t| cursor.is_none_or(|c| key(&t.status_id) > key(c)))
        .collect();
    // 新的在前
    fresh.sort_by(|a, b| key(&b.status_id).cmp(&key(&a.status_id)));
    fresh.dedup_by(|a, b| a.status_id == b.status_id);
    if cursor.is_none() {
        fresh.truncate(FIRST_FETCH_LIMIT);
    }
    let new_cursor = fresh
        .first()
        .map(|t| t.status_id.clone())
        .or_else(|| cursor.map(str::to_string));
    let items = fresh
        .iter()
        .rev()
        .map(|t| Item {
            external_id: Some(t.status_id.clone()),
            url: t.url.clone(),
            title: title_of(t),
            summary: Some(t.text.clone()),
            published_at: t.published_at.clone(),
            raw: truncate_raw(
                json!({
                    "status_id": t.status_id,
                    "url": t.url,
                    "text": t.text,
                    "published_at": t.published_at,
                    "retweet": t.retweet,
                })
                .to_string(),
            ),
        })
        .collect();
    (items, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../tests/profile.html");

    #[test]
    fn parses_normal_retweet_and_pinned() {
        let t = parse_timeline(PAGE);
        assert_eq!(t.len(), 4);
        assert!(t[0].pinned && !t[0].retweet);
        assert_eq!(t[0].status_id, "1700000000000000001");
        let n = &t[1];
        assert_eq!(n.status_id, "1700000000000000300");
        assert_eq!(n.url, "https://x.com/binance/status/1700000000000000300");
        assert!(n.text.starts_with("Hello 😀 world"));
        assert!(n.text.ends_with("second line"));
        assert_eq!(n.published_at.as_deref(), Some("2026-10-17T10:00:00Z"));
        assert!(!n.pinned && !n.retweet);
        let r = &t[2];
        assert!(r.retweet && !r.pinned);
        assert_eq!(r.status_id, "1700000000000000200");
        // 带时区偏移的时间转成 UTC
        assert_eq!(r.published_at.as_deref(), Some("2026-10-17T08:00:00Z"));
        // 没有文字的推文
        assert_eq!(t[3].text, "");
    }

    #[test]
    fn items_skip_pinned_prefix_retweet_and_order() {
        let t = parse_timeline(PAGE);
        let (items, cur) = build_items(&t, None);
        assert_eq!(items.len(), 3);
        assert_eq!(cur.as_deref(), Some("1700000000000000300"));
        let ids: Vec<_> = items
            .iter()
            .map(|i| i.external_id.clone().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "1700000000000000100",
                "1700000000000000200",
                "1700000000000000300"
            ]
        );
        assert_eq!(items[1].title, "转推：RT body from someone");
        assert!(items[2].summary.as_deref().unwrap().contains("second line"));
        assert_eq!(items[0].title, "（无文字内容）");
        assert!(items[2].raw.contains("1700000000000000300"));
    }

    #[test]
    fn cursor_filters_older() {
        let t = parse_timeline(PAGE);
        let (items, cur) = build_items(&t, Some("1700000000000000200"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id.as_deref(), Some("1700000000000000300"));
        assert_eq!(cur.as_deref(), Some("1700000000000000300"));
        let (items, cur) = build_items(&t, Some("1700000000000000300"));
        assert!(items.is_empty());
        assert_eq!(cur.as_deref(), Some("1700000000000000300"));
        // 位数更多的游标
        let (items, _) = build_items(&t, Some("99999999999999999999"));
        assert!(items.is_empty());
    }

    #[test]
    fn first_fetch_capped_at_20_and_title_120() {
        let long = "字".repeat(300);
        let tweets: Vec<Tweet> = (0..30)
            .map(|i| Tweet {
                status_id: format!("{}", 1000 + i),
                url: format!("https://x.com/a/status/{}", 1000 + i),
                text: long.clone(),
                published_at: None,
                retweet: false,
                pinned: false,
            })
            .collect();
        let (items, cur) = build_items(&tweets, None);
        assert_eq!(items.len(), 20);
        assert_eq!(cur.as_deref(), Some("1029"));
        assert_eq!(items[0].external_id.as_deref(), Some("1010"));
        assert_eq!(items[0].title.chars().count(), 120);
        assert_eq!(items[0].summary.as_ref().unwrap().chars().count(), 300);
    }

    #[test]
    fn page_signals() {
        assert!(parse_timeline("<html><body>nothing</body></html>").is_empty());
        assert!(looks_logged_out(r#"<a data-testid="loginButton">"#));
        assert!(looks_missing_account(
            "<span>This account doesn't exist</span>"
        ));
        assert!(!looks_logged_out(PAGE));
    }
}

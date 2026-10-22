//! 阶段 0 实测：经代理抓取五个 MVP 信源，输出 Markdown 汇总。一次性工具，不进正式产品。

mod parse;

use parse::{ParseError, Summary};
use serde::Deserialize;
use std::time::{Duration, Instant};
use thiserror::Error;
use time::{macros::format_description, OffsetDateTime};

#[derive(Debug, Error)]
enum AppError {
    #[error("读取配置失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置格式错误: {0}")]
    Config(#[from] toml::de::Error),
    #[error("HTTP 客户端错误: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    proxy: String,
    contact_email: String,
    binance_rounds: u32,
    binance_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: "http://127.0.0.1:7892".into(),
            contact_email: String::new(),
            binance_rounds: 5,
            binance_interval_secs: 10,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Feed,
    Okx,
}

struct Source {
    name: &'static str,
    url: &'static str,
    kind: Kind,
    sec: bool,
}

const BINANCE_URL: &str = "https://www.binance.com/bapi/composite/v1/public/cms/article/list/query?type=1&pageNo=1&pageSize=20&catalogId=48";

const SOURCES: &[Source] = &[
    Source {
        name: "SEC 新闻稿",
        url: "https://www.sec.gov/news/pressreleases.rss",
        kind: Kind::Feed,
        sec: true,
    },
    Source {
        name: "SEC 诉讼公告",
        url: "https://www.sec.gov/enforcement-litigation/litigation-releases/rss",
        kind: Kind::Feed,
        sec: true,
    },
    Source {
        name: "SEC 行政处罚",
        url: "https://www.sec.gov/enforcement-litigation/administrative-proceedings/rss",
        kind: Kind::Feed,
        sec: true,
    },
    Source {
        name: "CFTC 新闻稿",
        url: "https://www.cftc.gov/RSS/RSSGP/rssgp.xml",
        kind: Kind::Feed,
        sec: false,
    },
    Source {
        name: "CFTC 执法新闻稿",
        url: "https://www.cftc.gov/RSS/RSSENF/rssenf.xml",
        kind: Kind::Feed,
        sec: false,
    },
    Source {
        name: "CFTC 讲话与证词",
        url: "https://www.cftc.gov/RSS/RSSST/rssst.xml",
        kind: Kind::Feed,
        sec: false,
    },
    Source {
        name: "美联储 新闻稿",
        url: "https://www.federalreserve.gov/feeds/press_all.xml",
        kind: Kind::Feed,
        sec: false,
    },
    Source {
        name: "美联储 讲话与证词",
        url: "https://www.federalreserve.gov/feeds/speeches_and_testimony.xml",
        kind: Kind::Feed,
        sec: false,
    },
    Source {
        name: "OKX 公告",
        url: "https://www.okx.com/api/v5/support/announcements?page=1",
        kind: Kind::Okx,
        sec: false,
    },
];

struct Fetched {
    status: Option<u16>,
    content_type: String,
    body: Vec<u8>,
    elapsed: Duration,
    error: Option<String>,
}

async fn fetch(client: &reqwest::Client, url: &str, ua: &str) -> Fetched {
    let start = Instant::now();
    let result = async {
        let resp = client.get(url).header("User-Agent", ua).send().await?;
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>((status, ct, body))
    }
    .await;
    let elapsed = start.elapsed();
    match result {
        Ok((status, content_type, body)) => Fetched {
            status: Some(status),
            content_type,
            body,
            elapsed,
            error: None,
        },
        Err(e) => Fetched {
            status: None,
            content_type: String::new(),
            body: Vec::new(),
            elapsed,
            error: Some(e.without_url().to_string()),
        },
    }
}

fn parse_by_kind(kind: Kind, body: &[u8]) -> Result<Summary, ParseError> {
    match kind {
        Kind::Feed => parse::parse_feed(body),
        Kind::Okx => parse::parse_okx(body),
    }
}

fn fmt_time(t: OffsetDateTime) -> String {
    let f = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    t.format(&f).map(|s| format!("{s} UTC")).unwrap_or_default()
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn fail_row(name: &str, status: &str, ms: u128, note: &str) -> String {
    format!(
        "| {} | 否 | {status} | {ms} ms | - | - | - | - | {} |",
        cell(name),
        cell(note.trim())
    )
}

/// 返回表格一行；失败时把原因写进"备注"。
fn row(name: &str, kind: Kind, f: &Fetched) -> String {
    let status = f.status.map_or("-".to_string(), |s| s.to_string());
    let ms = f.elapsed.as_millis();
    if let Some(e) = &f.error {
        return fail_row(name, &status, ms, &format!("请求失败: {e}"));
    }
    if f.status != Some(200) {
        let head = String::from_utf8_lossy(&f.body[..f.body.len().min(2000)]);
        let title = head
            .split("<title>")
            .nth(1)
            .and_then(|s| s.split("</title>").next())
            .unwrap_or("");
        return fail_row(name, &status, ms, &format!("{} {}", f.content_type, title));
    }
    match parse_by_kind(kind, &f.body) {
        Ok(s) => {
            let (t, title) = s
                .latest
                .map_or(("-".into(), "-".into()), |(t, title)| (fmt_time(t), title));
            format!(
                "| {} | 是 | {status} | {ms} ms | {} | {} | {} | {} | |",
                cell(name),
                s.count,
                t,
                cell(&truncate(&title, 60)),
                cell(&s.id_note)
            )
        }
        Err(e) => fail_row(name, &status, ms, &e.to_string()),
    }
}

/// 判断 Binance 响应是否被风控拦截。
fn binance_verdict(f: &Fetched) -> &'static str {
    if f.error.is_some() {
        return "请求失败";
    }
    let looks_html = f.content_type.contains("html") || f.body.first() == Some(&b'<');
    match f.status {
        Some(200) if !looks_html && parse::parse_binance(&f.body).is_ok() => "正常",
        Some(200) if looks_html => "拦截（200 但返回 HTML）",
        Some(403) => "拦截（403）",
        Some(202) => "拦截（202 挑战页）",
        Some(429) => "限流（429）",
        _ if looks_html => "拦截（返回 HTML）",
        _ => "异常",
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), AppError> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml").to_string());
    let cfg: Config = match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("未找到 {path}，使用默认配置");
            Config::default()
        }
        Err(e) => return Err(e.into()),
    };

    // 显式走配置里的代理；不读环境变量，不直连。
    let client = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(&cfg.proxy)?)
        .timeout(Duration::from_secs(30))
        .build()?;
    let ua = "Mozilla/5.0 (compatible; crypto-news-probe/0.1)".to_string();
    let email = cfg.contact_email.trim();
    // SEC 要求 UA 带联系邮箱；没填时照发，如实记录结果。
    let sec_ua = if email.is_empty() {
        ua.clone()
    } else {
        format!("crypto-news-probe/0.1 {email}")
    };

    println!("# 信源实测汇总\n");
    println!("- 运行时间: {}", fmt_time(OffsetDateTime::now_utc()));
    println!("- 代理: {}", cfg.proxy);
    println!(
        "- SEC User-Agent: `{sec_ua}`{}\n",
        if email.is_empty() {
            "（contact_email 未填）"
        } else {
            ""
        }
    );
    println!("| 信源 | 成功 | HTTP | 耗时 | 条目数 | 最新发布时间 | 最新标题 | 去重 ID | 备注 |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");

    for src in SOURCES {
        let f = fetch(&client, src.url, if src.sec { &sec_ua } else { &ua }).await;
        println!("{}", row(src.name, src.kind, &f));
        // SEC 对请求频率敏感，来源之间留间隔。
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    let mut rounds = Vec::new();
    let mut last_summary = None;
    for i in 0..cfg.binance_rounds {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(cfg.binance_interval_secs)).await;
        }
        let f = fetch(&client, BINANCE_URL, &ua).await;
        let verdict = binance_verdict(&f);
        let summary = parse::parse_binance(&f.body).ok();
        let latest = summary
            .as_ref()
            .and_then(|s| s.latest.as_ref().map(|(t, _)| fmt_time(*t)))
            .unwrap_or_default();
        rounds.push(format!(
            "| {} | {} | {} ms | {} | {} | {} |",
            i + 1,
            f.status.map_or("-".to_string(), |s| s.to_string()),
            f.elapsed.as_millis(),
            verdict,
            summary.as_ref().map_or(0, |s| s.count),
            latest
        ));
        if summary.is_some() {
            last_summary = summary;
        }
    }
    println!(
        "\n## Binance 公告（catalogId=48，连续 {} 次，间隔 {} 秒）\n",
        cfg.binance_rounds, cfg.binance_interval_secs
    );
    println!("- 接口: `{BINANCE_URL}`");
    println!("\n| 轮次 | HTTP | 耗时 | 风控判断 | 条目数 | 最新发布时间 |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for r in &rounds {
        println!("{r}");
    }
    if let Some(s) = last_summary {
        println!("\n- 文章字段: `{}`", s.fields.join("`, `"));
        println!("- 去重 ID: {}", s.id_note);
        println!(
            "- 响应结构: `code`, `data.catalogs[].{{catalogId, catalogName, total, articles[]}}`, `success`"
        );
    } else {
        println!("\n- 没有成功解析的轮次，无法列出字段。");
    }
    Ok(())
}

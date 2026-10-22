//! 渲染：快讯、日报、提醒 × markdown / plain，按渠道字节上限分批。纯函数。

use std::ops::Range;

use plugin_sdk::contracts::channel::TextFormat;
use plugin_sdk::contracts::events::PushItem;
use plugin_sdk::contracts::screen::Level;

const PLAIN_SEP: &str = "━━━━━━━━";

/// 日报里的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub level: Level,
    pub title: String,
    pub url: String,
    pub impact: Option<String>,
    pub backfill: bool,
}

/// 快讯的一个批次：正文和它包含的 `items` 下标范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashBatch {
    pub text: String,
    pub items: Range<usize>,
}

/// 把 `s` 截到不超过 `max` 字节，且落在字符边界上。
pub fn truncate_at_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 日报统计：（总数不含归档，重要，关注，其他）。
pub fn digest_stats(entries: &[Entry]) -> (usize, usize, usize, usize) {
    let important = entries
        .iter()
        .filter(|e| matches!(e.level, Level::Urgent | Level::Important))
        .count();
    let normal = entries.iter().filter(|e| e.level == Level::Normal).count();
    let archive = entries.iter().filter(|e| e.level == Level::Archive).count();
    (important + normal, important, normal, archive)
}

/// 分批用的最小单位。
struct Atom {
    /// 不在批次开头时，接在前一个单位后面的分隔
    lead: String,
    body: String,
    /// 影响文本的字节数；0 表示没有可缩的内容
    impact_len: usize,
    /// 按给定的影响文本字节上限重新渲染
    shrink: Option<Box<dyn Fn(usize) -> String>>,
}

impl Atom {
    fn plain(lead: &str, body: String) -> Atom {
        Atom {
            lead: lead.to_string(),
            body,
            impact_len: 0,
            shrink: None,
        }
    }

    fn fit(&self, budget: usize) -> String {
        let Some(f) = &self.shrink else {
            return truncate_at_boundary(&self.body, budget).to_string();
        };
        let mut lim = self.impact_len;
        loop {
            let s = f(lim);
            if s.len() <= budget {
                return s;
            }
            if lim == 0 {
                return truncate_at_boundary(&s, budget).to_string();
            }
            lim = lim.saturating_sub((s.len() - budget).max(1));
        }
    }
}

fn cut_impact(impact: &str, lim: usize) -> String {
    if impact.len() <= lim {
        impact.to_string()
    } else {
        format!("{}…", truncate_at_boundary(impact, lim))
    }
}

/// 一条消息的渲染方式。
#[derive(Clone, Copy)]
enum Style {
    /// markdown 引用块
    MdQuote,
    /// markdown 列表项
    MdList,
    /// 纯文本
    Plain,
}

fn render_entry(
    style: Style,
    number: Option<usize>,
    e: &Entry,
    impact_lim: usize,
    heading: Option<&str>,
) -> String {
    let title = if e.backfill {
        format!("{}（补抓）", e.title)
    } else {
        e.title.clone()
    };
    let impact = e
        .impact
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && impact_lim > 0)
        .map(|s| cut_impact(s, impact_lim));
    let num = number.map(|n| format!("{n}. ")).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    if let Some(h) = heading {
        lines.push(h.to_string());
    }
    match style {
        Style::MdQuote => {
            lines.push(format!("> **{num}{title}**"));
            if let Some(i) = impact {
                lines.push(format!("> 影响：{i}"));
            }
            lines.push(format!("> [原文]({})", e.url));
        }
        Style::MdList => {
            let mut l = format!("- {title} [原文]({})", e.url);
            if let Some(i) = impact {
                l.push_str(&format!(" 影响：{i}"));
            }
            lines.push(l);
        }
        Style::Plain => {
            lines.push(format!("{num}{title}"));
            if let Some(i) = impact {
                lines.push(format!("    影响：{i}"));
            }
            lines.push(format!("🔗 {}", e.url));
        }
    }
    lines.join("\n")
}

fn entry_atom(
    style: Style,
    number: Option<usize>,
    e: &Entry,
    lead: &str,
    heading: Option<String>,
) -> Atom {
    let impact_len = e.impact.as_deref().map(|s| s.trim().len()).unwrap_or(0);
    let body = render_entry(style, number, e, usize::MAX, heading.as_deref());
    let e2 = e.clone();
    Atom {
        lead: lead.to_string(),
        body,
        impact_len,
        shrink: Some(Box::new(move |lim| {
            render_entry(style, number, &e2, lim, heading.as_deref())
        })),
    }
}

/// 组批：`title(None)` 是不分批时的标题，`title(Some((i, n)))` 是分批时的标题。
/// `footer` 含自己的前导分隔，只放在最后一批。返回每批正文和它覆盖的单位下标。
fn pack(
    title: &dyn Fn(Option<(usize, usize)>) -> String,
    sep0: &str,
    atoms: &[Atom],
    footer: &str,
    max: usize,
) -> Vec<(String, Range<usize>)> {
    let mut single = title(None);
    for (i, a) in atoms.iter().enumerate() {
        single.push_str(if i == 0 { sep0 } else { &a.lead });
        single.push_str(&a.body);
    }
    single.push_str(footer);
    if single.len() <= max {
        return vec![(single, 0..atoms.len())];
    }

    let reserve = title(Some((99, 99))).len();
    let mut batches: Vec<(String, Range<usize>)> = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    for (i, a) in atoms.iter().enumerate() {
        let mut lead = if cur.is_empty() { sep0 } else { &a.lead };
        if !cur.is_empty() && reserve + cur.len() + lead.len() + a.body.len() > max {
            batches.push((std::mem::take(&mut cur), start..i));
            start = i;
            lead = sep0;
        }
        let body = if reserve + lead.len() + a.body.len() > max && cur.is_empty() {
            a.fit(max.saturating_sub(reserve + lead.len()))
        } else {
            a.body.clone()
        };
        cur.push_str(lead);
        cur.push_str(&body);
    }
    if !cur.is_empty() || batches.is_empty() {
        batches.push((cur, start..atoms.len()));
    }
    let last = batches.last().map(|(c, _)| c.len()).unwrap_or(0);
    if !footer.is_empty() && reserve + last + footer.len() > max {
        batches.push((String::new(), atoms.len()..atoms.len()));
    }
    let n = batches.len();
    batches
        .into_iter()
        .enumerate()
        .map(|(i, (content, range))| {
            let mut t = title(Some((i + 1, n)));
            t.push_str(&content);
            if i + 1 == n {
                t.push_str(footer);
            }
            (t, range)
        })
        .collect()
}

fn suffix(part: Option<(usize, usize)>) -> String {
    part.map(|(i, n)| format!("（{i}/{n}）"))
        .unwrap_or_default()
}

fn sep0(fmt: TextFormat) -> String {
    match fmt {
        TextFormat::Markdown => "\n\n".to_string(),
        TextFormat::Plain => format!("\n{PLAIN_SEP}\n"),
    }
}

/// 渲染日报，已按 `max` 分批。
pub fn render_digest(
    date: &str,
    entries: &[Entry],
    fmt: TextFormat,
    max: usize,
    renew_hint: &str,
) -> Vec<String> {
    let (total, important, normal, archive) = digest_stats(entries);
    let top: Vec<&Entry> = entries
        .iter()
        .filter(|e| matches!(e.level, Level::Urgent | Level::Important))
        .collect();
    let mid: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.level == Level::Normal)
        .collect();
    let section_lead = sep0(fmt);
    let (quote_style, list_style) = match fmt {
        TextFormat::Markdown => (Style::MdQuote, Style::MdList),
        TextFormat::Plain => (Style::Plain, Style::Plain),
    };
    let head = |emoji_title: &str, n: usize| match fmt {
        TextFormat::Markdown => format!("### {emoji_title}（{n}）"),
        TextFormat::Plain => format!("{emoji_title}（{n}）"),
    };

    let mut atoms: Vec<Atom> = Vec::new();
    for (list, h, style, piece_lead) in [
        (
            &top,
            head("🔴 重要", top.len()),
            quote_style,
            match fmt {
                TextFormat::Markdown => "\n\n",
                TextFormat::Plain => "\n",
            },
        ),
        (&mid, head("🟡 关注", mid.len()), list_style, "\n"),
    ] {
        for (k, e) in list.iter().enumerate() {
            let (lead, heading) = if k == 0 {
                (section_lead.as_str(), Some(h.clone()))
            } else {
                (piece_lead, None)
            };
            // 关注栏的 markdown 列表不编号
            let number = match style {
                Style::MdList => None,
                _ => Some(k + 1),
            };
            atoms.push(entry_atom(style, number, e, lead, heading));
        }
    }
    if archive > 0 {
        atoms.push(Atom::plain(
            &section_lead,
            format!(
                "{}\n另有 {archive} 条低影响消息，已归档不推送。",
                head("🟢 其他", archive)
            ),
        ));
    }
    if atoms.is_empty() {
        atoms.push(Atom::plain(&section_lead, "今日暂无新消息".to_string()));
    }

    let stats = format!("共 {total} 条 · 重要 {important} · 关注 {normal} · 其他 {archive}");
    let footer = match fmt {
        TextFormat::Markdown => format!("\n\n<font color=\"comment\">{stats}</font>"),
        TextFormat::Plain => {
            let mut f = format!("\n{PLAIN_SEP}\n{stats}");
            if !renew_hint.trim().is_empty() {
                f.push('\n');
                f.push_str(renew_hint.trim());
            }
            f
        }
    };
    let title = |part: Option<(usize, usize)>| match fmt {
        TextFormat::Markdown => format!("## 📰 消息面日报 · {date}{}", suffix(part)),
        TextFormat::Plain => format!("【消息面日报 · {date}{}】", suffix(part)),
    };
    pack(&title, &section_lead, &atoms, &footer, max)
        .into_iter()
        .map(|(t, _)| t)
        .collect()
}

/// 渲染快讯，已按 `max` 分批。每个批次带它包含的 `items` 范围。
pub fn render_flash(items: &[PushItem], fmt: TextFormat, max: usize) -> Vec<FlashBatch> {
    let style = match fmt {
        TextFormat::Markdown => Style::MdQuote,
        TextFormat::Plain => Style::Plain,
    };
    let lead = match fmt {
        TextFormat::Markdown => "\n\n",
        TextFormat::Plain => "\n",
    };
    let numbered = items.len() > 1 && fmt == TextFormat::Plain;
    let atoms: Vec<Atom> = items
        .iter()
        .enumerate()
        .map(|(k, it)| {
            let e = Entry {
                level: it.level,
                title: it.title.clone(),
                url: it.url.clone(),
                impact: it.impact.clone(),
                backfill: false,
            };
            entry_atom(style, numbered.then_some(k + 1), &e, lead, None)
        })
        .collect();
    let title = |part: Option<(usize, usize)>| match fmt {
        TextFormat::Markdown => format!("## ⚡ 消息面快讯{}", suffix(part)),
        TextFormat::Plain => format!("【⚡ 消息面快讯{}】", suffix(part)),
    };
    pack(&title, &sep0(fmt), &atoms, "", max)
        .into_iter()
        .map(|(text, items)| FlashBatch { text, items })
        .collect()
}

/// 提醒、测试消息：纯文本一段，超长截断。两种格式都可直接发送。
pub fn render_alert(text: &str, max: usize) -> String {
    truncate_at_boundary(text, max).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: Level, n: usize, impact: Option<&str>, backfill: bool) -> Entry {
        Entry {
            level,
            title: format!("标题{n}"),
            url: format!("https://example.com/{n}"),
            impact: impact.map(str::to_string),
            backfill,
        }
    }

    fn sample() -> Vec<Entry> {
        vec![
            entry(Level::Urgent, 1, Some("短期利好"), false),
            entry(Level::Important, 2, Some("情绪缓和"), true),
            entry(Level::Normal, 3, None, false),
            entry(Level::Normal, 4, Some("有影响"), false),
            entry(Level::Archive, 5, None, false),
            entry(Level::Archive, 6, None, false),
        ]
    }

    #[test]
    fn markdown_digest_structure() {
        let out = render_digest("2026-10-16", &sample(), TextFormat::Markdown, 4000, "提示");
        assert_eq!(out.len(), 1);
        let want = [
            "## 📰 消息面日报 · 2026-10-16",
            "",
            "### 🔴 重要（2）",
            "> **1. 标题1**",
            "> 影响：短期利好",
            "> [原文](https://example.com/1)",
            "",
            "> **2. 标题2（补抓）**",
            "> 影响：情绪缓和",
            "> [原文](https://example.com/2)",
            "",
            "### 🟡 关注（2）",
            "- 标题3 [原文](https://example.com/3)",
            "- 标题4 [原文](https://example.com/4) 影响：有影响",
            "",
            "### 🟢 其他（2）",
            "另有 2 条低影响消息，已归档不推送。",
            "",
            "<font color=\"comment\">共 4 条 · 重要 2 · 关注 2 · 其他 2</font>",
        ]
        .join("\n");
        assert_eq!(out[0], want);
        assert!(!out[0].contains("提示"), "markdown 日报不追加 renew_hint");
    }

    #[test]
    fn plain_digest_structure_and_renew_hint() {
        let out = render_digest("2026-10-16", &sample(), TextFormat::Plain, 2000, "💬 续期");
        assert_eq!(out.len(), 1);
        let want = [
            "【消息面日报 · 2026-10-16】",
            "━━━━━━━━",
            "🔴 重要（2）",
            "1. 标题1",
            "    影响：短期利好",
            "🔗 https://example.com/1",
            "2. 标题2（补抓）",
            "    影响：情绪缓和",
            "🔗 https://example.com/2",
            "━━━━━━━━",
            "🟡 关注（2）",
            "1. 标题3",
            "🔗 https://example.com/3",
            "2. 标题4",
            "    影响：有影响",
            "🔗 https://example.com/4",
            "━━━━━━━━",
            "🟢 其他（2）",
            "另有 2 条低影响消息，已归档不推送。",
            "━━━━━━━━",
            "共 4 条 · 重要 2 · 关注 2 · 其他 2",
            "💬 续期",
        ]
        .join("\n");
        assert_eq!(out[0], want);
    }

    #[test]
    fn empty_digest_says_no_news() {
        for fmt in [TextFormat::Markdown, TextFormat::Plain] {
            let out = render_digest("2026-10-16", &[], fmt, 2000, "续期");
            assert_eq!(out.len(), 1);
            assert!(out[0].contains("今日暂无新消息"));
            assert!(out[0].contains("共 0 条"));
        }
        let plain = render_digest("d", &[], TextFormat::Plain, 2000, "续期");
        assert!(plain[0].ends_with("续期"));
    }

    fn many(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| {
                entry(
                    Level::Important,
                    i,
                    Some("这是一段比较长的影响说明文字"),
                    false,
                )
            })
            .collect()
    }

    #[test]
    fn digest_batches_respect_limit() {
        for fmt in [TextFormat::Markdown, TextFormat::Plain] {
            let mut entries = many(30);
            entries.extend((100..130).map(|i| entry(Level::Normal, i, None, false)));
            let out = render_digest("2026-10-16", &entries, fmt, 600, "续期提示");
            assert!(out.len() > 1);
            let n = out.len();
            for (i, t) in out.iter().enumerate() {
                assert!(t.len() <= 600, "第 {} 批 {} 字节", i + 1, t.len());
                assert!(t.contains(&format!("（{}/{}）", i + 1, n)), "{t}");
                let has_stats = t.contains("共 60 条");
                assert_eq!(has_stats, i + 1 == n, "统计只在最后一批");
            }
            if fmt == TextFormat::Plain {
                assert!(out[n - 1].ends_with("续期提示"));
                assert!(!out[0].contains("续期提示"));
            }
        }
    }

    #[test]
    fn single_batch_has_no_suffix() {
        let out = render_digest("d", &many(1), TextFormat::Plain, 2000, "");
        assert_eq!(out.len(), 1);
        assert!(!out[0].contains("（1/1）"));
    }

    #[test]
    fn oversize_entry_impact_is_truncated() {
        let big = "影".repeat(500);
        let entries = vec![entry(Level::Urgent, 1, Some(&big), false)];
        let out = render_digest("d", &entries, TextFormat::Plain, 400, "");
        assert!(out.iter().all(|t| t.len() <= 400));
        let joined = out.join("\n");
        assert!(joined.contains("标题1"));
        assert!(joined.contains("https://example.com/1"));
        assert!(joined.contains('…'));
    }

    #[test]
    fn footer_overflow_makes_extra_batch() {
        // 条目刚好占满，统计放不下时单独补一批
        let entries = many(3);
        let full = render_digest("d", &entries, TextFormat::Plain, 100_000, "");
        let limit = full[0].len() - 5;
        let out = render_digest("d", &entries, TextFormat::Plain, limit, "");
        assert!(out.iter().all(|t| t.len() <= limit));
        assert!(out.last().unwrap().contains("共 3 条"));
    }

    fn push(n: usize, impact: Option<&str>) -> PushItem {
        PushItem {
            item_id: n as i64,
            level: Level::Urgent,
            title: format!("快讯{n}"),
            impact: impact.map(str::to_string),
            url: format!("https://example.com/{n}"),
            source_id: "s".into(),
            published_at: None,
        }
    }

    #[test]
    fn flash_formats() {
        let md = render_flash(&[push(1, Some("利好"))], TextFormat::Markdown, 4000);
        assert_eq!(md.len(), 1);
        assert_eq!(md[0].items, 0..1);
        assert_eq!(
            md[0].text,
            "## ⚡ 消息面快讯\n\n> **快讯1**\n> 影响：利好\n> [原文](https://example.com/1)"
        );
        let plain = render_flash(
            &[push(1, None), push(2, Some("x"))],
            TextFormat::Plain,
            2000,
        );
        assert_eq!(
            plain[0].text,
            "【⚡ 消息面快讯】\n━━━━━━━━\n1. 快讯1\n🔗 https://example.com/1\n2. 快讯2\n    影响：x\n🔗 https://example.com/2"
        );
    }

    #[test]
    fn flash_batches_cover_items_without_overlap() {
        let items: Vec<PushItem> = (0..12).map(|n| push(n, Some("影响说明影响说明"))).collect();
        let out = render_flash(&items, TextFormat::Plain, 300);
        assert!(out.len() > 1);
        let n = out.len();
        let mut next = 0;
        for (i, b) in out.iter().enumerate() {
            assert!(b.text.len() <= 300);
            assert!(b.text.contains(&format!("（{}/{}）", i + 1, n)));
            assert_eq!(b.items.start, next);
            next = b.items.end;
        }
        assert_eq!(next, 12);
    }

    #[test]
    fn alert_truncates_on_char_boundary() {
        let s = render_alert(&"警".repeat(100), 10);
        assert_eq!(s.len(), 9);
    }
}

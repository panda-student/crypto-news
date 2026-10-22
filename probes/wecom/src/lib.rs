//! 企业微信群机器人消息长度实测：消息构造、分批切分、脱敏。纯函数，不访问网络。

/// 批次头预留字节，容纳 "第 NN/MM 批\n" 的最坏情况。
pub const HEADER_RESERVE: usize = 24;

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

/// 构造恰好 `total` 字节的测试消息：开头写字节数，结尾加 `[END-<字节数>]`，
/// 中间填中文；字节数不是 3 的整数倍时用 0 到 2 个 ASCII 字符补齐。
/// `total` 太小放不下首尾时返回 `None`。
pub fn build_message(total: usize) -> Option<String> {
    let head = format!("[{total}字节]\n");
    let tail = format!("\n[END-{total}]");
    let fixed = head.len() + tail.len();
    if total < fixed {
        return None;
    }
    let fill = total - fixed;
    let mut s = String::with_capacity(total);
    s.push_str(&head);
    for _ in 0..fill / 3 {
        s.push('中');
    }
    for _ in 0..fill % 3 {
        s.push('x');
    }
    s.push_str(&tail);
    debug_assert_eq!(s.len(), total);
    Some(s)
}

/// 按条目边界把 `items` 切成多批，每批不超过 `limit` 字节（含批次头）。
/// 每批开头写 "第 N/M 批"；仅当单条本身超过可用空间时才截断，截在字符边界上。
/// 条目之间用换行连接。空输入返回空列表。
pub fn split_batches(items: &[String], limit: usize) -> Vec<String> {
    let budget = limit.saturating_sub(HEADER_RESERVE);
    let mut bodies: Vec<String> = Vec::new();
    let mut cur = String::new();
    for item in items {
        let item = truncate_at_boundary(item, budget);
        if item.is_empty() {
            continue;
        }
        if !cur.is_empty() && cur.len() + 1 + item.len() > budget {
            bodies.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(item);
    }
    if !cur.is_empty() {
        bodies.push(cur);
    }
    let m = bodies.len();
    bodies
        .into_iter()
        .enumerate()
        .map(|(i, b)| format!("第 {}/{} 批\n{}", i + 1, m, b))
        .collect()
}

/// 结构化推送的 markdown 格式样例（内容全是虚构）。
pub fn sample_markdown() -> String {
    [
        "## 📰 消息面速报 · 格式样例",
        "",
        "### 🔴 重要（2）",
        "> **1. 某交易所宣布上线虚构代币 AAA**",
        "> 影响：短期利好 AAA，可能带动同板块情绪",
        "> [原文](https://example.com)",
        "",
        "> **2. 某稳定币发行方公布虚构储备报告**",
        "> 影响：市场对稳定币兑付的担忧缓和",
        "> [原文](https://example.com)",
        "",
        "### 🟡 关注（3）",
        "- 某公链计划下月升级主网（虚构） [原文](https://example.com)",
        "- 某机构提交虚构现货产品申请 [原文](https://example.com)",
        "- 某项目方公布虚构代币解锁计划 [原文](https://example.com)",
        "",
        "### 🟢 其他（4）",
        "另有 4 条低影响消息，已归档不推送。",
        "",
        "<font color=\"comment\">共 9 条 · 重要 2 · 关注 3 · 其他 4 · 本消息为格式样例，内容均为虚构</font>",
    ]
    .join("\n")
}

/// 从 webhook 地址里取出 key 参数的值。
fn extract_key(url: &str) -> Option<&str> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("key="))
        .filter(|k| !k.is_empty())
}

/// 脱敏：去掉文本里的完整 webhook 地址和其中的 key。
pub fn redact(text: &str, webhook_url: &str) -> String {
    let mut out = text.replace(webhook_url, "<webhook>");
    if let Some(key) = extract_key(webhook_url) {
        out = out.replace(key, "<key>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_char_boundary() {
        let s = "中文abc";
        assert_eq!(truncate_at_boundary(s, 4), "中");
        assert_eq!(truncate_at_boundary(s, 3), "中");
        assert_eq!(truncate_at_boundary(s, 2), "");
        assert_eq!(truncate_at_boundary(s, 100), s);
        assert_eq!(truncate_at_boundary("", 5), "");
    }

    #[test]
    fn build_message_exact_sizes() {
        for n in [4000, 4090, 4096, 4097, 4200, 5000, 4001, 4002] {
            let m = build_message(n).unwrap();
            assert_eq!(m.len(), n);
            assert!(m.starts_with(&format!("[{n}字节]")));
            assert!(m.ends_with(&format!("[END-{n}]")));
        }
        assert!(build_message(5).is_none());
    }

    #[test]
    fn split_empty() {
        assert!(split_batches(&[], 4000).is_empty());
        assert!(split_batches(&[String::new()], 4000).is_empty());
    }

    #[test]
    fn split_single_batch_has_header() {
        let r = split_batches(&["a".into(), "b".into()], 4000);
        assert_eq!(r, vec!["第 1/1 批\na\nb".to_string()]);
    }

    #[test]
    fn split_exactly_at_limit_stays_one_batch() {
        let limit = 100;
        let body = "x".repeat(limit - HEADER_RESERVE);
        let r = split_batches(std::slice::from_ref(&body), limit);
        assert_eq!(r.len(), 1);
        assert!(r[0].len() <= limit);
        assert!(r[0].ends_with(&body));
    }

    #[test]
    fn split_one_over_limit_makes_two_batches() {
        let limit = 100;
        let half = (limit - HEADER_RESERVE) / 2;
        // 两条加连接换行恰好多 1 字节
        let a = "a".repeat(half);
        let b = "b".repeat(limit - HEADER_RESERVE - half);
        let r = split_batches(&[a, b], limit);
        assert_eq!(r.len(), 2);
        assert!(r[0].starts_with("第 1/2 批"));
        assert!(r[1].starts_with("第 2/2 批"));
        assert!(r.iter().all(|x| x.len() <= limit));
    }

    #[test]
    fn split_oversized_item_truncated_on_char_boundary() {
        let limit = 100;
        let item = "中".repeat(100);
        let r = split_batches(&[item], limit);
        assert_eq!(r.len(), 1);
        assert!(r[0].len() <= limit);
        let body = r[0].split_once('\n').unwrap().1;
        assert!(!body.is_empty());
        assert!(body.chars().all(|c| c == '中'));
    }

    #[test]
    fn split_keeps_item_boundaries() {
        let items: Vec<String> = (0..50)
            .map(|i| format!("条目{i}:{}", "文".repeat(20)))
            .collect();
        let r = split_batches(&items, 300);
        assert!(r.len() > 1);
        assert!(r.iter().all(|x| x.len() <= 300));
        let joined: String = r
            .iter()
            .map(|b| b.split_once('\n').unwrap().1.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(joined, items.join("\n"));
    }

    #[test]
    fn redact_removes_url_and_key() {
        let url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123-secret";
        let t = format!("error sending {url}; bad key abc-123-secret here");
        let r = redact(&t, url);
        assert!(!r.contains("abc-123-secret"));
        assert!(!r.contains("webhook/send"));
        assert!(r.contains("<webhook>"));
        assert!(r.contains("<key>"));
    }

    #[test]
    fn sample_markdown_shape() {
        let m = sample_markdown();
        assert!(m.len() <= 4000);
        assert!(m.starts_with("## 📰 消息面速报 · 格式样例"));
        assert_eq!(m.matches("> **").count(), 2);
        assert_eq!(m.matches("\n- ").count(), 3);
        assert!(m.contains("🔴") && m.contains("🟡") && m.contains("🟢"));
        assert!(m.contains("[原文](https://example.com)"));
        assert!(m.trim_end().ends_with("</font>"));
    }

    #[test]
    fn redact_clean_text_unchanged() {
        let url = "https://example.com/hook";
        assert_eq!(redact("hello", url), "hello");
    }
}

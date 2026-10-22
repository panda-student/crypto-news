//! 规则手册里"硬规则"和"关键词"两节的解析与匹配，纯函数。

use plugin_sdk::contracts::screen::Level;

/// 一节规则：每行一个级别和若干小写词。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Section {
    pub rules: Vec<(Level, Vec<String>)>,
}

impl Section {
    /// 标题（加摘要）转小写后包含任一词即命中；多级命中取最高。
    pub fn matches(&self, text: &str) -> Option<Level> {
        let t = text.to_lowercase();
        self.rules
            .iter()
            .filter(|(_, words)| words.iter().any(|w| t.contains(w.as_str())))
            .map(|(l, _)| *l)
            .min()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub hard: Section,
    pub keywords: Section,
}

/// 解析规则手册，找不到的节为空。
pub fn parse(text: &str) -> Rules {
    let mut rules = Rules::default();
    let mut current: Option<bool> = None; // Some(true) = 硬规则, Some(false) = 关键词
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            current = match line {
                "## 硬规则" => Some(true),
                "## 关键词" => Some(false),
                _ => None,
            };
            continue;
        }
        let Some(hard) = current else { continue };
        if let Some((level, words)) = parse_line(line) {
            let sec = if hard {
                &mut rules.hard
            } else {
                &mut rules.keywords
            };
            sec.rules.push((level, words));
        }
    }
    rules
}

fn parse_line(line: &str) -> Option<(Level, Vec<String>)> {
    let body = line
        .strip_prefix('-')
        .or_else(|| line.strip_prefix('*'))?
        .trim();
    let idx = body.find([':', '：'])?;
    let (head, rest) = body.split_at(idx);
    let rest = &rest[rest.chars().next()?.len_utf8()..];
    let level = Level::parse(&head.trim().to_lowercase())?;
    let words: Vec<String> = rest
        .split(['、', ',', '，'])
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    Some((level, words))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# 规则\n说明文字\n\n## 硬规则\n- 紧急：暂停提现、被盗、HACK、exploit\n- 重要: SEC 起诉,ETF 获批\n\n## 关键词\n- 紧急：下架、delist\n- 重要：上线，listing、利率决议\n- 一般：公告\n- 乱写的一行\n- 未知级别：词\n- important：\n- Archive：闲聊 、 、水\n\n## 其他\n- 紧急：不该被解析\n";

    #[test]
    fn parses_sections_colons_and_separators() {
        let r = parse(TEXT);
        assert_eq!(
            r.hard.rules,
            vec![
                (
                    Level::Urgent,
                    vec![
                        "暂停提现".to_string(),
                        "被盗".into(),
                        "hack".into(),
                        "exploit".into()
                    ]
                ),
                (
                    Level::Important,
                    vec!["sec 起诉".to_string(), "etf 获批".into()]
                ),
            ]
        );
        // 半角冒号、三种分隔符、英文级别名；非法行与空词被丢弃
        assert_eq!(r.keywords.rules.len(), 4);
        assert_eq!(r.keywords.rules[1].1, vec!["上线", "listing", "利率决议"]);
        assert_eq!(
            r.keywords.rules[3],
            (Level::Archive, vec!["闲聊".into(), "水".into()])
        );
    }

    #[test]
    fn other_sections_are_ignored() {
        let r = parse(TEXT);
        assert!(r
            .hard
            .rules
            .iter()
            .chain(&r.keywords.rules)
            .all(|(_, w)| !w.contains(&"不该被解析".to_string())));
        assert_eq!(parse("没有任何节"), Rules::default());
    }

    #[test]
    fn matches_case_insensitive_and_highest_level() {
        let r = parse(TEXT);
        assert_eq!(r.hard.matches("Exchange HACKED today"), Some(Level::Urgent));
        assert_eq!(r.hard.matches("sec 起诉 某公司"), Some(Level::Important));
        // 同时命中重要和紧急，取最高
        assert_eq!(r.hard.matches("ETF 获批 后遭 exploit"), Some(Level::Urgent));
        assert_eq!(r.hard.matches("无关"), None);
        assert_eq!(
            r.keywords.matches("Binance will DELIST X"),
            Some(Level::Urgent)
        );
        assert_eq!(r.keywords.matches("发布公告"), Some(Level::Normal));
    }
}

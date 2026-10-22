//! 给模型的资料文本，以及按渠道格式排版回复。

use plugin_sdk::contracts::channel::TextFormat;
use plugin_sdk::contracts::screen::{Direction, Level};
use plugin_sdk::contracts::source::StoredItem;
use plugin_sdk::ServiceError;

/// 交给模型的一条资料。
#[derive(Debug, Clone)]
pub struct Material {
    pub item: StoredItem,
    pub level: Option<Level>,
    pub impact: Option<String>,
    /// 初筛给出的相关币种
    pub assets: Vec<String>,
    pub direction: Option<Direction>,
}

const SUMMARY_CHARS: usize = 300;
const DISCLAIMER: &str = "仅供参考，不构成投资建议";
const RULE: &str = "━━━━━━━━━━";

/// 资料按 1 起编号，模型用编号引用。
pub fn materials_prompt(materials: &[Material]) -> String {
    let mut out = String::new();
    for (i, m) in materials.iter().enumerate() {
        let level = m
            .level
            .map(|l| format!("（{}）", l.label()))
            .unwrap_or_default();
        out.push_str(&format!(
            "[{}]{} {}\n",
            i + 1,
            level,
            one_line(&m.item.title)
        ));
        let time = m
            .item
            .published_at
            .as_deref()
            .unwrap_or(&m.item.collected_at);
        out.push_str(&format!("时间：{time}\n"));
        let mut tags = Vec::new();
        if !m.assets.is_empty() {
            tags.push(format!("币种：{}", m.assets.join("、")));
        }
        if let Some(d) = m.direction {
            tags.push(format!("方向：{}", d.label()));
        }
        if !tags.is_empty() {
            out.push_str(&tags.join("；"));
            out.push('\n');
        }
        if let Some(s) = m.item.summary.as_deref().filter(|s| !s.trim().is_empty()) {
            let s: String = one_line(s).chars().take(SUMMARY_CHARS).collect();
            out.push_str(&format!("摘要：{s}\n"));
        }
        if let Some(imp) = m.impact.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push_str(&format!("影响：{}\n", one_line(imp)));
        }
        out.push('\n');
    }
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 一条待发送的回复。
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    question: String,
    body: String,
    /// (编号, 标题, 链接)
    sources: Vec<(usize, String, String)>,
}

impl Reply {
    pub(crate) fn stored(&self, materials: &[Material]) -> serde_json::Value {
        let citations: Vec<serde_json::Value> = self
            .sources
            .iter()
            .filter_map(|(n, title, url)| {
                let m = materials.get(n.checked_sub(1)?)?;
                Some(serde_json::json!({"n":n,"item_id":m.item.id,"title":title,"url":url}))
            })
            .collect();
        serde_json::json!({"answer":self.body,"citations":citations,"found":!materials.is_empty()})
    }
    pub fn not_found(question: &str) -> Reply {
        Reply {
            question: one_line(question),
            body: "最近没找到相关消息。可以换个说法，或者带上币种、项目名再问。".into(),
            sources: Vec::new(),
        }
    }

    pub fn failed(question: &str, err: &ServiceError) -> Reply {
        let body = match err.code {
            ServiceError::CREDENTIAL_EXPIRED => "模型密钥无效，请在设置页检查后再问。",
            ServiceError::QUOTA_EXHAUSTED => "模型额度已用完，请充值后再问。",
            _ => "模型暂时不可用，稍后再问。",
        };
        Reply {
            question: one_line(question),
            body: body.into(),
            sources: Vec::new(),
        }
    }

    pub fn answer(
        question: &str,
        answer: String,
        cited: &[usize],
        materials: &[Material],
    ) -> Reply {
        let sources = cited
            .iter()
            .filter_map(|&i| {
                let m = materials.get(i.checked_sub(1)?)?;
                Some((i, one_line(&m.item.title), m.item.url.clone()))
            })
            .collect();
        Reply {
            question: one_line(question),
            body: answer.trim().to_string(),
            sources,
        }
    }

    /// 按格式排版，不超过 `max_bytes`：先从末尾去掉来源，仍超长再截断正文。
    pub fn render(&self, format: TextFormat, max_bytes: usize) -> String {
        let mut r = self.clone();
        loop {
            let text = r.layout(format);
            if text.len() <= max_bytes {
                return text;
            }
            if r.sources.pop().is_some() {
                continue;
            }
            let over = text.len() - max_bytes;
            let keep = r.body.len().saturating_sub(over + "…".len());
            if keep == 0 {
                // 连问题和固定部分都放不下：直接按字节截断
                return truncate(&text, max_bytes);
            }
            r.body = format!("{}…", truncate(&r.body, keep));
        }
    }

    fn layout(&self, format: TextFormat) -> String {
        let mut out = String::new();
        match format {
            TextFormat::Markdown => {
                out.push_str("**追问回复**\n");
                out.push_str(&format!("> 问：{}\n\n", self.question));
                out.push_str(&self.body);
                out.push('\n');
                if !self.sources.is_empty() {
                    out.push_str("\n**来源**\n");
                    for (i, title, url) in &self.sources {
                        let title = title.replace('[', "【").replace(']', "】");
                        out.push_str(&format!("[{i}] [{title}]({url})\n"));
                    }
                }
                out.push_str(&format!("\n> {DISCLAIMER}"));
            }
            TextFormat::Plain => {
                out.push_str("【追问回复】\n");
                out.push_str(&format!("问：{}\n", self.question));
                out.push_str(RULE);
                out.push('\n');
                out.push_str(&self.body);
                out.push('\n');
                if !self.sources.is_empty() {
                    out.push_str(RULE);
                    out.push_str("\n来源\n");
                    for (i, title, url) in &self.sources {
                        out.push_str(&format!("[{i}] {title}\n{url}\n"));
                    }
                }
                out.push_str(RULE);
                out.push('\n');
                out.push_str(DISCLAIMER);
            }
        }
        out
    }
}

/// 截到不超过 `max` 字节的字符边界。
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

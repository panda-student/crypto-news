//! 初筛一轮：取 pending、硬规则、批量调用小模型、定级、简析、写库、发事件。

use std::collections::{HashMap, HashSet};

use plugin_sdk::contracts::events::ItemScreened;
use plugin_sdk::contracts::llm::{ChatMessage, ChatRequest, LlmClient, Tier};
use plugin_sdk::contracts::rulebook::RulebookClient;
use plugin_sdk::contracts::screen::{Direction, Level, ScreenMethod};
use plugin_sdk::contracts::source::ItemQueryClient;
use plugin_sdk::contracts::{format_rfc3339, now_rfc3339};
use plugin_sdk::ServiceError;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::rules::{self, Rules};
use crate::service::{method_str, publish_outcome};
use crate::Inner;

pub(crate) const SUMMARY_CHARS: usize = 300;
const IMPACT_CHARS: usize = 60;
pub(crate) const TITLE_ZH_CHARS: usize = 80;
pub(crate) const SUMMARY_ZH_CHARS: usize = 120;
/// 按去重后实际条数给预算，重点证据与简析仍有足够输出空间。
fn classify_max_tokens(count: usize) -> u32 {
    (256 + count.clamp(1, 10) as u32 * 768).max(2048)
}
/// 失败退避的基数（秒），第 n 次失败等 30 × 2^(n−1) 秒。
const BACKOFF_BASE_SECS: i64 = 30;

const SCREEN_PROMPT: &str = "你是加密货币消息面分析助手，负责给新消息分级。\
下面的规则手册给出四级（紧急、重要、一般、归档）的判断标准，请严格按它判断。\
用户消息是一个 JSON 数组，每项含 id、source、title、summary、published_at。\
只输出一个 JSON 对象，格式：\
{\"results\":[{\"id\":数字,\"level\":\"紧急|重要|一般|归档\",\"score\":0到1的数字,\"reason\":\"一句话理由\",\"assets\":[\"BTC\"],\"direction\":\"up|down|neutral\",\"title_zh\":\"中文标题\",\"summary_zh\":\"一句话中文摘要\",\"impact\":\"紧急或重要级的一句话影响，其他留空\"}]}。\
assets 只写消息明确涉及的币种代码（大写，最多 5 个），泛指整个市场或没有明确币种时留空数组；\
direction 只是消息面倾向（up 偏利好、down 偏利空、neutral 中性），判断不出写 neutral。\
title_zh 是标题的中文翻译或概括，原文已是中文时照抄；\nsummary_zh 是一句话中文摘要，不超过 60 字。\nreason、title_zh、summary_zh 一律使用简体中文，即使原文是英文。\n每条结果可增加 focus 对象：event_key 为同一具体事件的稳定标识（事件实际发生日期、主体、行动、产品/金额/统计周期；后续进展沿用原事件日期，不能用本次报道日期重建事件，勿仅写币种；不同事件不得共用），update_key 为本次具体事实的稳定标识（重复转述保持一致），change 为明确新增事实的一句话（无实质进展留空），priority 为 0 风险升级、1 官方确认等实质进展、2 常规信息，basis 为原文能支持的事实与仍缺的证据，source_confirmed 只有原始官方公告明确确认该事实才为 true。不可根据媒体转述或模型推断确认；不确定时 event_key 和 change 留空、priority=2、source_confirmed=false。reason 不超过 30 字，focus.change 不超过 40 字，focus.basis 不超过 60 字；impact 用不超过 60 字解释影响与不确定性，不作买卖承诺。初筛分级和重点简析必须在本次响应内一起完成。只依据输入，不得编造证据。每条消息都要有一个结果，id 与输入一致，不要输出其他文字。";

/// 一条待筛消息。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Pending {
    pub item_id: i64,
    pub source_id: String,
    pub url: String,
    pub title: String,
    pub published_at: Option<String>,
    pub backfill: i64,
    pub attempts: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct Decision {
    pub impact: Option<String>,
    pub focus: Option<plugin_sdk::contracts::focus::Focus>,
    pub level: Level,
    pub score: Option<f64>,
    pub reason: Option<String>,
    pub method: ScreenMethod,
    pub assets: Vec<String>,
    pub direction: Option<Direction>,
    pub title_zh: Option<String>,
    pub summary_zh: Option<String>,
}

/// 跑一轮，返回本轮取到的 pending 条数。所有错误只记日志，不中断循环。
pub(crate) async fn run_round(inner: &Inner) -> usize {
    let ctx = &inner.ctx;
    let llm = LlmClient(ctx.service("llm"));
    match llm.ready().await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!("模型未就绪，跳过本轮初筛");
            return 0;
        }
        Err(e) => {
            tracing::debug!("查询模型状态失败，跳过本轮初筛：{}", e.message);
            return 0;
        }
    }
    let rulebook = match RulebookClient(ctx.service("rulebook")).get().await {
        Ok(r) => r.text,
        Err(e) => {
            tracing::warn!("读取规则手册失败，跳过本轮初筛：{}", e.message);
            return 0;
        }
    };
    let rules = rules::parse(&rulebook);

    let now = now_rfc3339();
    let pending: Vec<Pending> = match ctx
        .store()
        .query_as(
            "SELECT item_id, source_id, url, title, published_at, backfill, attempts \
             FROM screenings WHERE status = 'pending' AND (next_at IS NULL OR next_at <= ?) \
             ORDER BY item_id LIMIT ?",
            vec![json!(now), json!(inner.cfg.batch_size as i64)],
        )
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("读取待筛消息失败：{}", e.message);
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }
    let fetched = pending.len();

    // 补摘要，取不到不影响
    let ids: Vec<i64> = pending.iter().map(|p| p.item_id).collect();
    let summaries: HashMap<i64, String> =
        match ItemQueryClient(ctx.service("item-query")).get(&ids).await {
            Ok(items) => items
                .into_iter()
                .filter_map(|i| i.summary.filter(|s| !s.is_empty()).map(|s| (i.id, s)))
                .collect(),
            Err(e) => {
                tracing::debug!("读取摘要失败：{}", e.message);
                HashMap::new()
            }
        };

    let env = Env {
        inner,
        llm: &llm,
        rulebook: &rulebook,
        rules: &rules,
        summaries: &summaries,
    };

    // 硬规则保底；紧急/重要进入同一批补齐简析，模型可升级但不可降级。
    let mut rest: Vec<&Pending> = vec![];
    let mut forced = HashMap::new();
    let mut signatures: HashMap<String, i64> = HashMap::new();
    let mut duplicates: Vec<(&Pending, i64)> = vec![];
    for p in &pending {
        let hard = rules.hard.matches(&env.match_text(p));
        if let Some(level) = hard {
            if !matches!(level, Level::Urgent | Level::Important) {
                env.finalize(p, rule_decision(level), p.attempts).await;
                continue;
            }
            forced.insert(p.item_id, level);
        }
        if let Some(d) = env.cached(p).await {
            env.finalize(p, d, p.attempts).await;
            continue;
        }
        if let Some(key) = env.input_hash(p) {
            if let Some(first) = signatures.get(&key) {
                duplicates.push((p, *first));
                continue;
            }
            signatures.insert(key, p.item_id);
        }
        rest.push(p);
    }
    if rest.is_empty() {
        return fetched;
    }
    let verdicts = match env.classify(&rest).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("批量初筛失败：{}", e.message);
            HashMap::new()
        }
    };
    let mut resolved = HashMap::new();
    for p in rest {
        let d = match (verdicts.get(&p.item_id), forced.get(&p.item_id)) {
            (Some(d), hard) => {
                let mut d = d.clone();
                if let Some(level) = hard {
                    d.level = d.level.min(*level);
                    d.method = ScreenMethod::Rule;
                    d.reason = Some("命中硬规则".to_string());
                }
                Some(d)
            }
            (None, Some(level)) => Some(rule_decision(*level)),
            _ => None,
        };
        if let Some(d) = d {
            env.finalize(p, d.clone(), p.attempts).await;
            resolved.insert(p.item_id, d);
        } else {
            env.fail(p).await;
        }
    }
    for (p, first) in duplicates {
        if let Some(d) = resolved.get(&first) {
            env.finalize(p, d.clone(), p.attempts).await;
        } else {
            env.fail(p).await;
        }
    }
    fetched
}

fn rule_decision(level: Level) -> Decision {
    Decision {
        level,
        impact: None,
        focus: None,
        score: None,
        reason: Some("命中硬规则".to_string()),
        method: ScreenMethod::Rule,
        assets: Vec::new(),
        direction: None,
        title_zh: None,
        summary_zh: None,
    }
}

pub(crate) struct Env<'a> {
    pub inner: &'a Inner,
    pub llm: &'a LlmClient,
    pub rulebook: &'a str,
    pub rules: &'a Rules,
    pub summaries: &'a HashMap<i64, String>,
}

impl Env<'_> {
    fn match_text(&self, p: &Pending) -> String {
        match self.summaries.get(&p.item_id) {
            Some(s) => format!("{}\n{}", p.title, s),
            None => p.title.clone(),
        }
    }

    // 只复用同信源、同一天且完整标题/摘要完全相同的报道；不使用模糊相似度。
    fn input_hash(&self, p: &Pending) -> Option<String> {
        let summary = self.summaries.get(&p.item_id)?.trim();
        let published = plugin_sdk::contracts::parse_rfc3339(p.published_at.as_deref()?)?;
        if summary.is_empty() || p.title.trim().is_empty() {
            return None;
        }
        let day = format_rfc3339(published.to_offset(time::UtcOffset::UTC));
        let origin = url::Url::parse(&p.url).ok()?.origin().ascii_serialization();
        let data = json!([
            p.source_id,
            origin,
            p.title.trim(),
            summary,
            &day[..10],
            self.rulebook
        ]);
        Some(format!("{:x}", Sha256::digest(data.to_string().as_bytes())))
    }

    async fn cached(&self, p: &Pending) -> Option<Decision> {
        let key = self.input_hash(p)?;
        let rows: Vec<crate::service::Row> = self.inner.ctx.store().query_as(
            &format!("SELECT {} FROM screenings WHERE input_hash = ? AND status = 'done' AND method = 'llm' ORDER BY item_id DESC LIMIT 1", crate::service::COLS),
            vec![json!(key)],
        ).await.ok()?;
        let s = rows.first()?.to_screening();
        Some(Decision {
            impact: s.impact,
            focus: s.focus,
            level: s.level?,
            score: s.score,
            reason: s.reason,
            method: ScreenMethod::Llm,
            assets: s.assets,
            direction: s.direction,
            title_zh: s.title_zh,
            summary_zh: s.summary_zh,
        })
    }

    pub(crate) async fn classify(
        &self,
        items: &[&Pending],
    ) -> Result<HashMap<i64, Decision>, ServiceError> {
        let arr: Vec<Value> = items
            .iter()
            .map(|p| {
                json!({
                    "id": p.item_id,
                    "source": p.source_id,
                    "title": p.title,
                    "summary": self.summaries.get(&p.item_id)
                        .map(|s| truncate_chars(s, SUMMARY_CHARS)),
                    "published_at": p.published_at,
                })
            })
            .collect();
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system(format!(
                    "{SCREEN_PROMPT}\n\n# 规则手册\n\n{}",
                    self.rulebook
                )),
                ChatMessage::user(Value::Array(arr).to_string()),
            ],
            tier: Tier::Small,
            json: true,
            max_tokens: Some(classify_max_tokens(items.len())),
            temperature: None,
            purpose: Some("screen".to_string()),
        };
        let resp = self.llm.chat(&req).await?;
        let wanted: HashSet<i64> = items.iter().map(|p| p.item_id).collect();
        Ok(parse_results(&resp.content, &wanted))
    }

    /// 模型失败或缺条：attempts + 1，退避；达到上限则关键词兜底或转人工。
    async fn fail(&self, p: &Pending) {
        let attempts = p.attempts + 1;
        let store = self.inner.ctx.store();
        if attempts >= self.inner.cfg.max_attempts {
            if let Some(level) = self.rules.keywords.matches(&self.match_text(p)) {
                let d = Decision {
                    impact: None,
                    level,
                    score: None,
                    reason: Some("模型多次失败，按关键词定级".to_string()),
                    method: ScreenMethod::Keyword,
                    assets: Vec::new(),
                    direction: None,
                    title_zh: None,
                    summary_zh: None,
                    focus: None,
                };
                self.finalize(p, d, attempts).await;
            } else if let Err(e) = store
                .execute(
                    "UPDATE screenings SET status = 'manual', attempts = ?, next_at = NULL \
                     WHERE item_id = ? AND status = 'pending'",
                    vec![json!(attempts), json!(p.item_id)],
                )
                .await
            {
                tracing::warn!("更新初筛记录失败：{}", e.message);
            }
            return;
        }
        let secs = BACKOFF_BASE_SECS << (attempts - 1).clamp(0, 20);
        let next = format_rfc3339(time::OffsetDateTime::now_utc() + time::Duration::seconds(secs));
        if let Err(e) = store
            .execute(
                "UPDATE screenings SET attempts = ?, next_at = ? \
                 WHERE item_id = ? AND status = 'pending'",
                vec![json!(attempts), json!(next), json!(p.item_id)],
            )
            .await
        {
            tracing::warn!("更新初筛记录失败：{}", e.message);
        }
    }

    /// 定级之后：紧急、重要级简析，写库，发事件。
    async fn finalize(&self, p: &Pending, d: Decision, attempts: i64) {
        let impact = matches!(d.level, Level::Urgent | Level::Important)
            .then(|| d.impact.clone())
            .flatten();
        let now = now_rfc3339();
        let res = self
            .inner
            .ctx
            .store()
            .execute(
                "UPDATE screenings SET level = ?, score = ?, reason = ?, impact = ?, \
                 status = 'done', method = ?, attempts = ?, screened_at = ?, next_at = NULL, \
                 assets = ?, direction = ?, title_zh = ?, summary_zh = ?, focus = ?, analyzed = ?, input_hash = ? WHERE item_id = ? AND status = 'pending'",
                vec![
                    json!(d.level.as_str()),
                    json!(d.score),
                    json!(d.reason),
                    json!(impact),
                    json!(method_str(d.method)),
                    json!(attempts),
                    json!(now),
                    json!(json!(d.assets).to_string()),
                    json!(d.direction.map(Direction::as_str)),
                    json!(d.title_zh),
                    json!(d.summary_zh),
                    json!(d.focus.as_ref().map(|v|serde_json::to_string(v).unwrap_or_default())),
                    json!(1),
                    json!(self.input_hash(p)),
                    json!(p.item_id),
                ],
            )
            .await;
        match res {
            // 简析期间被人工改级或删除时不再覆盖、不发事件
            Ok(r) if r.rows_affected == 0 => {}
            Ok(_) => {
                let ev = ItemScreened {
                    item_id: p.item_id,
                    source_id: p.source_id.clone(),
                    url: p.url.clone(),
                    title: p.title.clone(),
                    published_at: p.published_at.clone(),
                    level: d.level,
                    score: d.score,
                    reason: d.reason,
                    impact,
                    method: d.method,
                    backfill: p.backfill != 0,
                    assets: d.assets,
                    direction: d.direction,
                };
                publish_outcome(self.inner, &ev).await;
            }
            Err(e) => tracing::warn!("写入初筛结果失败：{}", e.message),
        }
    }
}

pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 解析模型返回：id 可为数字或字符串，级别可为中文或英文；score 夹到 0～1，非数字则留空。
/// 只保留属于本批的 id，重复 id 以第一个为准，级别不合法的条目丢弃。
fn parse_results(content: &str, wanted: &HashSet<i64>) -> HashMap<i64, Decision> {
    let mut out = HashMap::new();
    let Ok(v) = serde_json::from_str::<Value>(content.trim()) else {
        return out;
    };
    let arr = match &v {
        Value::Array(a) => a.as_slice(),
        other => match other.get("results").and_then(Value::as_array) {
            Some(a) => a.as_slice(),
            None => return out,
        },
    };
    for r in arr {
        let Some(id) = r.get("id").and_then(parse_id) else {
            continue;
        };
        if !wanted.contains(&id) || out.contains_key(&id) {
            continue;
        }
        let Some(level) = r.get("level").and_then(Value::as_str).and_then(parse_level) else {
            continue;
        };
        let score = r.get("score").and_then(parse_score);
        let reason = r
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        out.insert(
            id,
            Decision {
                impact: clean_text(r.get("impact"), IMPACT_CHARS),
                level,
                score,
                reason,
                method: ScreenMethod::Llm,
                assets: r.get("assets").map(clean_assets).unwrap_or_default(),
                direction: r
                    .get("direction")
                    .and_then(Value::as_str)
                    .and_then(Direction::parse),
                title_zh: clean_text(r.get("title_zh"), TITLE_ZH_CHARS),
                summary_zh: clean_text(r.get("summary_zh"), SUMMARY_ZH_CHARS),
                focus: r
                    .get("focus")
                    .cloned()
                    .and_then(|v| {
                        serde_json::from_value::<plugin_sdk::contracts::focus::Focus>(v).ok()
                    })
                    .map(|v| v.clean()),
            },
        );
    }
    out
}

/// 取字符串字段，去空白，空串或非字符串为 None，超长截断。
pub(crate) fn clean_text(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?.trim();
    (!s.is_empty()).then(|| truncate_chars(s, max))
}

/// 清洗币种：非数组丢弃；元素转大写去空白，只保留 1～10 位字母数字，去重，最多 5 个。
fn clean_assets(v: &Value) -> Vec<String> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for a in arr {
        let Some(s) = a.as_str() else { continue };
        let s = s.trim().to_uppercase();
        let ok = (1..=10).contains(&s.chars().count())
            && s.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if ok && !out.contains(&s) {
            out.push(s);
            if out.len() == 5 {
                break;
            }
        }
    }
    out
}

pub(crate) fn parse_id(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9e15)
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_level(s: &str) -> Option<Level> {
    let s = s.trim().to_lowercase();
    Level::parse(&s).or_else(|| Level::parse(s.trim_end_matches('级')))
}

fn parse_score(v: &Value) -> Option<f64> {
    let f = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    f.is_finite().then_some(f.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_budget_scales_without_starving_small_batches() {
        assert_eq!(classify_max_tokens(1), 2048);
        assert_eq!(classify_max_tokens(2), 2048);
        assert_eq!(classify_max_tokens(10), 7936);
        assert_eq!(classify_max_tokens(100), 7936);
    }

    #[test]
    fn tolerant_result_parsing() {
        let wanted: HashSet<i64> = [1, 2, 3, 4].into();
        let content = r#"{"results":[
            {"id":"1","level":"Urgent","score":"0.9","reason":" 被盗 "},
            {"id":2.0,"level":"重要级","score":7},
            {"id":3,"level":"一般"},
            {"id":4,"level":"???","score":0.5},
            {"id":9,"level":"紧急"},
            {"id":1,"level":"归档"}
        ]}"#;
        let r = parse_results(content, &wanted);
        assert_eq!(r.len(), 3);
        assert_eq!(r[&1].level, Level::Urgent);
        assert_eq!(r[&1].score, Some(0.9));
        assert_eq!(r[&1].reason.as_deref(), Some("被盗"));
        assert_eq!(r[&2].level, Level::Important);
        assert_eq!(r[&2].score, Some(1.0));
        assert_eq!(r[&3].score, None);
        assert!(parse_results("不是 JSON", &wanted).is_empty());
        assert_eq!(
            parse_results(r#"[{"id":1,"level":"归档"}]"#, &wanted).len(),
            1
        );
    }

    #[test]
    fn assets_and_direction_cleaning() {
        let wanted: HashSet<i64> = [1, 2].into();
        let content = r#"{"results":[
            {"id":1,"level":"一般","direction":"bullish",
             "assets":[" btc ","BTC","eth","bad-x",12,"","ABCDEFGHIJK","sol","doge","ada","xrp"]},
            {"id":2,"level":"一般","direction":"乱写","assets":"BTC"}
        ]}"#;
        let r = parse_results(content, &wanted);
        assert_eq!(r[&1].assets, vec!["BTC", "ETH", "SOL", "DOGE", "ADA"]);
        assert_eq!(r[&1].direction, Some(Direction::Up));
        assert!(r[&2].assets.is_empty());
        assert_eq!(r[&2].direction, None);
    }

    #[test]
    fn zh_fields_optional() {
        let wanted: HashSet<i64> = [1, 2].into();
        let content = r#"{"results":[
            {"id":1,"level":"一般","title_zh":" 中文标题 ","summary_zh":"摘要"},
            {"id":2,"level":"一般","title_zh":"","summary_zh":5}
        ]}"#;
        let r = parse_results(content, &wanted);
        assert_eq!(r[&1].title_zh.as_deref(), Some("中文标题"));
        assert_eq!(r[&1].summary_zh.as_deref(), Some("摘要"));
        assert_eq!(r[&2].title_zh, None);
        assert_eq!(r[&2].summary_zh, None);
    }
}

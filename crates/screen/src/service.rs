//! `screening` 服务与共用的行类型、事件发布。

use plugin_sdk::contracts::events::{
    ItemScreened, PushItem, PushPending, ITEM_SCREENED, PUSH_PENDING,
};
use plugin_sdk::contracts::screen::{
    Direction, ItemIds, Level, ScreenMethod, ScreenStatus, Screening, ScreeningList,
    ScreeningQuery, ScreeningStats, StatsCount, StatsQuery,
};
use plugin_sdk::contracts::{format_rfc3339, now_rfc3339, parse_params, parse_rfc3339, to_result};
use plugin_sdk::{Caller, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::Inner;

pub const COLS: &str = "item_id, source_id, url, title, published_at, level, score, reason, \
                        impact, status, method, attempts, backfill, screened_at, assets, direction, \n                        title_zh, summary_zh, focus";
const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 1000;

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读写数据失败：{}", e.message))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Row {
    pub focus: Option<String>,
    pub item_id: i64,
    pub source_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub published_at: Option<String>,
    pub level: Option<String>,
    pub score: Option<f64>,
    pub reason: Option<String>,
    pub impact: Option<String>,
    pub status: String,
    pub method: Option<String>,
    pub attempts: i64,
    pub backfill: i64,
    pub screened_at: Option<String>,
    pub assets: Option<String>,
    pub direction: Option<String>,
    pub title_zh: Option<String>,
    pub summary_zh: Option<String>,
}

fn from_str<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_string())).ok()
}

impl Row {
    fn asset_list(&self) -> Vec<String> {
        self.assets
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    fn direction_value(&self) -> Option<Direction> {
        self.direction.as_deref().and_then(Direction::parse)
    }

    pub fn to_screening(&self) -> Screening {
        Screening {
            focus: self
                .focus
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            item_id: self.item_id,
            level: self.level.as_deref().and_then(Level::parse),
            score: self.score,
            reason: self.reason.clone(),
            impact: self.impact.clone(),
            status: from_str(&self.status).unwrap_or(ScreenStatus::Pending),
            method: self.method.as_deref().and_then(from_str),
            attempts: self.attempts,
            backfill: self.backfill != 0,
            screened_at: self.screened_at.clone(),
            assets: self.asset_list(),
            direction: self.direction_value(),
            title_zh: self.title_zh.clone().filter(|s| !s.is_empty()),
            summary_zh: self.summary_zh.clone().filter(|s| !s.is_empty()),
        }
    }

    pub fn to_event(&self, level: Level, method: ScreenMethod) -> ItemScreened {
        ItemScreened {
            item_id: self.item_id,
            source_id: self.source_id.clone().unwrap_or_default(),
            url: self.url.clone().unwrap_or_default(),
            title: self.title.clone().unwrap_or_default(),
            published_at: self.published_at.clone(),
            level,
            score: self.score,
            reason: self.reason.clone(),
            impact: self.impact.clone(),
            method,
            backfill: self.backfill != 0,
            assets: self.asset_list(),
            direction: self.direction_value(),
        }
    }
}

pub fn method_str(m: ScreenMethod) -> &'static str {
    match m {
        ScreenMethod::Llm => "llm",
        ScreenMethod::Rule => "rule",
        ScreenMethod::Keyword => "keyword",
        ScreenMethod::Manual => "manual",
    }
}

/// 发 `item.screened`；紧急且非补抓时再发 `push.pending`。发布失败只记日志。
pub async fn publish_outcome(inner: &Inner, ev: &ItemScreened) {
    if let Err(e) = inner.ctx.publish(ITEM_SCREENED, ev).await {
        tracing::warn!("发布 item.screened 失败：{e}");
    }
    if ev.level == Level::Urgent && !ev.backfill {
        let pending = PushPending {
            kind: "flash".to_string(),
            items: vec![PushItem {
                item_id: ev.item_id,
                level: ev.level,
                title: ev.title.clone(),
                impact: ev.impact.clone(),
                url: ev.url.clone(),
                source_id: ev.source_id.clone(),
                published_at: ev.published_at.clone(),
            }],
        };
        if let Err(e) = inner.ctx.publish(PUSH_PENDING, &pending).await {
            tracing::warn!("发布 push.pending 失败：{e}");
        }
    }
}

async fn fetch_row(inner: &Inner, id: i64) -> Result<Option<Row>, ServiceError> {
    let rows: Vec<Row> = inner
        .ctx
        .store()
        .query_as(
            &format!("SELECT {COLS} FROM screenings WHERE item_id = ?"),
            vec![json!(id)],
        )
        .await
        .map_err(db_err)?;
    Ok(rows.into_iter().next())
}

fn check_ids(ids: &[i64]) -> Result<(), ServiceError> {
    if ids.len() > MAX_LIMIT as usize {
        return Err(ServiceError::invalid_params("ids 最多 1000 个"));
    }
    Ok(())
}

pub async fn call(
    inner: &Inner,
    caller: Caller,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    let store = inner.ctx.store();
    match method {
        "get" => {
            #[derive(Deserialize)]
            struct P {
                ids: Vec<i64>,
            }
            let p: P = parse_params(params)?;
            check_ids(&p.ids)?;
            if p.ids.is_empty() {
                return to_result(&ScreeningList { screenings: vec![] });
            }
            let marks = vec!["?"; p.ids.len()].join(",");
            let rows: Vec<Row> = store
                .query_as(
                    &format!(
                        "SELECT {COLS} FROM screenings WHERE item_id IN ({marks}) ORDER BY item_id"
                    ),
                    p.ids.iter().map(|i| json!(i)).collect(),
                )
                .await
                .map_err(db_err)?;
            to_result(&ScreeningList {
                screenings: rows.iter().map(Row::to_screening).collect(),
            })
        }
        "list" => {
            let q: ScreeningQuery = parse_params(params)?;
            let mut conds: Vec<&str> = vec![];
            let mut args: Vec<Value> = vec![];
            if let Some(l) = q.level {
                conds.push("level = ?");
                args.push(json!(l.as_str()));
            }
            if let Some(s) = q.status {
                conds.push("status = ?");
                args.push(serde_json::to_value(s).map_err(ServiceError::internal)?);
            }
            if let Some(v) = q.since {
                conds.push("screened_at >= ?");
                args.push(json!(v));
            }
            if let Some(v) = q.until {
                conds.push("screened_at < ?");
                args.push(json!(v));
            }
            let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
            let wh = if conds.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conds.join(" AND "))
            };
            let sql = format!(
                "SELECT {COLS} FROM screenings{wh} \
                 ORDER BY COALESCE(screened_at, created_at) DESC, item_id DESC LIMIT {limit}"
            );
            let rows: Vec<Row> = store.query_as(&sql, args).await.map_err(db_err)?;
            to_result(&ScreeningList {
                screenings: rows.iter().map(Row::to_screening).collect(),
            })
        }
        "review" => {
            if caller.user_id != plugin_sdk::contracts::OWNER
                || !matches!(caller.plugin.as_str(), "web" | "operations")
            {
                return Err(ServiceError::forbidden(
                    "只有界面或已确认的应用操作能人工改级",
                ));
            }
            #[derive(Deserialize)]
            struct P {
                item_id: i64,
                level: Level,
            }
            let p: P = parse_params(params)?;
            let Some(mut row) = fetch_row(inner, p.item_id).await? else {
                return Err(ServiceError::invalid_params(format!(
                    "消息 {} 没有初筛记录",
                    p.item_id
                )));
            };
            let now = now_rfc3339();
            store
                .execute(
                    "UPDATE screenings SET level = ?, method = 'manual', status = 'done', \
                     screened_at = ?, next_at = NULL WHERE item_id = ?",
                    vec![json!(p.level.as_str()), json!(now), json!(p.item_id)],
                )
                .await
                .map_err(db_err)?;
            row.level = Some(p.level.as_str().to_string());
            row.method = Some("manual".to_string());
            row.status = "done".to_string();
            row.screened_at = Some(now);
            publish_outcome(inner, &row.to_event(p.level, ScreenMethod::Manual)).await;
            to_result(&row.to_screening())
        }
        "stats" => {
            let q: StatsQuery = parse_params(params)?;
            let since = parse_rfc3339(&q.since)
                .map(format_rfc3339)
                .ok_or_else(|| ServiceError::invalid_params("since 不是 RFC3339 时间"))?;
            let until = match &q.until {
                Some(u) => parse_rfc3339(u)
                    .map(format_rfc3339)
                    .ok_or_else(|| ServiceError::invalid_params("until 不是 RFC3339 时间"))?,
                None => now_rfc3339(),
            };
            let rows = store
                .query(
                    "SELECT source_id, level, screened_at, assets FROM screenings \
                     WHERE status = 'done' AND screened_at >= ? AND screened_at < ?",
                    vec![json!(since), json!(until)],
                )
                .await
                .map_err(db_err)?;
            let mut by_day: BTreeMap<(String, Level), i64> = BTreeMap::new();
            let mut by_source: BTreeMap<(String, Level), i64> = BTreeMap::new();
            let mut by_asset: BTreeMap<(String, Level), i64> = BTreeMap::new();
            for r in &rows {
                let Some(level) = r
                    .get("level")
                    .and_then(Value::as_str)
                    .and_then(Level::parse)
                else {
                    continue;
                };
                if let Some(day) = r
                    .get("screened_at")
                    .and_then(Value::as_str)
                    .and_then(|s| s.get(..10))
                {
                    *by_day.entry((day.to_string(), level)).or_default() += 1;
                }
                if let Some(src) = r.get("source_id").and_then(Value::as_str) {
                    *by_source.entry((src.to_string(), level)).or_default() += 1;
                }
                let assets: Vec<String> = r
                    .get("assets")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let mut seen: Vec<&String> = vec![];
                for a in &assets {
                    if !seen.contains(&a) {
                        seen.push(a);
                        *by_asset.entry((a.clone(), level)).or_default() += 1;
                    }
                }
            }
            let flat = |m: BTreeMap<(String, Level), i64>| -> Vec<StatsCount> {
                m.into_iter()
                    .map(|((key, level), count)| StatsCount { key, level, count })
                    .collect()
            };
            to_result(&ScreeningStats {
                by_day: flat(by_day),
                by_source: flat(by_source),
                by_asset: flat(by_asset),
            })
        }
        "archived_before" => {
            #[derive(Deserialize)]
            struct P {
                before: String,
                #[serde(default)]
                limit: Option<i64>,
            }
            let p: P = parse_params(params)?;
            let limit = p.limit.unwrap_or(MAX_LIMIT).clamp(1, MAX_LIMIT);
            let rows = store
                .query(
                    &format!(
                        "SELECT item_id FROM screenings WHERE level = 'archive' \
                         AND screened_at < ? ORDER BY item_id LIMIT {limit}"
                    ),
                    vec![json!(p.before)],
                )
                .await
                .map_err(db_err)?;
            let ids = rows
                .iter()
                .filter_map(|r| r.get("item_id").and_then(Value::as_i64))
                .collect();
            to_result(&ItemIds { ids })
        }
        "delete" => {
            if caller.plugin != "maintenance" {
                return Err(ServiceError::forbidden("只有 maintenance 能删除初筛记录"));
            }
            #[derive(Deserialize)]
            struct P {
                ids: Vec<i64>,
            }
            let p: P = parse_params(params)?;
            check_ids(&p.ids)?;
            if p.ids.is_empty() {
                return Ok(json!({ "deleted": 0 }));
            }
            let marks = vec!["?"; p.ids.len()].join(",");
            let r = store
                .execute(
                    &format!("DELETE FROM screenings WHERE item_id IN ({marks})"),
                    p.ids.iter().map(|i| json!(i)).collect(),
                )
                .await
                .map_err(db_err)?;
            Ok(json!({ "deleted": r.rows_affected }))
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

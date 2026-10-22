//! `screening` 服务 1.1：初筛结果查询、人工改级与统计，screen 插件提供。
//!
//! 1.1 新增：`Screening.assets`、`Screening.direction`、`stats` 方法。

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{call_typed, Service, ServiceError};

pub const SERVICE: &str = "screening";

/// 四级分级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// 简析后推快讯
    Urgent,
    /// 进日报，放在最前面单独一栏
    Important,
    /// 进日报
    Normal,
    /// 只保留，不推送
    Archive,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Urgent => "紧急",
            Level::Important => "重要",
            Level::Normal => "一般",
            Level::Archive => "归档",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Urgent => "urgent",
            Level::Important => "important",
            Level::Normal => "normal",
            Level::Archive => "archive",
        }
    }
    /// 接受英文名或中文标签。
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim() {
            "urgent" | "紧急" => Some(Level::Urgent),
            "important" | "重要" => Some(Level::Important),
            "normal" | "一般" => Some(Level::Normal),
            "archive" | "归档" => Some(Level::Archive),
            _ => None,
        }
    }
}

/// 对相关币种的方向判断，只描述消息面倾向，不是价格预测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 偏利好
    Up,
    /// 偏利空
    Down,
    /// 中性或无法判断
    Neutral,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::Up => "利好",
            Direction::Down => "利空",
            Direction::Neutral => "中性",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Neutral => "neutral",
        }
    }
    /// 接受英文名或中文标签。
    pub fn parse(s: &str) -> Option<Direction> {
        match s.trim() {
            "up" | "利好" | "bullish" => Some(Direction::Up),
            "down" | "利空" | "bearish" => Some(Direction::Down),
            "neutral" | "中性" => Some(Direction::Neutral),
            _ => None,
        }
    }
}

/// 初筛状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenStatus {
    /// 等待或正在初筛
    Pending,
    /// 已分级
    Done,
    /// 模型失败 3 次且规则未命中，转人工
    Manual,
}

/// 分级方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenMethod {
    Llm,
    /// 硬规则
    Rule,
    /// 关键词兜底
    Keyword,
    /// 人工
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screening {
    #[serde(default)]
    pub focus: Option<crate::focus::Focus>,
    pub item_id: i64,
    /// 状态为 pending / manual 时为空
    #[serde(default)]
    pub level: Option<Level>,
    /// 0~1
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub reason: Option<String>,
    /// 简析得到的"影响"，一句话；紧急和重要级才有
    #[serde(default)]
    pub impact: Option<String>,
    pub status: ScreenStatus,
    #[serde(default)]
    pub method: Option<ScreenMethod>,
    /// 模型调用失败次数
    pub attempts: i64,
    /// 开机补抓且发布时间早于 2 小时前
    #[serde(default)]
    pub backfill: bool,
    /// RFC3339 UTC
    #[serde(default)]
    pub screened_at: Option<String>,
    /// 涉及的币种代码（大写，如 BTC），最多 5 个；无明确币种时为空
    #[serde(default)]
    pub assets: Vec<String>,
    /// 方向判断；未分级或判断不出时为空
    #[serde(default)]
    pub direction: Option<Direction>,
    /// 模型给出的中文标题；规则命中、旧数据或模型未给出时为空，显示时回退原标题
    #[serde(default)]
    pub title_zh: Option<String>,
    /// 模型给出的一句话中文摘要；为空时不显示
    #[serde(default)]
    pub summary_zh: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreeningQuery {
    #[serde(default)]
    pub level: Option<Level>,
    #[serde(default)]
    pub status: Option<ScreenStatus>,
    /// 按 screened_at 过滤，RFC3339 UTC，含
    #[serde(default)]
    pub since: Option<String>,
    /// 不含
    #[serde(default)]
    pub until: Option<String>,
    /// 缺省 100，最大 1000
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreeningList {
    pub screenings: Vec<Screening>,
}

/// `stats` 的参数：按 screened_at 统计已分级（done）的消息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsQuery {
    /// RFC3339 UTC，含
    pub since: String,
    /// 不含；缺省到现在
    #[serde(default)]
    pub until: Option<String>,
}

/// 一个统计桶的计数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsCount {
    /// 分组键：日期（YYYY-MM-DD，UTC）、信源 ID 或币种代码
    pub key: String,
    pub level: Level,
    pub count: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreeningStats {
    /// 按天（UTC 日期）× 级别
    pub by_day: Vec<StatsCount>,
    /// 按信源 ID × 级别
    pub by_source: Vec<StatsCount>,
    /// 按币种 × 级别（一条消息涉及多个币种时每个币种各计一次）
    pub by_asset: Vec<StatsCount>,
}

#[derive(Clone)]
pub struct ScreeningClient(pub Service);

impl ScreeningClient {
    /// 按消息 ID 批量取，不存在的跳过。
    pub async fn get(&self, ids: &[i64]) -> Result<Vec<Screening>, ServiceError> {
        let r: ScreeningList = call_typed(&*self.0, "get", &json!({ "ids": ids })).await?;
        Ok(r.screenings)
    }
    pub async fn list(&self, q: &ScreeningQuery) -> Result<Vec<Screening>, ServiceError> {
        let r: ScreeningList = call_typed(&*self.0, "list", q).await?;
        Ok(r.screenings)
    }
    /// 人工改级（只允许 web 调用），会重新发 `item.screened`。
    pub async fn review(&self, item_id: i64, level: Level) -> Result<Screening, ServiceError> {
        call_typed(
            &*self.0,
            "review",
            &json!({ "item_id": item_id, "level": level }),
        )
        .await
    }
    /// 按天、信源、币种统计已分级消息。
    pub async fn stats(&self, q: &StatsQuery) -> Result<ScreeningStats, ServiceError> {
        call_typed(&*self.0, "stats", q).await
    }
    /// 归档级且 screened_at 早于 `before` 的消息 ID，按 item_id 升序，最多 `limit` 条（最大 1000）。
    pub async fn archived_before(
        &self,
        before: &str,
        limit: i64,
    ) -> Result<Vec<i64>, ServiceError> {
        let r: ItemIds = call_typed(
            &*self.0,
            "archived_before",
            &json!({ "before": before, "limit": limit }),
        )
        .await?;
        Ok(r.ids)
    }
    /// 删除初筛记录，返回删除条数。只允许 maintenance 调用。
    pub async fn delete(&self, ids: &[i64]) -> Result<u64, ServiceError> {
        let r: serde_json::Value = self.0.call("delete", json!({ "ids": ids })).await?;
        Ok(r.get("deleted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemIds {
    pub ids: Vec<i64>,
}

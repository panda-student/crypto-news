use crate::{
    parse_rfc3339,
    screen::{Direction, Level},
    ServiceError,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Focus {
    pub event_key: String,
    pub update_key: String,
    pub change: String,
    pub priority: u8,
    pub basis: String,
    pub source_confirmed: bool,
}
impl Default for Focus {
    fn default() -> Self {
        Self {
            event_key: String::new(),
            update_key: String::new(),
            change: String::new(),
            priority: 2,
            basis: String::new(),
            source_confirmed: false,
        }
    }
}
impl Focus {
    pub fn clean(mut self) -> Self {
        self.event_key = self.event_key.chars().take(160).collect();
        self.update_key = self.update_key.chars().take(160).collect();
        self.change = self.change.chars().take(120).collect();
        self.basis = self.basis.chars().take(240).collect();
        self.priority = if self.change.trim().is_empty() {
            2
        } else {
            self.priority.min(2)
        };
        if self.basis.trim().is_empty() {
            self.source_confirmed = false;
        }
        self
    }
}
pub fn identity(title: &str, at: Option<&str>, focus: Option<&Focus>, id: i64) -> String {
    let day = at
        .and_then(parse_rfc3339)
        .map(|t| t.date().to_string())
        .unwrap_or_else(|| format!("unknown:{id}"));
    let key = focus
        .map(|f| f.event_key.trim())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            title
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect()
        });
    if focus.is_some_and(|f| !f.event_key.trim().is_empty()) {
        format!("event|{key}")
    } else {
        format!("{day}|{key}")
    }
}
pub fn in_window(
    level: Level,
    direction: Option<Direction>,
    at: Option<&str>,
    now: OffsetDateTime,
) -> bool {
    let Some(at) = at.and_then(parse_rfc3339) else {
        return false;
    };
    let age = (now - at).whole_seconds();
    age >= 0
        && match level {
            Level::Urgent => age < 21600,
            Level::Important => {
                age < 86400 && matches!(direction, Some(Direction::Up | Direction::Down))
            }
            _ => false,
        }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushSettings {
    pub enabled: bool,
    pub scope: String,
    pub assets: Vec<String>,
    pub interval: u32,
    pub effective_at: String,
}
impl Default for PushSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            scope: "all".into(),
            assets: vec![],
            interval: 5,
            effective_at: String::new(),
        }
    }
}
impl PushSettings {
    pub fn validate(&self) -> Result<(), ServiceError> {
        if !["all", "urgent", "followed"].contains(&self.scope.as_str())
            || ![5, 10, 15].contains(&self.interval)
            || self.assets.len() > 30
            || self.assets.iter().any(|a| {
                a.is_empty() || a.len() > 20 || !a.chars().all(|c| c.is_ascii_alphanumeric())
            })
            || self.enabled && self.scope == "followed" && self.assets.is_empty()
        {
            return Err(ServiceError::invalid_params(
                "无效的推送范围、币种或合并间隔",
            ));
        }
        Ok(())
    }
    pub fn accepts(&self, level: Level, assets: &[String]) -> bool {
        self.enabled
            && (self.scope != "urgent" || level == Level::Urgent)
            && (self.scope != "followed" || assets.iter().any(|a| self.assets.contains(a)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows() {
        let n = OffsetDateTime::now_utc();
        let a = crate::format_rfc3339(n - time::Duration::hours(6));
        assert!(!in_window(Level::Urgent, None, Some(&a), n));
        assert!(!in_window(Level::Urgent, None, None, n));
        assert!(!in_window(
            Level::Important,
            Some(Direction::Neutral),
            Some(&crate::format_rfc3339(n)),
            n
        ));
    }
    #[test]
    fn scopes() {
        let mut p = PushSettings {
            scope: "followed".into(),
            ..Default::default()
        };
        assert!(p.validate().is_err());
        p.assets = vec!["BTC".into()];
        assert!(p.validate().is_ok());
        assert!(!p.accepts(Level::Urgent, &["ETH".into()]));
        let a = Some("2026-10-20T12:00:00Z");
        assert_eq!(
            identity("BTC: ETF!", a, None, 1),
            identity("BTC ETF", a, None, 2)
        );
        assert_ne!(
            identity("BTC ETF", a, None, 1),
            identity("ETH ETF", a, None, 2)
        );
    }
}

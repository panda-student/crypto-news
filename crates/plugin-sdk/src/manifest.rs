//! 插件清单 `plugin.toml`。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PluginError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// 只有扩展插件写，相对于插件目录
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    /// 服务名 → "主版本.次版本"
    #[serde(default)]
    pub provides: BTreeMap<String, String>,
    #[serde(default)]
    pub requires: BTreeMap<String, Requirement>,
    #[serde(default)]
    pub events: Events,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub config: ConfigSchema,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Requirement {
    Version(String),
    Detailed {
        version: String,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        optional: bool,
    },
}

impl Requirement {
    pub fn version(&self) -> &str {
        match self {
            Requirement::Version(v) => v,
            Requirement::Detailed { version, .. } => version,
        }
    }
    pub fn all(&self) -> bool {
        matches!(self, Requirement::Detailed { all: true, .. })
    }
    pub fn optional(&self) -> bool {
        matches!(self, Requirement::Detailed { optional: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    #[serde(default)]
    pub publish: Vec<String>,
    #[serde(default)]
    pub subscribe: Vec<String>,
    #[serde(default)]
    pub intercept: Vec<String>,
    /// 本插件发出的高频事件：不写日志，只投递给在线订阅者
    #[serde(default)]
    pub ephemeral: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ConfigSchema {
    #[serde(default)]
    pub fields: BTreeMap<String, ConfigField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    /// string / integer / boolean / string_list / secret
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<toml::Value>,
    #[serde(default)]
    pub help: Option<String>,
}

pub const PERMISSIONS: &[&str] = &["network", "store"];

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, PluginError> {
        let m: Manifest =
            toml::from_str(text).map_err(|e| PluginError::new(format!("清单格式错误：{e}")))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), PluginError> {
        if !valid_name(&self.name) {
            return Err(PluginError::new(format!(
                "插件名只能用小写字母、数字、连字符：{}",
                self.name
            )));
        }
        for p in &self.permissions {
            if !PERMISSIONS.contains(&p.as_str()) {
                return Err(PluginError::new(format!("未知权限：{p}")));
            }
        }
        for (svc, v) in &self.provides {
            parse_version(v).ok_or_else(|| {
                PluginError::new(format!("服务 {svc} 的版本应为 主版本.次版本：{v}"))
            })?;
        }
        for (svc, r) in &self.requires {
            parse_version(r.version()).ok_or_else(|| {
                PluginError::new(format!("依赖 {svc} 的版本格式错误：{}", r.version()))
            })?;
        }
        for e in &self.events.ephemeral {
            if !self.events.publish.contains(e) {
                return Err(PluginError::new(format!(
                    "ephemeral 事件 {e} 不在 publish 里"
                )));
            }
        }
        Ok(())
    }
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// "1" → (1, 0)，"1.2" → (1, 2)。
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut it = s.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = match it.next() {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    if it.next().is_some() {
        return None;
    }
    Some((major, minor))
}

/// 主版本相等，提供者的次版本不低于要求的次版本。
pub fn version_satisfies(provided: &str, required: &str) -> bool {
    match (parse_version(provided), parse_version(required)) {
        (Some((pm, pn)), Some((rm, rn))) => pm == rm && pn >= rn,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name = "collect"
version = "0.1.0"
permissions = ["network"]
tables = ["sources"]

[provides]
item-query = "1.0"

[requires]
store = "1"
source = { version = "1", all = true }
hello = { version = "1.2", optional = true }

[events]
publish = ["item.collected"]
subscribe = ["timer.tick"]

[config.fields.contact_email]
type = "string"
label = "SEC 联系邮箱"
required = true
"#;

    #[test]
    fn parses_sample() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.name, "collect");
        assert!(m.requires["source"].all());
        assert!(m.requires["hello"].optional());
        assert_eq!(m.requires["store"].version(), "1");
        assert!(m.config.fields["contact_email"].required);
    }

    #[test]
    fn rejects_bad_name_and_permission() {
        assert!(Manifest::parse("name='Bad'\nversion='1'").is_err());
        assert!(Manifest::parse("name='a'\nversion='1'\npermissions=['root']").is_err());
    }

    #[test]
    fn version_rules() {
        assert!(version_satisfies("1.0", "1"));
        assert!(version_satisfies("1.3", "1.2"));
        assert!(!version_satisfies("1.1", "1.2"));
        assert!(!version_satisfies("2.0", "1"));
        assert!(!version_satisfies("x", "1"));
    }
}

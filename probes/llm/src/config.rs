use std::path::Path;

use serde::Deserialize;

const DEFAULT_PROXY: &str = "http://127.0.0.1:7892";
pub const KEY_ENV: &str = "PROBE_LLM_API_KEY";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("找不到配置文件 {0}")]
    Missing(String),
    #[error("读取配置文件失败: {0}")]
    Io(String),
    #[error("配置文件格式错误: {0}")]
    Parse(String),
    #[error("配置项 {0} 未填写")]
    Empty(&'static str),
}

#[derive(Deserialize)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_proxy")]
    pub proxy: String,
}

fn default_proxy() -> String {
    DEFAULT_PROXY.to_string()
}

impl Config {
    /// 读取配置；环境变量 PROBE_LLM_API_KEY 非空时覆盖文件里的 api_key。
    pub fn load(path: &Path, env_key: Option<String>) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::Missing(path.display().to_string()))
            }
            Err(e) => return Err(ConfigError::Io(e.to_string())),
        };
        Config::parse(&text, env_key)
    }

    pub fn parse(text: &str, env_key: Option<String>) -> Result<Config, ConfigError> {
        let mut cfg: Config =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if let Some(k) = env_key.filter(|k| !k.trim().is_empty()) {
            cfg.api_key = k;
        }
        cfg.api_key = cfg.api_key.trim().to_string();
        if cfg.base_url.trim().is_empty() {
            return Err(ConfigError::Empty("base_url"));
        }
        if cfg.model.trim().is_empty() {
            return Err(ConfigError::Empty("model"));
        }
        if cfg.api_key.is_empty() {
            return Err(ConfigError::Empty(
                "api_key（或环境变量 PROBE_LLM_API_KEY）",
            ));
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_proxy_and_env_override() {
        let c = Config::parse(
            "base_url='u'\nmodel='m'\napi_key='a'",
            Some("envkey".into()),
        )
        .unwrap();
        assert_eq!(c.proxy, DEFAULT_PROXY);
        assert_eq!(c.api_key, "envkey");
    }

    #[test]
    fn empty_key_rejected() {
        let e = Config::parse("base_url='u'\nmodel='m'\napi_key=''", None)
            .err()
            .unwrap();
        assert!(matches!(e, ConfigError::Empty(_)));
    }
}

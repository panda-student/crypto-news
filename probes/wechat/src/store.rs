//! 配置与本地状态：登录凭证、会话凭证（context token）、轮询游标。
//! 这些文件都属于密钥，只落在 `state/` 目录，不打印。

use std::fs;
use std::path::{Path, PathBuf};

use ilink::{Context, Credentials};
use serde::{Deserialize, Serialize};

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub const DEFAULT_PROXY: &str = "http://127.0.0.1:7892";
/// 二维码登录的服务主机（真实模式）。ilink 库不写死任何地址，由这里提供。
pub const DEFAULT_QR_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// 各文件的位置。mock 模式的状态放在 `state/mock/`，与真实凭证隔开。
#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub state: PathBuf,
}

impl Paths {
    pub fn new(dir: PathBuf, mock: bool) -> Self {
        let mut state = dir.join("state");
        if mock {
            state.push("mock");
        }
        Self { dir, state }
    }
    pub fn config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
    pub fn credentials(&self) -> PathBuf {
        self.state.join("credentials.json")
    }
    pub fn context(&self) -> PathBuf {
        self.state.join("context.json")
    }
    pub fn cursor(&self) -> PathBuf {
        self.state.join("cursor.txt")
    }
    pub fn log(&self) -> PathBuf {
        self.state.join("log.jsonl")
    }
    pub fn qr_png(&self) -> PathBuf {
        self.state.join("qr.png")
    }
    pub fn mock_quota(&self) -> PathBuf {
        self.state.join("mock-quota.json")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub proxy: String,
    pub qr_base_url: String,
    pub push_count: usize,
    pub push_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: DEFAULT_PROXY.to_string(),
            qr_base_url: DEFAULT_QR_BASE_URL.to_string(),
            push_count: 12,
            push_interval_secs: 20,
        }
    }
}

impl Config {
    /// 文件不存在时用默认值（代理为默认代理）。
    pub fn load(path: &Path) -> Res<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Config = toml::from_str(&text)?;
                if cfg.proxy.trim().is_empty() {
                    return Err("config.toml 的 proxy 不能为空：所有请求必须走代理".into());
                }
                if cfg.qr_base_url.trim().is_empty() {
                    return Err("config.toml 的 qr_base_url 不能为空".into());
                }
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
}

/// 最近一次收到用户消息时保存的会话凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRecord {
    pub context: Context,
    /// 本程序收到消息的时间（Unix 毫秒，UTC）。
    pub received_at_ms: i64,
}

fn write_secret(path: &Path, bytes: &[u8]) -> Res<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Res<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn save_credentials(paths: &Paths, creds: &Credentials) -> Res<()> {
    write_secret(&paths.credentials(), &serde_json::to_vec_pretty(creds)?)
}

pub fn load_credentials(paths: &Paths) -> Res<Option<Credentials>> {
    read_json(&paths.credentials())
}

pub fn save_context(paths: &Paths, record: &ContextRecord) -> Res<()> {
    write_secret(&paths.context(), &serde_json::to_vec_pretty(record)?)
}

pub fn load_context(paths: &Paths) -> Res<Option<ContextRecord>> {
    read_json(&paths.context())
}

pub fn save_cursor(paths: &Paths, cursor: &str) -> Res<()> {
    write_secret(&paths.cursor(), cursor.as_bytes())
}

pub fn load_cursor(paths: &Paths) -> Option<String> {
    fs::read_to_string(paths.cursor())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_creds() -> Credentials {
        Credentials {
            token: "tok-secret-1".into(),
            base_url: "https://example.invalid".into(),
            account_id: "acc".into(),
            user_id: "uid".into(),
            saved_at: None,
        }
    }

    #[test]
    fn credentials_roundtrip_in_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf(), false);
        assert!(load_credentials(&paths).unwrap().is_none());
        save_credentials(&paths, &sample_creds()).unwrap();
        let back = load_credentials(&paths).unwrap().unwrap();
        assert_eq!(back.token, "tok-secret-1");
        assert_eq!(back.account_id, "acc");
    }

    #[test]
    fn context_and_cursor_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf(), false);
        let rec = ContextRecord {
            context: Context {
                account_key: "a".into(),
                user_id: "u".into(),
                context_token: "ctx-secret".into(),
                observed_at_unix_ms: 5,
                source_message_id: None,
            },
            received_at_ms: 1234,
        };
        save_context(&paths, &rec).unwrap();
        let back = load_context(&paths).unwrap().unwrap();
        assert_eq!(back.context.context_token, "ctx-secret");
        assert_eq!(back.received_at_ms, 1234);

        assert!(load_cursor(&paths).is_none());
        save_cursor(&paths, "cur-1").unwrap();
        assert_eq!(load_cursor(&paths).as_deref(), Some("cur-1"));
    }

    #[test]
    fn mock_state_is_separate() {
        let real = Paths::new(PathBuf::from("x"), false);
        let mock = Paths::new(PathBuf::from("x"), true);
        assert_ne!(real.credentials(), mock.credentials());
        assert!(mock.credentials().to_string_lossy().contains("mock"));
    }

    #[test]
    fn config_defaults_and_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap().proxy, DEFAULT_PROXY);
        fs::write(&path, "push_count = 3\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!((cfg.push_count, cfg.push_interval_secs), (3, 20));
        fs::write(&path, "proxy = \"\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}

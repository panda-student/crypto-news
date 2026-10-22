//! 主程序：`app serve` 读配置、打开数据库、组装 hub、登记官方插件、加载扩展插件。

use std::collections::BTreeMap;
use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hub::{Hub, HubOptions};
use serde_json::Value;
use tracing_subscriber::filter::LevelFilter;
mod mcp;

const USAGE: &str = "用法：\n  app serve [--data-dir <路径>]\n  app mcp [--data-dir <路径>]\n  app plugin <名字>   （暂不支持）";

const DEFAULT_CONFIG: &str = r#"# 消息面分析系统配置

# 监听地址，只允许回环地址（127.0.0.1 / ::1），否则拒绝启动
listen = "127.0.0.1:8730"

# 代理地址，必填；所有外部请求都走它
proxy = "http://127.0.0.1:7892"

# 日志级别：trace / debug / info / warn / error
log_level = "info"

# 插件配置示例：enabled 缺省为 true，其余键原样交给该插件
# [plugins.rss]
# enabled = true

# 模型服务，默认 DeepSeek（OpenAI 兼容接口）。API 密钥在网页"设置"页填写，不要写进配置文件
[plugins.llm]
provider = "openai"              # openai 或 anthropic
base_url = "https://api.deepseek.com/v1"
small_model = "deepseek-flash"
large_model = "deepseek-v4-pro"  # 留空则同小模型

# 推送调度
# [plugins.dispatch]
# digest_time = "08:00"          # 日报时间
# quiet_hours = ""               # 免打扰时段，留空不启用
# utc_offset = "+08:00"          # 本地时区偏移

# 企业微信机器人。webhook 地址在网页"密钥"页填写，不要写进配置文件
# [plugins.wecom-webhook]
# backup = true                  # 作为备用渠道
"#;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("配置错误：{0}")]
    Config(String),
    #[error("{0}")]
    Io(String),
    #[error("已有 app serve 在运行（数据目录：{0}）")]
    Locked(String),
    #[error("{0}")]
    Start(String),
}

/// 命令行。
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve { data_dir: Option<PathBuf> },
    Mcp { data_dir: Option<PathBuf> },
    Plugin(String),
    Usage,
}

pub fn parse_args(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("serve" | "mcp") => {
            let mut data_dir = None;
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--data-dir" => match it.next() {
                        Some(p) => data_dir = Some(PathBuf::from(p)),
                        None => return Command::Usage,
                    },
                    _ => return Command::Usage,
                }
            }
            if args[0] == "mcp" {
                Command::Mcp { data_dir }
            } else {
                Command::Serve { data_dir }
            }
        }
        Some("plugin") if args.len() == 2 => Command::Plugin(args[1].clone()),
        _ => Command::Usage,
    }
}

/// 解析后的配置。
#[derive(Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub proxy: String,
    pub log_level: LevelFilter,
    pub event_retention_days: i64,
    /// 插件名 → 是否启用
    pub enabled: BTreeMap<String, bool>,
    /// 插件名 → 配置（已去掉 enabled，web 已注入 listen）
    pub plugin_configs: BTreeMap<String, Value>,
}

impl Config {
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.get(name).copied().unwrap_or(true)
    }
}

const KNOWN_KEYS: &[&str] = &[
    "listen",
    "proxy",
    "log_level",
    "event_retention_days",
    "plugins",
];

pub fn parse_config(text: &str) -> Result<Config, AppError> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| AppError::Config(e.message().to_string()))?;
    for k in table.keys() {
        if !KNOWN_KEYS.contains(&k.as_str()) {
            tracing::warn!("config.toml 中有未知顶层键，已忽略：{k}");
        }
    }
    let listen_str = match table.get("listen") {
        Some(toml::Value::String(s)) => s.clone(),
        Some(_) => return Err(AppError::Config("listen 必须是字符串".into())),
        None => return Err(AppError::Config("缺少 listen".into())),
    };
    let listen: SocketAddr = listen_str
        .parse()
        .map_err(|_| AppError::Config(format!("listen 不是有效地址：{listen_str}")))?;
    if !listen.ip().is_loopback() {
        return Err(AppError::Config(format!(
            "listen 必须是回环地址，当前为 {listen}"
        )));
    }
    let proxy = match table.get("proxy") {
        Some(toml::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(AppError::Config("proxy 必填且不能为空".into())),
    };
    let log_level = match table.get("log_level") {
        None => LevelFilter::INFO,
        Some(toml::Value::String(s)) => match s.as_str() {
            "trace" => LevelFilter::TRACE,
            "debug" => LevelFilter::DEBUG,
            "info" => LevelFilter::INFO,
            "warn" => LevelFilter::WARN,
            "error" => LevelFilter::ERROR,
            other => {
                return Err(AppError::Config(format!(
                    "log_level 取值无效：{other}（可选 trace/debug/info/warn/error）"
                )))
            }
        },
        Some(_) => return Err(AppError::Config("log_level 必须是字符串".into())),
    };
    let event_retention_days = match table.get("event_retention_days") {
        None => 7,
        Some(toml::Value::Integer(n)) if *n >= 1 => *n,
        Some(_) => {
            return Err(AppError::Config(
                "event_retention_days 必须是不小于 1 的整数".into(),
            ))
        }
    };

    let mut enabled = BTreeMap::new();
    let mut plugin_configs = BTreeMap::new();
    match table.get("plugins") {
        None => {}
        Some(toml::Value::Table(plugins)) => {
            for (name, v) in plugins {
                let toml::Value::Table(t) = v else {
                    return Err(AppError::Config(format!("[plugins.{name}] 必须是表")));
                };
                let mut t = t.clone();
                match t.remove("enabled") {
                    None => {}
                    Some(toml::Value::Boolean(b)) => {
                        enabled.insert(name.clone(), b);
                    }
                    Some(_) => {
                        return Err(AppError::Config(format!(
                            "[plugins.{name}] 的 enabled 必须是布尔值"
                        )))
                    }
                }
                let json = serde_json::to_value(toml::Value::Table(t))
                    .map_err(|e| AppError::Config(format!("[plugins.{name}]：{e}")))?;
                plugin_configs.insert(name.clone(), json);
            }
        }
        Some(_) => return Err(AppError::Config("plugins 必须是表".into())),
    }
    // 顶层 listen 优先于 web 自己的 listen
    let web = plugin_configs
        .entry("web".to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(m) = web {
        m.insert("listen".into(), Value::String(listen.to_string()));
    }
    Ok(Config {
        listen,
        proxy,
        log_level,
        event_retention_days,
        enabled,
        plugin_configs,
    })
}

/// 默认数据目录。
pub fn default_data_dir() -> Result<PathBuf, AppError> {
    #[cfg(windows)]
    let p = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("crypto-news"));
    #[cfg(not(windows))]
    let p = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".crypto-news"));
    p.ok_or_else(|| AppError::Io("无法确定数据目录，请用 --data-dir 指定".into()))
}

/// 创建数据目录和 `plugins/`，返回配置文本（不存在则写默认配置）。
pub fn prepare_data_dir(dir: &Path) -> Result<String, AppError> {
    let io = |what: &str, e: std::io::Error| AppError::Io(format!("{what}：{e}"));
    std::fs::create_dir_all(dir.join("plugins")).map_err(|e| io("创建数据目录失败", e))?;
    let cfg = dir.join("config.toml");
    if !cfg.exists() {
        std::fs::write(&cfg, DEFAULT_CONFIG).map_err(|e| io("写入默认配置失败", e))?;
    }
    std::fs::read_to_string(&cfg).map_err(|e| io("读取 config.toml 失败", e))
}

/// 对数据目录加独占锁，句柄要持有到进程结束。
pub fn acquire_lock(dir: &Path) -> Result<File, AppError> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("app.lock"))
        .map_err(|e| AppError::Io(format!("打开 app.lock 失败：{e}")))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(std::fs::TryLockError::WouldBlock) => Err(AppError::Locked(dir.display().to_string())),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(AppError::Io(format!("锁定 app.lock 失败：{e}")))
        }
    }
}

/// 打开数据库并组装 hub，登记全部启用的官方插件，加载扩展插件。尚未 start。
pub async fn build_hub(cfg: &Config, data_dir: &Path) -> Result<Hub, AppError> {
    let store = store::open(&data_dir.join("data.db"))
        .await
        .map_err(|e| AppError::Start(format!("打开数据库失败：{e}")))?;
    let store: contracts::store::SharedStore = store;
    let proxy = effective_proxy(&store, &cfg.proxy).await;
    let http = plugin_sdk::build_http_client(&proxy)
        .map_err(|e| AppError::Config(format!("代理配置无效：{e}")))?;
    let hub = Hub::new(HubOptions {
        store,
        http,
        proxy,
        data_dir: data_dir.to_path_buf(),
        configs: cfg.plugin_configs.clone(),
        event_retention_days: cfg.event_retention_days,
    });
    let officials: Vec<(&str, Arc<dyn plugin_sdk::Plugin>)> = vec![
        ("auth", Arc::new(auth::AuthPlugin)),
        ("secrets", Arc::new(secrets::SecretsPlugin::default())),
        ("scheduler", Arc::new(scheduler::SchedulerPlugin)),
        ("collect", Arc::new(collect::CollectPlugin)),
        ("rss", Arc::new(rss::RssPlugin::new())),
        ("okx-ann", Arc::new(okx_ann::OkxAnnPlugin::new())),
        (
            "binance-ann",
            Arc::new(binance_ann::BinanceAnnPlugin::new()),
        ),
        ("x-browser", Arc::new(x_browser::XBrowserPlugin::new())),
        ("llm", Arc::new(llm::LlmPlugin)),
        ("rulebook", Arc::new(rulebook::RulebookPlugin)),
        ("screen", Arc::new(screen::ScreenPlugin)),
        ("dispatch", Arc::new(dispatch::DispatchPlugin)),
        ("wecom-webhook", Arc::new(wecom_webhook::WecomWebhookPlugin)),
        ("wechat-ilink", Arc::new(wechat_ilink::WechatIlinkPlugin)),
        ("agent", Arc::new(agent::AgentPlugin)),
        ("maintenance", Arc::new(maintenance::MaintenancePlugin)),
        ("web", Arc::new(web::WebPlugin)),
    ];
    for (name, p) in officials {
        if cfg.is_enabled(name) {
            hub.add_official(p)
                .map_err(|e| AppError::Start(format!("登记插件 {name} 失败：{e}")))?;
        } else {
            tracing::info!("插件 {name} 已在配置中停用");
        }
    }
    for e in hub.load_extensions(&data_dir.join("plugins")) {
        tracing::warn!("{e}");
    }
    Ok(hub)
}

/// 界面里改过的代理优先于 config.toml；改过的值无效时回退到 config.toml，避免程序起不来。
async fn effective_proxy(store: &contracts::store::SharedStore, configured: &str) -> String {
    match hub::read_app_setting(store, contracts::hub_admin::app_keys::PROXY).await {
        Ok(Some(Value::String(p))) if plugin_sdk::build_http_client(&p).is_ok() => p,
        Ok(Some(_)) => {
            tracing::warn!("界面设置的代理无效，改用 config.toml 里的代理");
            configured.to_string()
        }
        Ok(None) => configured.to_string(),
        Err(e) => {
            tracing::warn!("读取代理设置失败：{e}");
            configured.to_string()
        }
    }
}

fn init_logging(level: LevelFilter) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

/// 正在运行的服务，由 [`start`] 返回，桌面外壳可持有它并在退出时调用 [`Running::stop`]。
pub struct Running {
    /// 配置里的监听地址。
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub hub: Hub,
    lock: File,
}

impl Running {
    /// 停止 hub 并释放单实例锁。
    pub async fn stop(self) {
        self.hub.shutdown().await;
        drop(self.lock);
    }
}

/// 本机界面地址。
pub fn web_url(listen: SocketAddr) -> String {
    match listen {
        SocketAddr::V4(a) => format!("http://{}:{}/", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("http://[{}]:{}/", a.ip(), a.port()),
    }
}

/// 在本进程启动服务：准备数据目录、加锁、组装并启动 hub。
pub async fn start(data_dir: Option<PathBuf>) -> Result<Running, AppError> {
    let dir = match data_dir {
        Some(d) => d,
        None => default_data_dir()?,
    };
    let text = prepare_data_dir(&dir)?;
    let cfg = parse_config(&text)?;
    init_logging(cfg.log_level);
    let lock = acquire_lock(&dir)?;
    let hub = build_hub(&cfg, &dir).await?;
    hub.start()
        .await
        .map_err(|e| AppError::Start(format!("hub 启动失败：{e}")))?;
    tracing::info!("app serve 已启动，数据目录 {}", dir.display());
    Ok(Running {
        listen: cfg.listen,
        data_dir: dir,
        hub,
        lock,
    })
}

async fn serve(data_dir: Option<PathBuf>) -> Result<(), AppError> {
    let running = start(data_dir).await?;
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!("监听 Ctrl+C 失败：{e}");
    }
    tracing::info!("正在停止");
    running.stop().await;
    Ok(())
}

/// 程序入口逻辑，返回进程退出码。
pub async fn run(args: &[String]) -> i32 {
    match parse_args(args) {
        Command::Mcp { data_dir } => mcp::run(data_dir).await,
        Command::Serve { data_dir } => match serve(data_dir).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        Command::Plugin(_) => {
            eprintln!("暂不支持 app plugin");
            2
        }
        Command::Usage => {
            eprintln!("{USAGE}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn default_config_parses() {
        let c = parse_config(DEFAULT_CONFIG).unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:8730");
        assert_eq!(c.proxy, "http://127.0.0.1:7892");
        assert_eq!(c.log_level, LevelFilter::INFO);
        assert_eq!(c.event_retention_days, 7);
        assert_eq!(c.plugin_configs["web"]["listen"], "127.0.0.1:8730");
    }

    #[test]
    fn default_config_prefills_deepseek_without_key() {
        let c = parse_config(DEFAULT_CONFIG).unwrap();
        let llm = &c.plugin_configs["llm"];
        assert_eq!(llm["provider"], "openai");
        assert_eq!(llm["base_url"], "https://api.deepseek.com/v1");
        assert_eq!(llm["small_model"], "deepseek-flash");
        assert_eq!(llm["large_model"], "deepseek-v4-pro");
        assert!(llm.get("api_key").is_none());
    }

    #[test]
    fn default_config_enables_all_new_plugins() {
        let c = parse_config(DEFAULT_CONFIG).unwrap();
        for n in [
            "secrets",
            "llm",
            "rulebook",
            "screen",
            "dispatch",
            "wecom-webhook",
            "wechat-ilink",
            "agent",
            "maintenance",
        ] {
            assert!(c.is_enabled(n), "{n} 应默认启用");
        }
    }

    #[test]
    fn web_url_formats() {
        assert_eq!(
            web_url("127.0.0.1:8730".parse().unwrap()),
            "http://127.0.0.1:8730/"
        );
        assert_eq!(web_url("[::1]:9".parse().unwrap()), "http://[::1]:9/");
    }

    #[test]
    fn rejects_non_loopback() {
        let e = parse_config("listen = \"0.0.0.0:8730\"\nproxy = \"http://x:1\"").unwrap_err();
        assert!(e.to_string().contains("回环"));
    }

    #[test]
    fn rejects_missing_proxy() {
        assert!(parse_config("listen = \"127.0.0.1:1\"").is_err());
        assert!(parse_config("listen = \"127.0.0.1:1\"\nproxy = \"  \"").is_err());
    }

    #[test]
    fn rejects_bad_log_level() {
        let t = "listen = \"127.0.0.1:1\"\nproxy = \"http://x:1\"\nlog_level = \"loud\"";
        assert!(parse_config(t).is_err());
    }

    #[test]
    fn plugin_enabled_and_configs() {
        let t = r#"
listen = "127.0.0.1:9000"
proxy = "http://x:1"
event_retention_days = 3
foo = 1
[plugins.rss]
enabled = false
interval = 60
[plugins.web]
listen = "127.0.0.1:1111"
title = "t"
"#;
        let c = parse_config(t).unwrap();
        assert!(!c.is_enabled("rss"));
        assert!(c.is_enabled("collect"));
        assert_eq!(c.event_retention_days, 3);
        assert_eq!(c.plugin_configs["rss"], serde_json::json!({"interval": 60}));
        assert_eq!(c.plugin_configs["web"]["listen"], "127.0.0.1:9000");
        assert_eq!(c.plugin_configs["web"]["title"], "t");
    }

    #[test]
    fn args_parse() {
        assert_eq!(
            parse_args(&s(&["serve"])),
            Command::Serve { data_dir: None }
        );
        assert_eq!(
            parse_args(&s(&["serve", "--data-dir", "X"])),
            Command::Serve {
                data_dir: Some(PathBuf::from("X"))
            }
        );
        assert_eq!(
            parse_args(&s(&["plugin", "foo"])),
            Command::Plugin("foo".into())
        );
        assert_eq!(parse_args(&[]), Command::Usage);
        assert_eq!(parse_args(&s(&["serve", "--data-dir"])), Command::Usage);
        assert_eq!(parse_args(&s(&["bogus"])), Command::Usage);
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("app-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire_lock(&dir).unwrap();
        assert!(matches!(acquire_lock(&dir), Err(AppError::Locked(_))));
        drop(first);
        assert!(acquire_lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_writes_default_config() {
        let dir = std::env::temp_dir().join(format!("app-prep-{}", std::process::id()));
        let text = prepare_data_dir(&dir).unwrap();
        assert!(dir.join("plugins").is_dir());
        assert!(parse_config(&text).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

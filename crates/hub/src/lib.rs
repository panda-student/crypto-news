//! hub：插件框架。负责加载插件、按依赖顺序启停、失败退避重启、路由服务调用、
//! 持久化并投递事件、管理扩展插件进程。hub 自己提供 `store` 和 `hub-admin` 两个服务。

mod db;
mod events;
mod ext;
mod link;
mod supervise;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use contracts::hub_admin::PluginInfo;
use contracts::store::SharedStore;
use contracts::{ServiceError, ServiceHandler};
use plugin_sdk::{EventFn, InterceptFn, Manifest, Plugin, StopFn};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// hub 自己作为调用方和服务提供方时用的名字。
pub const HUB: &str = "hub";

/// hub 自己的表，插件不能声明。
const HUB_TABLES: &[&str] = &[
    "plugins",
    "event_log",
    "event_cursors",
    "config_overrides",
    "app_settings",
];

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("插件清单错误：{0}")]
    Manifest(String),
    #[error("插件重名：{0}")]
    Duplicate(String),
    #[error("存储错误：{0}")]
    Store(String),
    #[error("hub 已停止")]
    Stopped,
}

pub struct HubOptions {
    pub store: SharedStore,
    /// 官方插件共用的 HTTP 客户端，必须带代理
    pub http: reqwest::Client,
    /// 代理地址，注入扩展插件进程
    pub proxy: String,
    /// 数据目录，官方插件的私有目录在 `<data_dir>/plugin-data/<名字>`
    pub data_dir: PathBuf,
    /// 插件名 → 配置
    pub configs: BTreeMap<String, Value>,
    /// 事件日志保留天数
    pub event_retention_days: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Official,
    Extension,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Official => "official",
            Kind::Extension => "extension",
        }
    }
}

#[derive(Clone)]
pub(crate) enum Runner {
    Official(Arc<dyn Plugin>),
    Extension { dir: PathBuf, exe: PathBuf },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Stopped,
    Starting,
    Running,
}

/// 插件运行期间的资源，停止时统一清理。
#[derive(Default)]
pub(crate) struct Running {
    tasks: Vec<tokio::task::AbortHandle>,
    stops: Vec<StopFn>,
    ext: Option<ext::ExtHandle>,
}

pub(crate) struct Entry {
    manifest: Manifest,
    kind: Kind,
    runner: Runner,
    phase: Phase,
    /// 每次启动加一，旧上下文的登记和失败报告按它作废
    generation: u64,
    missing: Vec<String>,
    last_error: Option<String>,
    granted: bool,
    enabled: bool,
    failures: u32,
    retry_at: Option<Instant>,
    started_at: Option<Instant>,
    run: Option<Running>,
}

impl Entry {
    fn status(&self) -> &'static str {
        if !self.enabled {
            // 停用后要等监督循环真正停掉插件，期间如实显示
            if self.phase == Phase::Stopped {
                "已停用"
            } else {
                "停止中"
            }
        } else if !self.granted {
            "待授权"
        } else {
            match self.phase {
                Phase::Running => "运行中",
                Phase::Starting => "启动中",
                Phase::Stopped if self.retry_at.is_some() => "失败退避",
                Phase::Stopped if !self.missing.is_empty() => "缺依赖",
                Phase::Stopped => "已停止",
            }
        }
    }

    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            kind: self.kind.as_str().to_string(),
            status: self.status().to_string(),
            missing: self.missing.clone(),
            permissions: self.manifest.permissions.clone(),
            granted: self.granted,
            enabled: self.enabled,
            last_error: self.last_error.clone(),
            config_fields: serde_json::to_value(&self.manifest.config.fields)
                .unwrap_or(Value::Null),
            description: self.manifest.description.clone(),
        }
    }

    /// 权限的持久化形式：排序后的 JSON 数组。
    fn permissions_json(&self) -> String {
        let mut p = self.manifest.permissions.clone();
        p.sort();
        p.dedup();
        serde_json::to_string(&p).unwrap_or_default()
    }
}

type Providers = BTreeMap<String, Arc<dyn ServiceHandler>>;

#[derive(Default)]
pub(crate) struct State {
    plugins: BTreeMap<String, Entry>,
    /// 服务名 → 提供者插件名 → 处理器（只有运行中的插件）
    services: HashMap<String, Providers>,
    /// (事件种类, 订阅者)
    subscribers: BTreeMap<(String, String), EventFn>,
    /// (事件种类, 拦截者)，按插件名顺序调用
    interceptors: BTreeMap<(String, String), InterceptFn>,
    stopped: bool,
}

impl State {
    /// 清除插件登记的服务、订阅和拦截。
    fn unregister(&mut self, name: &str) {
        for ps in self.services.values_mut() {
            ps.remove(name);
        }
        self.services.retain(|_, ps| !ps.is_empty());
        self.subscribers.retain(|(_, p), _| p != name);
        self.interceptors.retain(|(_, p), _| p != name);
    }
}

pub(crate) enum Cmd {
    Reconcile(Option<oneshot::Sender<()>>),
    Failed {
        name: String,
        generation: u64,
        error: String,
    },
    /// 停止后按当前配置重新启动
    Restart(String),
    Shutdown(oneshot::Sender<()>),
}

pub(crate) struct Inner {
    store: SharedStore,
    http: reqwest::Client,
    proxy: String,
    data_dir: PathBuf,
    configs: BTreeMap<String, Value>,
    /// 界面改过的配置，覆盖 configs
    overrides: Mutex<BTreeMap<String, serde_json::Map<String, Value>>>,
    /// 外壳写入的只读运行时信息（app_keys::SHELL），不持久化
    runtime: Mutex<BTreeMap<String, Value>>,
    retention_days: i64,
    state: Mutex<State>,
    notifies: Mutex<HashMap<String, Arc<Notify>>>,
    cmd: mpsc::UnboundedSender<Cmd>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<Cmd>>>,
    background: Mutex<Vec<JoinHandle<()>>>,
}

impl Inner {
    pub(crate) fn send(&self, cmd: Cmd) {
        let _ = self.cmd.send(cmd);
    }

    pub(crate) fn notify_for(&self, kind: &str) -> Arc<Notify> {
        self.notifies
            .lock()
            .unwrap()
            .entry(kind.to_string())
            .or_default()
            .clone()
    }

    /// 是否有插件把该事件声明为 ephemeral。
    pub(crate) fn is_ephemeral(&self, kind: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.plugins
            .values()
            .any(|e| e.manifest.events.ephemeral.iter().any(|k| k == kind))
    }

    /// 插件的配置：顶层配置里该插件的部分，再叠加界面改过的值，缺省值由清单补上。
    pub(crate) fn config_for(&self, manifest: &Manifest) -> Value {
        let mut cfg = match self.configs.get(&manifest.name) {
            Some(Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if let Some(ov) = self.overrides.lock().unwrap().get(&manifest.name) {
            cfg.extend(ov.clone());
        }
        for (k, field) in &manifest.config.fields {
            if cfg.contains_key(k) {
                continue;
            }
            if let Some(d) = &field.default {
                if let Ok(v) = serde_json::to_value(d) {
                    cfg.insert(k.clone(), v);
                }
            }
        }
        Value::Object(cfg)
    }
}

/// hub 句柄，可以克隆。
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new(opts: HubOptions) -> Hub {
        let (tx, rx) = mpsc::unbounded_channel();
        Hub {
            inner: Arc::new(Inner {
                store: opts.store,
                http: opts.http,
                proxy: opts.proxy,
                data_dir: opts.data_dir,
                configs: opts.configs,
                overrides: Mutex::new(BTreeMap::new()),
                runtime: Mutex::new(BTreeMap::new()),
                retention_days: opts.event_retention_days.max(1),
                state: Mutex::new(State::default()),
                notifies: Mutex::new(HashMap::new()),
                cmd: tx,
                cmd_rx: Mutex::new(Some(rx)),
                background: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 登记官方插件。官方插件默认已授权。
    pub fn add_official(&self, plugin: Arc<dyn Plugin>) -> Result<(), HubError> {
        let m = Manifest::parse(plugin.manifest()).map_err(|e| HubError::Manifest(e.0))?;
        self.insert(m, Kind::Official, Runner::Official(plugin))
    }

    /// 扫描扩展插件目录：每个子目录一个插件，带 `plugin.toml`。
    /// 返回加载失败的说明（也写进日志），不影响其他插件。
    pub fn load_extensions(&self, dir: &Path) -> Vec<String> {
        let mut errors = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return errors;
        };
        let mut dirs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for d in dirs {
            if let Err(e) = self.load_extension(&d) {
                let msg = format!("扩展插件 {} 加载失败：{e}", d.display());
                tracing::warn!("{msg}");
                errors.push(msg);
            }
        }
        errors
    }

    fn load_extension(&self, dir: &Path) -> Result<(), HubError> {
        let text = std::fs::read_to_string(dir.join("plugin.toml"))
            .map_err(|e| HubError::Manifest(format!("读取 plugin.toml 失败：{e}")))?;
        let m = Manifest::parse(&text).map_err(|e| HubError::Manifest(e.0))?;
        let entry = m
            .entry
            .clone()
            .ok_or_else(|| HubError::Manifest("扩展插件必须写 entry".into()))?;
        let exe = dir.join(&entry);
        if !exe.is_file() {
            return Err(HubError::Manifest(format!("找不到可执行文件 {entry}")));
        }
        self.insert(
            m,
            Kind::Extension,
            Runner::Extension {
                dir: dir.to_path_buf(),
                exe,
            },
        )
    }

    fn insert(&self, m: Manifest, kind: Kind, runner: Runner) -> Result<(), HubError> {
        let mut st = self.inner.state.lock().unwrap();
        if m.name == HUB || st.plugins.contains_key(&m.name) {
            return Err(HubError::Duplicate(m.name));
        }
        for t in &m.tables {
            if HUB_TABLES.contains(&t.as_str()) {
                return Err(HubError::Manifest(format!("表名 {t} 保留给 hub")));
            }
            if let Some(other) = st.plugins.values().find(|e| e.manifest.tables.contains(t)) {
                return Err(HubError::Manifest(format!(
                    "表 {t} 已属于插件 {}",
                    other.manifest.name
                )));
            }
        }
        for svc in m.provides.keys() {
            if svc == contracts::store::SERVICE || svc == contracts::hub_admin::SERVICE {
                return Err(HubError::Manifest(format!("服务 {svc} 由 hub 提供")));
            }
        }
        st.plugins.insert(
            m.name.clone(),
            Entry {
                manifest: m,
                kind,
                runner,
                phase: Phase::Stopped,
                generation: 0,
                missing: Vec::new(),
                last_error: None,
                granted: kind == Kind::Official,
                enabled: true,
                failures: 0,
                retry_at: None,
                started_at: None,
                run: None,
            },
        );
        Ok(())
    }

    /// 建表、读取授权与启停状态，然后按依赖顺序启动插件。返回时首轮启动已完成。
    pub async fn start(&self) -> Result<(), HubError> {
        let rx = self
            .inner
            .cmd_rx
            .lock()
            .unwrap()
            .take()
            .ok_or(HubError::Stopped)?;
        db::migrate(&self.inner).await?;
        db::load(&self.inner).await?;
        db::load_overrides(&self.inner).await?;
        db::init_cursors(&self.inner).await?;
        let sup = tokio::spawn(supervise::run(self.inner.clone(), rx));
        let clean = tokio::spawn(events::cleanup_loop(self.inner.clone()));
        self.inner.background.lock().unwrap().extend([sup, clean]);
        let (tx, done) = oneshot::channel();
        self.inner.send(Cmd::Reconcile(Some(tx)));
        done.await.map_err(|_| HubError::Stopped)
    }

    /// 按依赖反序停止全部插件。
    pub async fn shutdown(&self) {
        let (tx, done) = oneshot::channel();
        self.inner.send(Cmd::Shutdown(tx));
        let _ = done.await;
        for t in std::mem::take(&mut *self.inner.background.lock().unwrap()) {
            t.abort();
        }
    }

    /// 以 hub 的身份调用服务。
    pub async fn call(
        &self,
        service: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        link::call_as(&self.inner, HUB, service, None, method, params).await
    }

    /// 以 hub 的身份发出事件。
    pub async fn publish(&self, kind: &str, payload: Value) -> Result<i64, ServiceError> {
        events::publish(&self.inner, HUB, kind, payload, None)
            .await
            .map_err(|e| ServiceError::internal(e.0))
    }

    /// 外壳写入只读的运行时信息（如 `app_keys::SHELL`），经 `app_get` 可读，不持久化。
    pub fn set_runtime(&self, key: &str, value: Value) {
        self.inner
            .runtime
            .lock()
            .unwrap()
            .insert(key.to_string(), value);
    }

    /// 全部插件的状态。
    pub fn plugins(&self) -> Vec<PluginInfo> {
        let st = self.inner.state.lock().unwrap();
        st.plugins.values().map(Entry::info).collect()
    }
}

/// 启动 hub 之前读应用级设置（比如代理）。没设过时返回 None。
pub async fn read_app_setting(store: &SharedStore, key: &str) -> Result<Option<Value>, HubError> {
    db::read_app_setting(store, key).await
}

/// 去掉日志里可能是密钥的内容：长度不少于 24 的连续字母数字串替换为 `***`。
pub(crate) fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= 24 {
            out.push_str("***");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests;

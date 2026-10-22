//! X 采集插件：用本机浏览器和用户自己的 X 登录态抓关注账号的时间线。
//! 提供 `source` 1.0 与 `x-admin` 1.0。

mod accounts;
mod driver;
pub mod find;
pub mod parse;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use plugin_sdk::contracts::hub_admin::{app_keys, HubAdminClient};
use plugin_sdk::contracts::source::SERVICE as SOURCE_SERVICE;
use plugin_sdk::contracts::source::{FetchParams, FetchResult, SourceInfo, SourceList};
use plugin_sdk::contracts::store::StoreClient;
use plugin_sdk::contracts::x::{XAccount, XAccountList, XStatus, ADMIN_SERVICE};
use plugin_sdk::contracts::{parse_params, to_result};
use plugin_sdk::{BoxFuture, Caller, Context, Plugin, PluginError, ServiceError, ServiceHandler};
use serde::Deserialize;
use serde_json::{json, Value};

pub use accounts::{normalize_handle, DEFAULT_HANDLES};

const MANIFEST: &str = include_str!("../plugin.toml");
const DEFAULT_INTERVAL: u64 = 600;
const LOGIN_HINT: &str = "X 未登录：请在 设置 → 信源 → X 里点“登录 X”";
pub const NO_BROWSER: &str = "未找到 Chrome / Edge / Chromium，请安装其中一个后重启";

/// 浏览器后端出的错误。说明里不得带 Cookie 等敏感内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    NotLoggedIn,
    NoBrowser,
    Failed(String),
}

/// 浏览器后端：真实实现驱动本机浏览器，测试用假的。
pub trait Backend: Send + Sync + 'static {
    /// 找到的浏览器名称。
    fn browser_name(&self) -> Option<String>;
    /// 后端已知的登录状态；未知时返回 None。
    fn logged_in(&self) -> BoxFuture<'_, Option<bool>>;
    fn login_window_open(&self) -> bool;
    /// 关掉无界面浏览器，打开有界面的登录窗口。
    fn open_login(&self) -> BoxFuture<'_, Result<(), BackendError>>;
    fn open_home(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        self.open_login()
    }
    /// 打开账号页，返回渲染后的 HTML。
    fn fetch_profile<'a>(&'a self, handle: &'a str) -> BoxFuture<'a, Result<String, BackendError>>;
    /// 关掉闲置过久的无界面浏览器。
    fn reap_idle(&self) -> BoxFuture<'_, ()>;
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

#[derive(Deserialize, Default)]
struct Config {
    interval_secs: Option<u64>,
}

pub struct XBrowserPlugin {
    backend: Option<Arc<dyn Backend>>,
    gap_ms: (u64, u64),
}

impl XBrowserPlugin {
    pub fn new() -> Self {
        XBrowserPlugin {
            backend: None,
            gap_ms: (3000, 5000),
        }
    }

    /// 测试用：换掉浏览器后端，并指定账号之间的间隔范围（毫秒）。
    pub fn with_backend(backend: Arc<dyn Backend>, gap_ms: (u64, u64)) -> Self {
        XBrowserPlugin {
            backend: Some(backend),
            gap_ms,
        }
    }
}

impl Default for XBrowserPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for XBrowserPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: Config = ctx.config_as()?;
            let store = ctx.store();
            store.migrate(1, accounts::SCHEMA).await?;
            accounts::seed(&store, &DEFAULT_HANDLES).await?;
            let backend: Arc<dyn Backend> = match &self.backend {
                Some(b) => b.clone(),
                None => {
                    let proxy = HubAdminClient(ctx.service("hub-admin"))
                        .app_get(app_keys::PROXY)
                        .await?;
                    let proxy = proxy.as_str().unwrap_or("").trim().to_string();
                    Arc::new(driver::Chrome::new(
                        find::detect(),
                        ctx.data_dir().join("profile"),
                        proxy,
                    ))
                }
            };
            let core = Arc::new(Core {
                store,
                backend: backend.clone(),
                interval_secs: cfg.interval_secs.unwrap_or(DEFAULT_INTERVAL).max(1),
                last_error: StdMutex::new(None),
                serial: tokio::sync::Mutex::new(None),
                gap_ms: self.gap_ms,
            });
            ctx.provide(SOURCE_SERVICE, Arc::new(SourceFacade(core.clone())))?;
            ctx.provide(ADMIN_SERVICE, Arc::new(AdminFacade(core)))?;
            let reaper = backend.clone();
            ctx.spawn("x-browser-idle", async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    reaper.reap_idle().await;
                }
            });
            ctx.on_stop(move || async move { backend.shutdown().await });
            Ok(())
        })
    }
}

struct Core {
    store: StoreClient,
    backend: Arc<dyn Backend>,
    interval_secs: u64,
    last_error: StdMutex<Option<String>>,
    /// 串行抓取，记录上一次抓取结束的时间
    serial: tokio::sync::Mutex<Option<Instant>>,
    gap_ms: (u64, u64),
}

fn map_error(e: BackendError) -> ServiceError {
    match e {
        BackendError::NotLoggedIn => ServiceError::credential_expired(LOGIN_HINT),
        BackendError::NoBrowser => ServiceError::unavailable(NO_BROWSER),
        BackendError::Failed(m) => ServiceError::unavailable(m),
    }
}

impl Core {
    fn set_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = msg;
        }
    }

    async fn set_logged_in(&self, v: bool) {
        if let Err(e) = accounts::meta_set(
            &self.store,
            accounts::LOGGED_IN_KEY,
            if v { "1" } else { "0" },
        )
        .await
        {
            tracing::warn!("保存 X 登录状态失败：{}", e.message);
        }
    }

    async fn logged_in(&self) -> bool {
        let stored = accounts::meta_get(&self.store, accounts::LOGGED_IN_KEY)
            .await
            .ok()
            .flatten()
            .is_some_and(|v| v == "1");
        match self.backend.logged_in().await {
            Some(v) => {
                if v != stored {
                    self.set_logged_in(v).await;
                }
                v
            }
            None => stored,
        }
    }

    async fn status(&self) -> XStatus {
        let browser = self.backend.browser_name();
        let last_error = if browser.is_none() {
            Some(NO_BROWSER.to_string())
        } else {
            self.last_error.lock().ok().and_then(|g| g.clone())
        };
        XStatus {
            logged_in: self.logged_in().await,
            login_window_open: self.backend.login_window_open(),
            browser,
            last_error,
        }
    }

    async fn login(&self) -> Result<XStatus, ServiceError> {
        if self.backend.browser_name().is_none() {
            return Err(ServiceError::unavailable(NO_BROWSER));
        }
        match self.backend.open_login().await {
            Ok(()) => self.set_error(None),
            Err(e) => {
                let err = map_error(e);
                self.set_error(Some(err.message.clone()));
                return Err(err);
            }
        }
        Ok(self.status().await)
    }

    async fn list(&self) -> Result<SourceList, ServiceError> {
        if self.backend.browser_name().is_none() {
            return Ok(SourceList { sources: vec![] });
        }
        Ok(SourceList {
            sources: accounts::list(&self.store)
                .await?
                .into_iter()
                .filter(|a| !a.paused)
                .map(|a| SourceInfo {
                    id: format!("x:{}", a.handle),
                    name: format!("X @{}", a.display),
                    interval_secs: self.interval_secs,
                })
                .collect(),
        })
    }

    async fn fetch(&self, p: FetchParams) -> Result<FetchResult, ServiceError> {
        let handle = p.id.strip_prefix("x:").unwrap_or(&p.id);
        let account = accounts::list(&self.store)
            .await?
            .into_iter()
            .find(|a| a.handle == handle.to_ascii_lowercase() && !a.paused);
        let Some(account) = account else {
            return Ok(FetchResult {
                items: vec![],
                cursor: p.cursor,
            });
        };
        if self.backend.browser_name().is_none() {
            return Err(ServiceError::unavailable(NO_BROWSER));
        }
        let result = self.fetch_account(&account.display, p.cursor).await;
        match &result {
            Ok(_) => self.set_error(None),
            Err(e) => self.set_error(Some(e.message.clone())),
        }
        result
    }

    async fn fetch_account(
        &self,
        display: &str,
        cursor: Option<String>,
    ) -> Result<FetchResult, ServiceError> {
        // 账号之间串行，且至少间隔 3～5 秒
        let mut last = self.serial.lock().await;
        if let Some(t) = *last {
            let nanos = u64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos()),
            );
            let (lo, hi) = self.gap_ms;
            let gap = Duration::from_millis(lo + nanos % (hi.saturating_sub(lo) + 1));
            if let Some(wait) = gap.checked_sub(t.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
        let html = self.backend.fetch_profile(display).await;
        *last = Some(Instant::now());
        drop(last);
        let html = match html {
            Ok(h) => h,
            Err(BackendError::NotLoggedIn) => {
                self.set_logged_in(false).await;
                return Err(map_error(BackendError::NotLoggedIn));
            }
            Err(e) => return Err(map_error(e)),
        };
        let tweets = parse::parse_timeline(&html);
        if tweets.is_empty() {
            if parse::looks_logged_out(&html) {
                self.set_logged_in(false).await;
                return Err(map_error(BackendError::NotLoggedIn));
            }
            if parse::looks_missing_account(&html) {
                return Err(ServiceError::unavailable(format!(
                    "X 账号 @{display} 不存在或已被封禁"
                )));
            }
            return Err(ServiceError::internal(format!(
                "X 账号 @{display} 没有找到推文：账号可能没有公开推文，或页面结构变了"
            )));
        }
        let (items, cursor) = parse::build_items(&tweets, cursor.as_deref());
        Ok(FetchResult { items, cursor })
    }

    async fn admin(&self, method: &str, params: Value) -> Result<Value, ServiceError> {
        #[derive(Deserialize)]
        struct H {
            handle: String,
        }
        #[derive(Deserialize)]
        struct P {
            handle: String,
            paused: bool,
        }
        fn view(a: &accounts::Account) -> XAccount {
            XAccount {
                handle: a.display.clone(),
                paused: a.paused,
                builtin: a.builtin,
            }
        }
        match method {
            "status" => to_result(&self.status().await),
            "open" => {
                self.backend.open_home().await.map_err(map_error)?;
                to_result(&self.status().await)
            }
            "login" => to_result(&self.login().await?),
            "list" => to_result(&XAccountList {
                accounts: accounts::list(&self.store)
                    .await?
                    .iter()
                    .map(view)
                    .collect(),
            }),
            "add" => {
                let p: H = parse_params(params)?;
                to_result(&view(&accounts::add(&self.store, &p.handle).await?))
            }
            "delete" => {
                let p: H = parse_params(params)?;
                accounts::delete(&self.store, &p.handle).await?;
                Ok(json!({ "ok": true }))
            }
            "set_paused" => {
                let p: P = parse_params(params)?;
                let a = accounts::set_paused(&self.store, &p.handle, p.paused).await?;
                to_result(&view(&a))
            }
            _ => Err(ServiceError::method_not_found(method)),
        }
    }
}

struct SourceFacade(Arc<Core>);
struct AdminFacade(Arc<Core>);

impl ServiceHandler for SourceFacade {
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            match method {
                "list" => to_result(&self.0.list().await?),
                "fetch" => to_result(&self.0.fetch(parse_params(params)?).await?),
                _ => Err(ServiceError::method_not_found(method)),
            }
        })
    }
}

impl ServiceHandler for AdminFacade {
    fn call<'a>(
        &'a self,
        _caller: Caller,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move { self.0.admin(method, params).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::contracts::source::SourceClient;
    use plugin_sdk::contracts::x::XAdminClient;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use std::sync::atomic::{AtomicBool, Ordering};

    const PAGE: &str = include_str!("../tests/profile.html");

    struct Fake {
        name: Option<String>,
        logged: StdMutex<Option<bool>>,
        page: StdMutex<Result<String, BackendError>>,
        login_calls: StdMutex<u32>,
        shut: AtomicBool,
    }

    impl Fake {
        fn new(name: Option<&str>, page: Result<String, BackendError>) -> Arc<Self> {
            Arc::new(Fake {
                name: name.map(str::to_string),
                logged: StdMutex::new(None),
                page: StdMutex::new(page),
                login_calls: StdMutex::new(0),
                shut: AtomicBool::new(false),
            })
        }
    }

    impl Backend for Fake {
        fn browser_name(&self) -> Option<String> {
            self.name.clone()
        }
        fn logged_in(&self) -> BoxFuture<'_, Option<bool>> {
            Box::pin(async move { *self.logged.lock().unwrap() })
        }
        fn login_window_open(&self) -> bool {
            false
        }
        fn open_login(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            Box::pin(async move {
                *self.login_calls.lock().unwrap() += 1;
                *self.logged.lock().unwrap() = Some(true);
                Ok(())
            })
        }
        fn fetch_profile<'a>(
            &'a self,
            _handle: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            Box::pin(async move { self.page.lock().unwrap().clone() })
        }
        fn reap_idle(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move { self.shut.store(true, Ordering::SeqCst) })
        }
    }

    async fn start(fake: Arc<Fake>) -> (Arc<TestHub>, SourceClient, XAdminClient) {
        let hub = TestHub::new();
        hub.with_store(store::open_memory().await.unwrap());
        let plugin = XBrowserPlugin::with_backend(fake, (0, 0));
        plugin
            .start(hub.context("x-browser", json!({}), test_http_client()))
            .await
            .unwrap();
        let caller = hub.context("caller", json!({}), test_http_client());
        (
            hub,
            SourceClient(caller.service("source")),
            XAdminClient(caller.service(ADMIN_SERVICE)),
        )
    }

    #[test]
    fn manifest_is_valid() {
        let m = plugin_sdk::Manifest::parse(MANIFEST).unwrap();
        assert_eq!(m.name, "x-browser");
    }

    #[tokio::test]
    async fn lists_default_accounts_and_skips_paused() {
        let (_h, src, admin) = start(Fake::new(Some("Edge"), Ok(PAGE.into()))).await;
        let list = src.list().await.unwrap();
        assert_eq!(list.len(), 9);
        assert!(list
            .iter()
            .any(|s| s.id == "x:vitalikbuterin" && s.name == "X @VitalikButerin"));
        assert!(list.iter().all(|s| s.interval_secs == 600));
        admin.set_paused("@Binance", true).await.unwrap();
        let list = src.list().await.unwrap();
        assert_eq!(list.len(), 8);
        assert!(!list.iter().any(|s| s.id == "x:binance"));
        // 暂停的账号不抓取
        let r = src.fetch("x:binance", Some("5".into())).await.unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.cursor.as_deref(), Some("5"));
    }

    #[tokio::test]
    async fn no_browser_lists_nothing_and_reports_error() {
        let (_h, src, admin) = start(Fake::new(None, Ok(PAGE.into()))).await;
        assert!(src.list().await.unwrap().is_empty());
        let st = admin.status().await.unwrap();
        assert!(st.browser.is_none());
        assert_eq!(st.last_error.as_deref(), Some(NO_BROWSER));
        assert!(admin.login().await.is_err());
        assert_eq!(admin.list().await.unwrap().len(), 9);
    }

    #[tokio::test]
    async fn fetch_returns_items_and_cursor() {
        let (_h, src, _a) = start(Fake::new(Some("Chrome"), Ok(PAGE.into()))).await;
        let r = src.fetch("x:binance", None).await.unwrap();
        assert_eq!(r.items.len(), 3);
        assert_eq!(r.cursor.as_deref(), Some("1700000000000000300"));
        let r = src.fetch("x:binance", r.cursor).await.unwrap();
        assert!(r.items.is_empty());
        assert_eq!(r.cursor.as_deref(), Some("1700000000000000300"));
    }

    #[tokio::test]
    async fn not_logged_in_is_credential_expired() {
        let fake = Fake::new(Some("Chrome"), Err(BackendError::NotLoggedIn));
        let (_h, src, admin) = start(fake).await;
        let e = src.fetch("x:binance", None).await.unwrap_err();
        assert!(e.message.contains("X 未登录"), "{}", e.message);
        let st = admin.status().await.unwrap();
        assert!(!st.logged_in);
        assert!(st.last_error.unwrap().contains("X 未登录"));
    }

    #[tokio::test]
    async fn empty_page_gives_clear_errors() {
        let (_h, src, _a) = start(Fake::new(
            Some("Chrome"),
            Ok("<html><body>nothing</body></html>".into()),
        ))
        .await;
        let e = src.fetch("x:binance", None).await.unwrap_err();
        assert!(e.message.contains("没有找到推文"), "{}", e.message);
        let (_h, src, _a) = start(Fake::new(
            Some("Chrome"),
            Ok(r#"<a data-testid="loginButton">Log in</a>"#.into()),
        ))
        .await;
        let e = src.fetch("x:binance", None).await.unwrap_err();
        assert!(e.message.contains("X 未登录"), "{}", e.message);
    }

    #[tokio::test]
    async fn login_updates_state() {
        let fake = Fake::new(Some("Chrome"), Ok(PAGE.into()));
        let (_h, _s, admin) = start(fake.clone()).await;
        assert!(!admin.status().await.unwrap().logged_in);
        let st = admin.login().await.unwrap();
        assert!(st.logged_in);
        assert_eq!(*fake.login_calls.lock().unwrap(), 1);
        let a = admin.add("@New_One").await.unwrap();
        assert_eq!(a.handle, "New_One");
        admin.delete("new_one").await.unwrap();
        assert!(admin.add("bad-name").await.is_err());
    }
}

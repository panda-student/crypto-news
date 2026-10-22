//! 真实的浏览器后端：用 chromiumoxide 驱动本机的 Chrome / Edge / Chromium。
//!
//! 无界面浏览器和登录用的有界面浏览器共用同一个配置目录，所以任何时刻最多一个在运行。
//! 日志和错误里不出现 Cookie 的值，只看 `auth_token` 这个名字是否存在。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use futures_util::StreamExt;
use plugin_sdk::BoxFuture;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::find::Found;
use crate::parse::{looks_logged_out, looks_missing_account};
use crate::{Backend, BackendError};

const IDLE_CLOSE: Duration = Duration::from_secs(120);
const PAGE_WAIT: Duration = Duration::from_secs(25);
const LOGIN_WATCH_MAX: Duration = Duration::from_secs(15 * 60);
const TWEET_SELECTOR: &str = r#"article[data-testid="tweet"]"#;
const AUTH_COOKIE: &str = "auth_token";

/// 登录状态：0 未知，1 已登录，2 未登录。
const UNKNOWN: u8 = 0;
const YES: u8 = 1;
const NO: u8 = 2;

struct Live {
    browser: Browser,
    task: JoinHandle<()>,
    child: tokio::process::Child,
    last_used: Instant,
}

impl Live {
    async fn shut(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.browser.close()).await;
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        self.task.abort();
    }
}

pub struct Chrome {
    exe: Option<Found>,
    profile: PathBuf,
    proxy: String,
    headless: Mutex<Option<Live>>,
    login_open: Arc<AtomicBool>,
    logged: Arc<AtomicU8>,
    cancel: Arc<Notify>,
}

impl Chrome {
    pub fn new(exe: Option<Found>, profile: PathBuf, proxy: String) -> Self {
        Chrome {
            exe,
            profile,
            proxy,
            headless: Mutex::new(None),
            login_open: Arc::new(AtomicBool::new(false)),
            logged: Arc::new(AtomicU8::new(UNKNOWN)),
            cancel: Arc::new(Notify::new()),
        }
    }

    fn args(&self, headed: bool, port: u16) -> Result<Vec<String>, BackendError> {
        if self.proxy.contains('@') {
            return Err(BackendError::Failed(
                "代理地址不能带账号密码：浏览器采集不支持".into(),
            ));
        }
        std::fs::create_dir_all(&self.profile)
            .map_err(|e| BackendError::Failed(format!("无法创建浏览器配置目录：{e}")))?;
        let mut args = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", self.profile.display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--password-store=basic".to_string(),
            "--use-mock-keychain".to_string(),
            "--disable-blink-features=AutomationControlled".to_string(),
            "--lang=en-US".to_string(),
            "--disable-background-timer-throttling".to_string(),
            "--disable-renderer-backgrounding".to_string(),
        ];
        if !self.proxy.is_empty() {
            args.push(format!("--proxy-server={}", self.proxy));
        }
        if !headed {
            args.extend([
                "--headless=new".to_string(),
                "--hide-scrollbars".to_string(),
                "--mute-audio".to_string(),
                "--window-size=1280,1600".to_string(),
            ]);
        }
        Ok(args)
    }

    /// 自己启动浏览器进程，再通过调试端口连上去。
    async fn launch(&self, headed: bool) -> Result<Live, BackendError> {
        let exe = self.exe.as_ref().ok_or(BackendError::NoBrowser)?;
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| BackendError::Failed(format!("找不到可用端口：{e}")))?
            .port();
        let args = self.args(headed, port)?;
        let mut child = tokio::process::Command::new(&exe.path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| BackendError::Failed(format!("启动浏览器失败：{e}")))?;
        let url = format!("http://127.0.0.1:{port}");
        let mut last_err = String::new();
        for _ in 0..40 {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(BackendError::Failed(format!(
                    "浏览器启动后立即退出（{status}）：配置目录可能正被其他浏览器占用"
                )));
            }
            match Browser::connect(url.as_str()).await {
                Ok((browser, mut handler)) => {
                    let task = tokio::spawn(async move {
                        while let Some(r) = handler.next().await {
                            // 个别 CDP 消息解析失败不影响连接；连接关闭时流会结束
                            let _ = r;
                        }
                    });
                    // 等浏览器把初始页面建好
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    return Ok(Live {
                        browser,
                        task,
                        child,
                        last_used: Instant::now(),
                    });
                }
                Err(e) => last_err = e.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let _ = child.kill().await;
        Err(BackendError::Failed(format!("连接浏览器失败：{last_err}")))
    }

    /// 浏览器里是否有登录 Cookie（只看名字和值是否非空）。
    async fn has_auth(browser: &Browser) -> Result<bool, BackendError> {
        let cookies = browser
            .get_cookies()
            .await
            .map_err(|_| BackendError::Failed("读取浏览器登录状态失败".into()))?;
        Ok(cookies
            .iter()
            .any(|c| c.name == AUTH_COOKIE && !c.value.is_empty()))
    }

    async fn fetch_once(&self, live: &mut Live, handle: &str) -> Result<String, BackendError> {
        let has_auth = Self::has_auth(&live.browser).await?;
        self.logged
            .store(if has_auth { YES } else { NO }, Ordering::SeqCst);
        if !has_auth {
            return Err(BackendError::NotLoggedIn);
        }
        let url = format!("https://x.com/{handle}");
        let page = live
            .browser
            .new_page(url.as_str())
            .await
            .map_err(|e| BackendError::Failed(format!("打开页面失败：{e}")))?;
        let start = Instant::now();
        let mut html = String::new();
        let mut result = Err(BackendError::Failed("等待页面超时".into()));
        while start.elapsed() < PAGE_WAIT {
            if let Ok(u) = page.url().await {
                let u = u.unwrap_or_default();
                if u.contains("/login") || u.contains("/i/flow/") {
                    result = Err(BackendError::NotLoggedIn);
                    break;
                }
            }
            if page.find_element(TWEET_SELECTOR).await.is_ok() {
                // 等懒加载的推文多渲染一会儿
                tokio::time::sleep(Duration::from_millis(1500)).await;
                html = page.content().await.unwrap_or_default();
                result = Ok(());
                break;
            }
            if let Ok(c) = page.content().await {
                if looks_missing_account(&c) || looks_logged_out(&c) {
                    html = c;
                    result = Ok(());
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if result.is_err() && html.is_empty() {
            html = page.content().await.unwrap_or_default();
        }
        let _ = page.close().await;
        match result {
            Ok(()) => Ok(html),
            // 超时但页面上有内容：交给解析层判断（可能是没有推文的账号）
            Err(BackendError::Failed(_)) if !html.is_empty() => Ok(html),
            Err(e) => Err(e),
        }
    }
}

impl Backend for Chrome {
    fn browser_name(&self) -> Option<String> {
        self.exe.as_ref().map(|e| e.name.to_string())
    }

    fn logged_in(&self) -> BoxFuture<'_, Option<bool>> {
        Box::pin(async move {
            match self.logged.load(Ordering::SeqCst) {
                YES => Some(true),
                NO => Some(false),
                _ => None,
            }
        })
    }

    fn login_window_open(&self) -> bool {
        self.login_open.load(Ordering::SeqCst)
    }

    fn open_login(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            // 持有锁期间别的抓取进不来
            let mut guard = self.headless.lock().await;
            if self.login_open.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(live) = guard.take() {
                live.shut().await;
            }
            let live = self.launch(true).await?;
            if let Err(e) = live.browser.new_page("https://x.com/login").await {
                live.shut().await;
                return Err(BackendError::Failed(format!("打开登录页失败：{e}")));
            }
            self.login_open.store(true, Ordering::SeqCst);
            let open = self.login_open.clone();
            let logged = self.logged.clone();
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                let start = Instant::now();
                let mut ok = false;
                while start.elapsed() < LOGIN_WATCH_MAX {
                    tokio::select! {
                        () = cancel.notified() => break,
                        () = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                    match Chrome::has_auth(&live.browser).await {
                        Ok(true) => {
                            ok = true;
                            break;
                        }
                        Ok(false) => {}
                        // 读不到：窗口被关了或浏览器退出
                        Err(_) => break,
                    }
                }
                if ok {
                    // 让浏览器把 Cookie 写进配置目录
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    logged.store(YES, Ordering::SeqCst);
                }
                live.shut().await;
                open.store(false, Ordering::SeqCst);
            });
            Ok(())
        })
    }

    fn open_home(&self) -> BoxFuture<'_, Result<(), BackendError>> {
        Box::pin(async move {
            let mut guard = self.headless.lock().await;
            if self.login_open.load(Ordering::SeqCst) {
                return Err(BackendError::Failed(
                    "浏览器窗口已打开，请切换到该窗口。".into(),
                ));
            }
            if let Some(live) = guard.take() {
                live.shut().await;
            }
            let live = self.launch(true).await?;
            if live.browser.new_page("https://x.com/home").await.is_err() {
                live.shut().await;
                return Err(BackendError::Failed("打开X首页失败".into()));
            }
            self.login_open.store(true, Ordering::SeqCst);
            let open = self.login_open.clone();
            let logged = self.logged.clone();
            let cancel = self.cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! { ()=cancel.notified()=>break, ()=tokio::time::sleep(Duration::from_secs(2))=>{} }
                    match Chrome::has_auth(&live.browser).await {
                        Ok(on) => logged.store(if on { YES } else { NO }, Ordering::SeqCst),
                        Err(_) => break,
                    }
                }
                live.shut().await;
                open.store(false, Ordering::SeqCst);
            });
            Ok(())
        })
    }

    fn fetch_profile<'a>(&'a self, handle: &'a str) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            if self.login_open.load(Ordering::SeqCst) {
                return Err(BackendError::Failed(
                    "登录窗口还开着：登录完成或关闭窗口后再抓取".into(),
                ));
            }
            let mut guard = self.headless.lock().await;
            for attempt in 0..2 {
                if guard.is_none() {
                    *guard = Some(self.launch(false).await?);
                }
                let Some(live) = guard.as_mut() else { break };
                live.last_used = Instant::now();
                let r = self.fetch_once(live, handle).await;
                live.last_used = Instant::now();
                match r {
                    Err(BackendError::Failed(m))
                        if attempt == 0 && m.starts_with("打开页面失败") =>
                    {
                        // 浏览器可能已经挂了：关掉重启一次
                        if let Some(l) = guard.take() {
                            l.shut().await;
                        }
                    }
                    other => return other,
                }
            }
            Err(BackendError::Failed("浏览器反复启动失败".into()))
        })
    }

    fn reap_idle(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Ok(mut guard) = self.headless.try_lock() else {
                return;
            };
            if guard
                .as_ref()
                .is_some_and(|l| l.last_used.elapsed() >= IDLE_CLOSE)
            {
                if let Some(l) = guard.take() {
                    l.shut().await;
                }
            }
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.cancel.notify_waiters();
            let mut guard = self.headless.lock().await;
            if let Some(l) = guard.take() {
                l.shut().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动验证本机浏览器能启动并读到页面：`cargo test -p x-browser -- --ignored`
    /// （已知：在部分机器上由本进程拉起的浏览器 CDP new_page 会超时，见任务报告。）
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要本机安装 Chrome / Edge / Chromium"]
    async fn launches_real_browser() {
        let exe = crate::find::detect().expect("本机没有浏览器");
        let dir = std::env::temp_dir().join(format!("xbt-{}", std::process::id()));
        let c = Chrome::new(Some(exe), dir, String::new());
        let live = c.launch(false).await.unwrap();
        let t = std::time::Duration::from_secs(15);
        let page = tokio::time::timeout(t, live.browser.new_page("about:blank"))
            .await
            .expect("new_page 超时")
            .unwrap();
        tokio::time::timeout(t, page.goto("data:text/html,<p>hi</p>"))
            .await
            .expect("goto 超时")
            .unwrap();
        let html = tokio::time::timeout(t, page.content())
            .await
            .expect("content 超时")
            .unwrap();
        assert!(html.contains("hi"));
        assert!(!Chrome::has_auth(&live.browser).await.unwrap());
        live.shut().await;
    }
}

//! Signed GitHub releases, automatic checks and explicit installation.
use contracts::hub_admin::{self, app_keys};
use hub::Hub;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::runtime::Runtime;

fn status(hub: &Hub, state: &str, message: &str, version: Option<&str>) {
    hub.set_runtime(app_keys::UPDATE_STATUS,json!({"state":state,"message":message,"version":version,"current_version":env!("CARGO_PKG_VERSION")}));
}
async fn read(hub: &Hub, key: &str) -> Value {
    hub.call(hub_admin::SERVICE, "app_get", json!({"key":key}))
        .await
        .ok()
        .and_then(|v| v.get("value").cloned())
        .unwrap_or(Value::Null)
}
async fn check(handle: &AppHandle, hub: &Hub) -> Result<Option<Update>, String> {
    let proxy = read(hub, app_keys::PROXY).await;
    let mut builder = handle.updater_builder().timeout(Duration::from_secs(30));
    if let Some(proxy) = proxy.as_str().filter(|s| !s.is_empty()) {
        builder = builder.proxy(proxy.parse().map_err(|_| "代理地址无效")?)
    } else {
        builder = builder.no_proxy()
    }
    let updater = builder.build().map_err(|_| "无法初始化更新检查")?;
    updater
        .check()
        .await
        .map_err(|_| "更新检查失败，请检查网络后重试。".into())
}
pub fn spawn(handle: AppHandle, hub: Hub, rt: Arc<Runtime>) {
    status(&hub, "idle", "等待检查更新", None);
    std::thread::spawn(move || {
        let previous = rt.block_on(read(&hub, app_keys::UPDATE_ACTION));
        let mut last_request = previous["request_id"].as_str().unwrap_or("").to_string();
        let mut last_check = Instant::now() - Duration::from_secs(6 * 3600);
        let mut available: Option<Update> = None;
        loop {
            if handle
                .state::<crate::Core>()
                .running
                .lock()
                .unwrap()
                .is_none()
            {
                break;
            }
            let action = rt.block_on(read(&hub, app_keys::UPDATE_ACTION));
            let request = action["request_id"].as_str().unwrap_or("");
            let explicit = !request.is_empty() && request != last_request;
            let auto = rt
                .block_on(read(&hub, app_keys::AUTO_UPDATE))
                .as_bool()
                .unwrap_or(true);
            if explicit {
                last_request = request.into();
            }
            if explicit && action["kind"] == "install" {
                if let Some(update) = available.take() {
                    status(
                        &hub,
                        "downloading",
                        "正在下载并校验更新签名",
                        Some(&update.version),
                    );
                    let download = rt.block_on(update.download(|_, _| {}, || {}));
                    match download {
                        Ok(bytes) => {
                            status(
                                &hub,
                                "installing",
                                "正在安装更新，应用即将重新启动",
                                Some(&update.version),
                            );
                            let running =
                                handle.state::<crate::Core>().running.lock().unwrap().take();
                            if let Some(running) = running {
                                rt.block_on(running.stop());
                            }
                            if update.install(bytes).is_err() {
                                crate::notify(
                                    &handle,
                                    "更新安装失败，将重新启动当前版本。请稍后重试。",
                                );
                                handle.restart();
                            }
                            return;
                        }
                        Err(_) => {
                            status(
                                &hub,
                                "error",
                                "下载或签名校验失败，未安装更新。",
                                Some(&update.version),
                            );
                            available = Some(update);
                        }
                    }
                } else {
                    status(&hub, "error", "没有可安装的更新，请先检查更新。", None)
                }
            } else if explicit && action["kind"] == "check"
                || auto && last_check.elapsed() >= Duration::from_secs(6 * 3600)
            {
                status(&hub, "checking", "正在检查更新", None);
                last_check = Instant::now();
                match rt.block_on(check(&handle, &hub)) {
                    Ok(Some(update)) => {
                        status(
                            &hub,
                            "available",
                            "发现新版本，可确认安装",
                            Some(&update.version),
                        );
                        available = Some(update);
                    }
                    Ok(None) => {
                        status(&hub, "current", "当前已是最新版本", None);
                        available = None;
                    }
                    Err(message) => status(&hub, "error", &message, None),
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

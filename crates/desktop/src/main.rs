//! 桌面外壳：在本进程内启动核心服务，窗口加载本机网页界面。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod logic;
mod updates;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use contracts::hub_admin::{self, app_keys};
use hub::Hub;
use serde_json::{json, Value};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewWindow, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;
use tokio::runtime::Runtime;

use logic::Nav;

/// 核心服务的运行时和句柄；运行时由独立线程创建并长期持有。
#[derive(Default)]
struct Core {
    rt: OnceLock<Arc<Runtime>>,
    running: Mutex<Option<app::Running>>,
}

#[derive(Default)]
struct HiddenNotified(AtomicBool);

/// 设置里"关闭窗口时缩到托盘"的缓存值（缺省 true），避免在界面线程上调用核心。
struct CloseToTray(AtomicBool);

/// 托盘里的"开机自启"勾选项，用于跟随设置同步。
struct TrayAuto(CheckMenuItem<Wry>);

fn bool_of(v: &Value) -> Option<bool> {
    v.as_bool()
}

/// 读一个应用级设置（布尔）；失败或未设置返回 `None`。
fn read_bool(rt: &Runtime, hub: &Hub, key: &str) -> Option<bool> {
    let r = rt.block_on(hub.call(hub_admin::SERVICE, "app_get", json!({ "key": key })));
    r.ok().and_then(|v| v.get("value").and_then(bool_of))
}

fn write_bool(rt: &Runtime, hub: &Hub, key: &str, value: bool) {
    let r = rt.block_on(hub.call(
        hub_admin::SERVICE,
        "app_set",
        json!({ "key": key, "value": value }),
    ));
    if let Err(e) = r {
        tracing::warn!("写入设置 {key} 失败：{}", e.message);
    }
}

/// 当前核心的 hub 句柄和运行时（核心未启动或已停止时为 `None`）。
fn hub_and_rt(handle: &AppHandle) -> Option<(Hub, Arc<Runtime>)> {
    let core = handle.state::<Core>();
    let hub = core
        .running
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.hub.clone())?;
    Some((hub, core.rt.get()?.clone()))
}

fn system_autostart(handle: &AppHandle) -> bool {
    handle.autolaunch().is_enabled().unwrap_or(false)
}

/// 核心启动后：标记外壳、必要时用系统状态播种设置，然后每 2 秒让系统状态跟随设置。
fn spawn_settings_sync(handle: AppHandle, hub: Hub, rt: Arc<Runtime>) {
    hub.set_runtime(app_keys::SHELL, json!("desktop"));
    std::thread::spawn(move || loop {
        let auto = read_bool(&rt, &hub, app_keys::AUTOSTART);
        if let Some(v) = logic::autostart_seed(auto, system_autostart(&handle)) {
            write_bool(&rt, &hub, app_keys::AUTOSTART, v);
        }
        let ctt = read_bool(&rt, &hub, app_keys::CLOSE_TO_TRAY);
        handle
            .state::<CloseToTray>()
            .0
            .store(logic::close_to_tray_value(ctt), Ordering::SeqCst);
        let al = handle.autolaunch();
        let res = match logic::autostart_action(auto, system_autostart(&handle)) {
            logic::AutostartAction::Nothing => Ok(()),
            logic::AutostartAction::Enable => al.enable(),
            logic::AutostartAction::Disable => al.disable(),
        };
        if let Err(e) = res {
            tracing::warn!("应用开机自启设置失败：{e}");
        }
        let _ = handle
            .state::<TrayAuto>()
            .0
            .set_checked(system_autostart(&handle));
        std::thread::sleep(std::time::Duration::from_secs(2));
        // 核心已停止（退出中）就结束
        if handle.state::<Core>().running.lock().unwrap().is_none() {
            break;
        }
    });
}

fn notify(handle: &AppHandle, body: &str) {
    if let Err(e) = handle
        .notification()
        .builder()
        .title("消息面分析")
        .body(body)
        .show()
    {
        tracing::warn!("发送系统通知失败：{e}");
    }
}

fn show_main(handle: &AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// 已有独立 `app serve` 在跑时，读同一数据目录的配置取监听地址。
fn locked_listen() -> Result<std::net::SocketAddr, app::AppError> {
    let dir = app::default_data_dir()?;
    let text = app::prepare_data_dir(&dir)?;
    Ok(app::parse_config(&text)?.listen)
}

fn apply_nav(handle: &AppHandle, window: &WebviewWindow, nav: Nav) {
    match nav {
        Nav::Url(u) => match u.parse::<Url>() {
            Ok(url) => {
                if let Err(e) = window.navigate(url) {
                    tracing::warn!("导航失败：{e}");
                }
            }
            Err(e) => tracing::warn!("界面地址无效：{e}"),
        },
        Nav::Error(msg) => {
            let _ = window.eval(logic::error_script(&msg));
            notify(handle, &format!("启动失败：{msg}"));
            // 失败时窗口即使是 --hidden 启动也要让用户看到
            show_main(handle);
        }
    }
}

/// 在独立线程的多线程运行时里启动核心服务。
fn spawn_core(handle: AppHandle) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => Arc::new(rt),
            Err(e) => {
                tracing::error!("创建运行时失败：{e}");
                return;
            }
        };
        let core = handle.state::<Core>();
        let _ = core.rt.set(rt.clone());
        let result = rt.block_on(app::start(None));
        let nav = logic::nav_for(&result, |r| r.listen, locked_listen);
        if let Ok(running) = result {
            let hub = running.hub.clone();
            *core.running.lock().unwrap() = Some(running);
            spawn_settings_sync(handle.clone(), hub.clone(), rt.clone());
            updates::spawn(handle.clone(), hub, rt.clone());
        }
        if let Some(w) = handle.get_webview_window("main") {
            apply_nav(&handle, &w, nav);
        }
    });
}

/// 停止核心服务（等待完成）后退出进程。
fn quit(handle: &AppHandle) {
    let handle = handle.clone();
    std::thread::spawn(move || {
        let core = handle.state::<Core>();
        let running = core.running.lock().unwrap().take();
        if let (Some(running), Some(rt)) = (running, core.rt.get()) {
            rt.block_on(running.stop());
        }
        handle.exit(0);
    });
}

fn setup_tray(handle: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(handle, "open", "打开界面", true, None::<&str>)?;
    let enabled = system_autostart(handle);
    let auto =
        CheckMenuItem::with_id(handle, "autostart", "开机自启", true, enabled, None::<&str>)?;
    let exit = MenuItem::with_id(handle, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(handle, &[&open, &auto, &exit])?;
    handle.manage(TrayAuto(auto.clone()));

    let mut builder = TrayIconBuilder::new()
        .tooltip("消息面分析")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event({
            let auto = auto.clone();
            move |h, event| match event.id.as_ref() {
                "open" => show_main(h),
                "autostart" => {
                    // 先写设置再改系统，避免同步线程把刚切换的状态改回去
                    let target = !system_autostart(h);
                    if let Some((hub, rt)) = hub_and_rt(h) {
                        let h = h.clone();
                        std::thread::spawn(move || {
                            write_bool(&rt, &hub, app_keys::AUTOSTART, target);
                            let al = h.autolaunch();
                            let r = if target { al.enable() } else { al.disable() };
                            if let Err(e) = r {
                                tracing::warn!("切换开机自启失败：{e}");
                            }
                        });
                    } else {
                        let al = h.autolaunch();
                        let r = if target { al.enable() } else { al.disable() };
                        if let Err(e) = r {
                            tracing::warn!("切换开机自启失败：{e}");
                        }
                    }
                    let _ = auto.set_checked(target);
                }
                "quit" => quit(h),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = handle.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(handle)?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "mcp") {
        let rt = tokio::runtime::Runtime::new().expect("创建MCP运行时");
        std::process::exit(rt.block_on(app::run(&args)));
    }
    let hidden = logic::has_hidden(std::env::args());
    let built = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|h, args, _cwd| {
            if args.iter().any(|a| a == "--shutdown") {
                quit(h);
            } else {
                show_main(h);
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Core::default())
        .manage(HiddenNotified::default())
        .manage(CloseToTray(AtomicBool::new(true)))
        .setup(move |tauri_app| {
            let handle = tauri_app.handle().clone();
            setup_tray(&handle)?;
            if let Some(w) = tauri_app.get_webview_window("main") {
                let h = handle.clone();
                let win = w.clone();
                w.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if !h.state::<CloseToTray>().0.load(Ordering::SeqCst) {
                            quit(&h);
                            return;
                        }
                        let _ = win.hide();
                        if !h.state::<HiddenNotified>().0.swap(true, Ordering::SeqCst) {
                            notify(&h, "仍在后台运行，可从托盘打开");
                        }
                    }
                });
                if !hidden {
                    let _ = w.show();
                }
            }
            spawn_core(handle);
            Ok(())
        })
        .build(tauri::generate_context!());
    match built {
        Ok(tauri_app) => tauri_app.run(|_h, event| {
            // 最后一个窗口关闭（code 为空）时不退出，退出只走托盘
            if let RunEvent::ExitRequested {
                api, code: None, ..
            } = event
            {
                api.prevent_exit();
            }
        }),
        Err(e) => {
            eprintln!("桌面外壳启动失败：{e}");
            std::process::exit(1);
        }
    }
}

//! 不依赖 Tauri 的纯逻辑：参数解析、启动结果到界面动作的映射。

use std::net::SocketAddr;

/// 启动参数里是否带 `--hidden`（开机自启时不显示窗口）。
pub fn has_hidden<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| a.as_ref() == "--hidden")
}

/// 启动核心服务后窗口要做的事。
#[derive(Debug, PartialEq, Eq)]
pub enum Nav {
    /// 导航到本机界面。
    Url(String),
    /// 在启动页显示错误，并发通知。
    Error(String),
}

/// 把 `app::start` 的结果映射成界面动作。`Locked` 时用 `locked_listen` 提供已运行实例的监听地址。
pub fn nav_for<T>(
    result: &Result<T, app::AppError>,
    listen_of: impl Fn(&T) -> SocketAddr,
    locked_listen: impl FnOnce() -> Result<SocketAddr, app::AppError>,
) -> Nav {
    match result {
        Ok(r) => Nav::Url(app::web_url(listen_of(r))),
        Err(app::AppError::Locked(_)) => match locked_listen() {
            Ok(addr) => Nav::Url(app::web_url(addr)),
            Err(e) => Nav::Error(e.to_string()),
        },
        Err(e) => Nav::Error(e.to_string()),
    }
}

/// 百分号编码（只保留 ASCII 字母数字和 `-_.~`），用于把错误文本安全地放进脚本字符串。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 生成在启动页显示错误文本的脚本。
pub fn error_script(msg: &str) -> String {
    format!(
        "document.getElementById('msg').textContent = '启动失败：\n' + decodeURIComponent('{}');",
        percent_encode(msg)
    )
}

/// 对系统开机自启状态要做的操作。
#[derive(Debug, PartialEq, Eq)]
pub enum AutostartAction {
    Nothing,
    Enable,
    Disable,
}

/// 设置值（未设置或类型不对为 `None`）与系统状态不一致时，决定如何改系统状态。
pub fn autostart_action(setting: Option<bool>, system: bool) -> AutostartAction {
    match setting {
        Some(true) if !system => AutostartAction::Enable,
        Some(false) if system => AutostartAction::Disable,
        _ => AutostartAction::Nothing,
    }
}

/// 设置值缺失（从未设置）时，应把系统当前状态写回设置；否则无需写。
pub fn autostart_seed(setting: Option<bool>, system: bool) -> Option<bool> {
    setting.is_none().then_some(system)
}

/// "关闭窗口时缩到托盘"的有效值：缺省或类型不对为 true。
pub fn close_to_tray_value(setting: Option<bool>) -> bool {
    setting.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::AppError;

    fn addr() -> SocketAddr {
        "127.0.0.1:8730".parse().unwrap()
    }

    #[test]
    fn hidden_flag() {
        assert!(has_hidden(["desktop.exe", "--hidden"]));
        assert!(!has_hidden(["desktop.exe"]));
        assert!(!has_hidden(Vec::<String>::new()));
    }

    #[test]
    fn ok_goes_to_url() {
        let r: Result<SocketAddr, AppError> = Ok(addr());
        let n = nav_for(&r, |a| *a, || panic!("不应调用"));
        assert_eq!(n, Nav::Url("http://127.0.0.1:8730/".into()));
    }

    #[test]
    fn locked_uses_config_listen() {
        let r: Result<SocketAddr, AppError> = Err(AppError::Locked("d".into()));
        let n = nav_for(&r, |a| *a, || Ok("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(n, Nav::Url("http://127.0.0.1:9000/".into()));
    }

    #[test]
    fn locked_config_failure_is_error() {
        let r: Result<SocketAddr, AppError> = Err(AppError::Locked("d".into()));
        let n = nav_for(&r, |a| *a, || Err(AppError::Config("坏".into())));
        assert!(matches!(n, Nav::Error(m) if m.contains("坏")));
    }

    #[test]
    fn other_error_shown() {
        let r: Result<SocketAddr, AppError> = Err(AppError::Start("boom".into()));
        assert_eq!(
            nav_for(&r, |a| *a, || Ok(addr())),
            Nav::Error("boom".into())
        );
    }

    #[test]
    fn encode_is_script_safe() {
        let s = percent_encode("a'b\"c\n中");
        assert!(s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "%-_.~".contains(c)));
        assert_eq!(percent_encode("a b"), "a%20b");
        assert!(error_script("x'y").contains("x%27y"));
    }

    #[test]
    fn autostart_decisions() {
        use AutostartAction::*;
        assert_eq!(autostart_action(Some(true), false), Enable);
        assert_eq!(autostart_action(Some(false), true), Disable);
        assert_eq!(autostart_action(Some(true), true), Nothing);
        assert_eq!(autostart_action(Some(false), false), Nothing);
        assert_eq!(autostart_action(None, true), Nothing);
    }

    #[test]
    fn autostart_seed_only_when_unset() {
        assert_eq!(autostart_seed(None, true), Some(true));
        assert_eq!(autostart_seed(None, false), Some(false));
        assert_eq!(autostart_seed(Some(false), true), None);
    }

    #[test]
    fn close_to_tray_defaults_true() {
        assert!(close_to_tray_value(None));
        assert!(close_to_tray_value(Some(true)));
        assert!(!close_to_tray_value(Some(false)));
    }
}

//! 用系统默认浏览器打开原文。抽象成 trait，测试里换成记录调用的假实现。

use std::process::{Command, Stdio};

pub trait Opener: Send + Sync {
    /// 调用方已保证 url 是 http 或 https。
    fn open(&self, url: &str) -> Result<(), String>;
}

/// 直接启动系统命令，不经过 shell。
pub struct SystemOpener;

impl Opener for SystemOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("rundll32");
            c.args(["url.dll,FileProtocolHandler", url]);
            c
        } else if cfg!(target_os = "macos") {
            let mut c = Command::new("open");
            c.arg(url);
            c
        } else {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        };
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("无法启动系统浏览器：{e}"))?;
        // 回收子进程，避免留下僵尸进程
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// 只允许 http / https。
pub(crate) fn is_http(url: &str) -> bool {
    let u = url.trim();
    let lower = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !u.chars().any(|c| c.is_control())
}

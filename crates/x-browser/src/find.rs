//! 在常见安装路径里查找本机浏览器，顺序：Chrome、Edge、Chromium。

use std::path::{Path, PathBuf};

/// 找到的浏览器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: &'static str,
    pub path: PathBuf,
}

/// 一种浏览器及其可能的安装路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: &'static str,
    pub paths: Vec<PathBuf>,
}

/// 生成候选清单，`env` 读环境变量（测试时可注入）。
pub fn candidates(env: &dyn Fn(&str) -> Option<String>) -> Vec<Candidate> {
    // Windows：Program Files、Program Files (x86)、用户目录下的安装
    let win_roots: Vec<PathBuf> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|k| env(k))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect();
    let win = |rel: &str| -> Vec<PathBuf> { win_roots.iter().map(|r| r.join(rel)).collect() };
    let fixed = |list: &[&str]| -> Vec<PathBuf> { list.iter().map(PathBuf::from).collect() };

    let mut chrome = win("Google/Chrome/Application/chrome.exe");
    chrome.extend(fixed(&[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/opt/google/chrome/chrome",
    ]));
    let mut edge = win("Microsoft/Edge/Application/msedge.exe");
    edge.extend(fixed(&[
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/usr/bin/microsoft-edge",
        "/usr/bin/microsoft-edge-stable",
        "/opt/microsoft/msedge/msedge",
    ]));
    let mut chromium = win("Chromium/Application/chrome.exe");
    chromium.extend(fixed(&[
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ]));
    vec![
        Candidate {
            name: "Chrome",
            paths: chrome,
        },
        Candidate {
            name: "Edge",
            paths: edge,
        },
        Candidate {
            name: "Chromium",
            paths: chromium,
        },
    ]
}

/// 按候选顺序取第一个存在的。
pub fn find_in(cands: &[Candidate], exists: &dyn Fn(&Path) -> bool) -> Option<Found> {
    cands.iter().find_map(|c| {
        c.paths.iter().find(|p| exists(p)).map(|p| Found {
            name: c.name,
            path: p.clone(),
        })
    })
}

/// 在本机查找。
pub fn detect() -> Option<Found> {
    let cands = candidates(&|k| std::env::var(k).ok());
    find_in(&cands, &|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> Option<String> {
        match k {
            "ProgramFiles" => Some("C:/PF".into()),
            "LOCALAPPDATA" => Some("C:/Local".into()),
            _ => None,
        }
    }

    #[test]
    fn chrome_wins_over_edge() {
        let c = candidates(&env);
        let f = find_in(&c, &|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            s.ends_with("chrome.exe") && s.contains("Google") || s.ends_with("msedge.exe")
        })
        .unwrap();
        assert_eq!(f.name, "Chrome");
        assert!(f
            .path
            .to_string_lossy()
            .replace('\\', "/")
            .starts_with("C:/PF/"));
    }

    #[test]
    fn falls_back_to_edge_then_chromium() {
        let c = candidates(&env);
        let f = find_in(&c, &|p| p.to_string_lossy().ends_with("msedge.exe")).unwrap();
        assert_eq!(f.name, "Edge");
        let f = find_in(&c, &|p| p.to_string_lossy().contains("Chromium")).unwrap();
        assert_eq!(f.name, "Chromium");
    }

    #[test]
    fn none_when_nothing_installed() {
        assert!(find_in(&candidates(&env), &|_| false).is_none());
    }

    #[test]
    fn includes_mac_and_linux_paths() {
        let all: Vec<String> = candidates(&|_| None)
            .into_iter()
            .flat_map(|c| c.paths)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(all.iter().any(|p| p.contains("Google Chrome.app")));
        assert!(all.iter().any(|p| p == "/usr/bin/chromium"));
    }
}

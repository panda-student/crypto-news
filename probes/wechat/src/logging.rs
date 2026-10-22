//! 追加写 `log.jsonl`：时间、动作、结果、错误码。已知的密钥会被替换成 `***`。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use ilink::Error;
use serde_json::{json, Value};

use crate::store::Res;
use crate::timeutil::now_rfc3339;

pub struct Logger {
    path: PathBuf,
    secrets: Vec<String>,
}

impl Logger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            secrets: Vec::new(),
        }
    }

    /// 登记需要从日志和终端输出中抹掉的密钥（登录 token、context token）。
    pub fn add_secret(&mut self, secret: &str) {
        if secret.len() >= 4 && !self.secrets.iter().any(|s| s == secret) {
            self.secrets.push(secret.to_string());
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "***");
        }
        out
    }

    pub fn format_line(
        &self,
        action: &str,
        ok: bool,
        errcode: Option<i32>,
        detail: Value,
    ) -> String {
        let entry = json!({
            "ts": now_rfc3339(),
            "action": action,
            "ok": ok,
            "errcode": errcode,
            "detail": detail,
        });
        self.redact(&entry.to_string())
    }

    pub fn record(&self, action: &str, ok: bool, errcode: Option<i32>, detail: Value) -> Res<()> {
        let line = self.format_line(action, ok, errcode, detail);
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

/// 把库的错误归类，供日志和终端输出使用。
pub struct ErrInfo {
    pub kind: &'static str,
    pub errcode: Option<i32>,
    pub http_status: Option<u16>,
    pub message: String,
}

pub fn describe(err: &Error) -> ErrInfo {
    let kind = match err {
        Error::RateLimited { .. } => "rate_limited",
        Error::CredentialExpired { .. } => "session_expired",
        Error::Api { .. } | Error::HttpStatus { .. } => "api",
        Error::Network(_) => "transport",
        Error::Decode(_) => "other",
    };
    ErrInfo {
        kind,
        errcode: err.errcode(),
        http_status: err.http_status(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_required_fields_and_no_secrets() {
        let mut log = Logger::new(PathBuf::from("unused"));
        log.add_secret("tok-secret-1");
        log.add_secret("ctx-secret-2");
        let line = log.format_line(
            "push",
            false,
            Some(-2),
            json!({"message": "bad tok-secret-1 and ctx-secret-2", "seq": 11}),
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        for key in ["ts", "action", "ok", "errcode", "detail"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["errcode"], -2);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
        assert!(!line.contains("tok-secret-1") && !line.contains("ctx-secret-2"));
        assert!(line.contains("***"));
    }

    #[test]
    fn record_appends_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("state").join("log.jsonl");
        let log = Logger::new(file.clone());
        log.record("a", true, None, json!({})).unwrap();
        log.record("b", false, Some(-14), json!({})).unwrap();
        let text = fs::read_to_string(file).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn classify_errors() {
        let e = Error::CredentialExpired {
            errcode: -14,
            message: "x".into(),
        };
        assert_eq!(describe(&e).kind, "session_expired");
        assert_eq!(describe(&e).errcode, Some(-14));
        let e = Error::RateLimited {
            errcode: -2,
            message: "x".into(),
        };
        assert_eq!(describe(&e).kind, "rate_limited");
        let e = Error::Api {
            errcode: 5,
            message: "x".into(),
        };
        assert_eq!(describe(&e).kind, "api");
        let e = Error::HttpStatus { status: 502 };
        assert_eq!(describe(&e).http_status, Some(502));
        let e = Error::Decode("bad".into());
        assert_eq!(describe(&e).errcode, None);
    }
}

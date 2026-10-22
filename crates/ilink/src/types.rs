//! 凭证、会话凭证、消息等数据类型。凭证类型的 `Debug` 一律脱敏。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// 登录后得到的账号凭证。库是无状态的：调用方负责保存（序列化）和恢复（反序列化）。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    /// 登录后服务端下发的 API 地址；后续收发消息都发往这里。
    #[serde(rename = "baseUrl", alias = "base_url")]
    pub base_url: String,
    #[serde(rename = "accountId", alias = "account_id", default)]
    pub account_id: String,
    #[serde(rename = "userId", alias = "user_id", default)]
    pub user_id: String,
    /// 保存时间（RFC 3339，UTC）。
    #[serde(default)]
    pub saved_at: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"***")
            .field("base_url", &self.base_url)
            .field("account_id", &self.account_id)
            .field("user_id", &self.user_id)
            .field("saved_at", &self.saved_at)
            .finish()
    }
}

/// 会话凭证：收到用户消息时下发的 `context_token`，发消息必须带上。
#[derive(Clone, Serialize, Deserialize)]
pub struct Context {
    pub account_key: String,
    /// 对端用户 ID（发消息的收件人）。
    pub user_id: String,
    pub context_token: String,
    pub observed_at_unix_ms: i64,
    #[serde(default)]
    pub source_message_id: Option<String>,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("account_key", &self.account_key)
            .field("user_id", &self.user_id)
            .field("context_token", &"***")
            .field("observed_at_unix_ms", &self.observed_at_unix_ms)
            .field("source_message_id", &self.source_message_id)
            .finish()
    }
}

/// 获取二维码的结果。
#[derive(Debug, Clone, Deserialize)]
pub struct QrCode {
    /// 轮询状态时使用的标识。
    pub qrcode: String,
    /// 二维码内容（要编码成二维码图片给用户扫）。
    pub qrcode_img_content: String,
}

/// 扫码状态。
#[derive(Debug, Clone)]
pub enum QrStatus {
    Wait,
    Scaned,
    /// 已确认，附带凭证。
    Confirmed(Credentials),
    Expired,
    NeedVerifyCode,
    VerifyCodeBlocked,
    BindedRedirect,
    /// 需要改用新的主机继续轮询（`https://<redirect_host>`）。
    ScanedButRedirect {
        redirect_host: String,
    },
    /// 未识别的状态值。
    Unknown(String),
}

fn de_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// 消息内容项。
#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    #[serde(rename = "type", default)]
    pub kind: i32,
    #[serde(default)]
    pub text_item: Option<TextItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

/// 收到的一条消息。
#[derive(Clone, Deserialize)]
pub struct Message {
    #[serde(default, deserialize_with = "de_id")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    pub to_user_id: String,
    #[serde(default)]
    pub create_time_ms: i64,
    /// 1 = 用户消息，2 = 机器人消息。
    #[serde(default)]
    pub message_type: i32,
    #[serde(default)]
    pub context_token: Option<String>,
    #[serde(default)]
    pub item_list: Vec<Item>,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("message_id", &self.message_id)
            .field("from_user_id", &self.from_user_id)
            .field("message_type", &self.message_type)
            .field("context_token", &self.context_token.as_ref().map(|_| "***"))
            .field("item_list", &self.item_list)
            .finish()
    }
}

impl Message {
    /// 是否为用户发来的消息（而不是机器人自己的回显）。
    pub fn is_from_user(&self) -> bool {
        self.message_type == 1
    }

    /// 所有文本项拼接。
    pub fn text(&self) -> String {
        self.item_list
            .iter()
            .filter_map(|i| i.text_item.as_ref())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 取出会话凭证（消息带了 context_token 时）。`observed_at_unix_ms` 由调用方给出收到消息的时间。
    pub fn context(&self, account_key: &str, observed_at_unix_ms: i64) -> Option<Context> {
        let token = self.context_token.as_deref().filter(|t| !t.is_empty())?;
        Some(Context {
            account_key: account_key.to_string(),
            user_id: self.from_user_id.clone(),
            context_token: token.to_string(),
            observed_at_unix_ms,
            source_message_id: self.message_id.clone(),
        })
    }
}

/// 一次长轮询的结果。
#[derive(Debug, Clone, Default)]
pub struct Updates {
    pub messages: Vec<Message>,
    /// 下一次轮询要带的游标。服务端没有给新游标（或超时）时等于传入的旧游标。
    pub cursor: String,
    /// 游标是否前进了。
    pub cursor_advanced: bool,
}

/// 发送成功的回执。
#[derive(Debug, Clone, Default)]
pub struct SendReceipt {
    pub client_id: String,
    pub message_id: Option<String>,
}

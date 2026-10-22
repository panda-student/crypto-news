//! 微信 iLink（ClawBot）协议客户端。纯协议库：
//!
//! - 无状态：登录得到的 [`Credentials`] 和收消息得到的 [`Context`] 由调用方保存、恢复；
//! - 不建 HTTP 客户端、不设代理：调用方传入 `reqwest::Client`；
//! - 所有地址（包括二维码主机）由 [`Config`] 和 [`Credentials`] 提供，没有写死的主机；
//! - 不重试、不退避：错误原样返回，分类见 [`Error`]。
//!
//! 协议细节见同目录的 `PROTOCOL.md`。

mod client;
mod error;
mod types;

pub use client::{Client, Config};
pub use error::{Error, Result, CODE_RATE_LIMITED, CODE_SESSION_EXPIRED};
pub use types::{
    Context, Credentials, Item, Message, QrCode, QrStatus, SendReceipt, TextItem, Updates,
};

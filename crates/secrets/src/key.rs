//! 主密钥来源。首版只有系统钥匙串；测试用内存实现。以后可以加环境变量、密钥文件等来源。

use std::fmt;

/// 主密钥长度（AES-256）。
pub const KEY_LEN: usize = 32;

/// 取主密钥时的错误。信息里不含密钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(pub String);

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyError {}

/// 主密钥来源：取出主密钥，没有时生成并保存。
pub trait MasterKeySource: Send + Sync + 'static {
    fn load_or_create(&self) -> Result<[u8; KEY_LEN], KeyError>;
}

/// 生成随机字节。
pub fn random_bytes<const N: usize>() -> Result<[u8; N], KeyError> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|e| KeyError(format!("生成随机数失败：{e}")))?;
    Ok(b)
}

/// 系统钥匙串（Windows 凭据管理器、macOS 钥匙串、Linux Secret Service）。
pub struct KeyringSource {
    service: String,
    user: String,
}

impl KeyringSource {
    pub fn new(service: impl Into<String>, user: impl Into<String>) -> Self {
        KeyringSource {
            service: service.into(),
            user: user.into(),
        }
    }
}

impl Default for KeyringSource {
    fn default() -> Self {
        KeyringSource::new("crypto-news", "master-key")
    }
}

impl MasterKeySource for KeyringSource {
    fn load_or_create(&self) -> Result<[u8; KEY_LEN], KeyError> {
        let entry = keyring::Entry::new(&self.service, &self.user)
            .map_err(|e| KeyError(format!("打开系统钥匙串失败：{}", describe(&e))))?;
        match entry.get_password() {
            Ok(s) => decode_key(&s),
            Err(keyring::Error::NoEntry) => {
                let key = random_bytes::<KEY_LEN>()?;
                entry
                    .set_password(&to_hex(&key))
                    .map_err(|e| KeyError(format!("写入系统钥匙串失败：{}", describe(&e))))?;
                Ok(key)
            }
            Err(e) => Err(KeyError(format!("读取系统钥匙串失败：{}", describe(&e)))),
        }
    }
}

/// 钥匙串错误只保留类别，不带可能含密钥的内容。
fn describe(e: &keyring::Error) -> &'static str {
    match e {
        keyring::Error::PlatformFailure(_) => "系统接口错误",
        keyring::Error::NoStorageAccess(_) => "无法访问钥匙串",
        keyring::Error::NoEntry => "没有条目",
        keyring::Error::BadEncoding(_) => "条目编码错误",
        keyring::Error::TooLong(_, _) => "字段过长",
        keyring::Error::Invalid(_, _) => "参数无效",
        keyring::Error::Ambiguous(_) => "存在多个同名条目",
        _ => "未知错误",
    }
}

fn decode_key(s: &str) -> Result<[u8; KEY_LEN], KeyError> {
    let bad = || KeyError("钥匙串里的主密钥格式不对".to_string());
    let v = from_hex(s.trim()).ok_or_else(bad)?;
    v.try_into().map_err(|_| bad())
}

/// 内存里的固定主密钥，给测试用。
pub struct MemoryKeySource(pub [u8; KEY_LEN]);

impl MasterKeySource for MemoryKeySource {
    fn load_or_create(&self) -> Result<[u8; KEY_LEN], KeyError> {
        Ok(self.0)
    }
}

pub fn to_hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 0xf) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(nib(p[0])? << 4 | nib(p[1])?))
        .collect()
}

//! secrets 插件：提供 `secrets` 服务 1.0。
//!
//! - 每个值用 AES-256-GCM 加密，nonce 每次随机 12 字节，键名作附加数据（密文不能挪给别的键用）。
//! - nonce 和密文以 hex 文本存在 `secrets` 表（store 的查询结果不支持 BLOB）。
//! - 主密钥来自 [`MasterKeySource`]，首版是系统钥匙串。
//! - 权限：插件只能读写自己前缀（`<插件名>.`）下的键；web 能写、删任意键、列出全部，但读不到明文。
//! - 值不进日志、错误信息。

mod key;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use plugin_sdk::contracts::secrets::{SecretEntry, SecretList, SecretValue, SERVICE};
use plugin_sdk::contracts::{now_rfc3339, parse_params, to_result, Caller};
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError, ServiceError};
use serde::Deserialize;
use serde_json::{json, Value};

pub use key::{KeyError, KeyringSource, MasterKeySource, MemoryKeySource, KEY_LEN};

const MANIFEST: &str = include_str!("../plugin.toml");
const SCHEMA: &str = "CREATE TABLE secrets (\
    key TEXT PRIMARY KEY, nonce TEXT NOT NULL, ciphertext TEXT NOT NULL, updated_at TEXT NOT NULL)";

/// 能管理全部键的插件。
const ADMIN_PLUGIN: &str = "web";
/// 值的上限。
const MAX_VALUE_BYTES: usize = 8 * 1024;
const NONCE_LEN: usize = 12;

pub struct SecretsPlugin {
    source: Arc<dyn MasterKeySource>,
}

impl SecretsPlugin {
    pub fn new(source: Arc<dyn MasterKeySource>) -> Self {
        SecretsPlugin { source }
    }
}

impl Default for SecretsPlugin {
    /// 主密钥放在系统钥匙串。
    fn default() -> Self {
        SecretsPlugin::new(Arc::new(KeyringSource::default()))
    }
}

impl Plugin for SecretsPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let key = self
                .source
                .load_or_create()
                .map_err(|e| PluginError::new(format!("取主密钥失败：{e}")))?;
            ctx.store()
                .migrate(1, SCHEMA)
                .await
                .map_err(|e| PluginError::new(format!("建表失败：{}", e.message)))?;
            let state = Arc::new(State::new(ctx.clone(), &key));
            ctx.provide_fn(SERVICE, move |caller, method, params| {
                let s = state.clone();
                async move { s.handle(&caller, &method, params).await }
            })?;
            Ok(())
        })
    }
}

struct State {
    ctx: Context,
    cipher: Aes256Gcm,
}

#[derive(Deserialize)]
struct KeyParam {
    key: String,
}

#[derive(Deserialize)]
struct SetParam {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct Row {
    nonce: String,
    ciphertext: String,
}

/// 键名格式 `<插件名>.<字段名>`：插件名不含点；字段名可含点分隔的多段，每段都非空，
/// 只含小写字母、数字、`_`、`-`（如 `llm.key.abc`）。
fn valid_key(key: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
    };
    match key.split_once('.') {
        Some((p, f)) => key.len() <= 128 && ok(p) && f.split('.').all(ok),
        None => false,
    }
}

/// 键属于调用方自己。
fn owns(caller: &Caller, key: &str) -> bool {
    key.split_once('.').is_some_and(|(p, _)| p == caller.plugin)
}

fn db_err(e: ServiceError) -> ServiceError {
    ServiceError::internal(format!("读写密钥表失败：{}", e.message))
}

impl State {
    fn new(ctx: Context, key: &[u8; KEY_LEN]) -> Self {
        State {
            ctx,
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
        }
    }

    async fn handle(
        &self,
        caller: &Caller,
        method: &str,
        params: Value,
    ) -> Result<Value, ServiceError> {
        match method {
            "get" => {
                let p: KeyParam = parse_params(params)?;
                check_key(&p.key)?;
                if !owns(caller, &p.key) {
                    return Err(ServiceError::forbidden("只能读取自己的密钥"));
                }
                to_result(&SecretValue {
                    value: self.get(&p.key).await?,
                })
            }
            "set" => {
                let p: SetParam = parse_params(params)?;
                check_key(&p.key)?;
                if !(owns(caller, &p.key) || caller.plugin == ADMIN_PLUGIN) {
                    return Err(ServiceError::forbidden("只能写自己的密钥"));
                }
                if p.value.is_empty() {
                    return Err(ServiceError::invalid_params("密钥值不能为空"));
                }
                if p.value.len() > MAX_VALUE_BYTES {
                    return Err(ServiceError::invalid_params("密钥值不能超过 8 KiB"));
                }
                self.set(&p.key, &p.value).await?;
                Ok(json!({}))
            }
            "delete" => {
                let p: KeyParam = parse_params(params)?;
                check_key(&p.key)?;
                if !(owns(caller, &p.key) || caller.plugin == ADMIN_PLUGIN) {
                    return Err(ServiceError::forbidden("只能删除自己的密钥"));
                }
                self.ctx
                    .store()
                    .execute("DELETE FROM secrets WHERE key = ?", vec![json!(p.key)])
                    .await
                    .map_err(db_err)?;
                Ok(json!({}))
            }
            "list" => {
                #[derive(Deserialize)]
                struct R {
                    key: String,
                    updated_at: String,
                }
                let rows: Vec<R> = self
                    .ctx
                    .store()
                    .query_as("SELECT key, updated_at FROM secrets ORDER BY key", vec![])
                    .await
                    .map_err(db_err)?;
                let secrets = rows
                    .into_iter()
                    .filter(|r| caller.plugin == ADMIN_PLUGIN || owns(caller, &r.key))
                    .map(|r| SecretEntry {
                        key: r.key,
                        configured: true,
                        updated_at: Some(r.updated_at),
                    })
                    .collect();
                to_result(&SecretList { secrets })
            }
            other => Err(ServiceError::method_not_found(other)),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<String>, ServiceError> {
        let rows: Vec<Row> = self
            .ctx
            .store()
            .query_as(
                "SELECT nonce, ciphertext FROM secrets WHERE key = ?",
                vec![json!(key)],
            )
            .await
            .map_err(db_err)?;
        let Some(r) = rows.into_iter().next() else {
            return Ok(None);
        };
        self.decrypt(key, &r.nonce, &r.ciphertext).map(Some)
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), ServiceError> {
        let nonce =
            key::random_bytes::<NONCE_LEN>().map_err(|e| ServiceError::internal(e.to_string()))?;
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| ServiceError::internal("加密失败"))?;
        self.ctx
            .store()
            .execute(
                "INSERT INTO secrets (key, nonce, ciphertext, updated_at) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(key) DO UPDATE SET nonce = excluded.nonce, \
                 ciphertext = excluded.ciphertext, updated_at = excluded.updated_at",
                vec![
                    json!(key),
                    json!(key::to_hex(&nonce)),
                    json!(key::to_hex(&ct)),
                    json!(now_rfc3339()),
                ],
            )
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// 解不开（主密钥换了或数据损坏）时报错，提示重新填写；错误里不带任何内容。
    fn decrypt(&self, key: &str, nonce: &str, ct: &str) -> Result<String, ServiceError> {
        let broken = || {
            tracing::warn!(key, "密钥无法解密，需要重新填写");
            ServiceError::unavailable(format!("密钥 {key} 无法解密，请在界面重新填写"))
        };
        let nonce = key::from_hex(nonce)
            .filter(|n| n.len() == NONCE_LEN)
            .ok_or_else(broken)?;
        let ct = key::from_hex(ct).ok_or_else(broken)?;
        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ct,
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| broken())?;
        String::from_utf8(plain).map_err(|_| broken())
    }
}

fn check_key(key: &str) -> Result<(), ServiceError> {
    if valid_key(key) {
        Ok(())
    } else {
        Err(ServiceError::invalid_params(
            "键名格式应为 <插件名>.<字段名>，只含小写字母、数字、_ 和 -",
        ))
    }
}

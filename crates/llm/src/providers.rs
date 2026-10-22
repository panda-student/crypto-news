//! 模型服务与用途分配：增删改、默认、选模型、旧配置迁移。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use plugin_sdk::contracts::llm::{
    provider_kinds, purposes, Assignment, ProviderInfo, ProviderInput, ProviderList, TestResult,
    Tier,
};
use plugin_sdk::contracts::now_rfc3339;
use plugin_sdk::contracts::secrets::SecretsClient;
use plugin_sdk::contracts::store::Statement;
use plugin_sdk::{Context, ServiceError};
use serde::Deserialize;
use serde_json::json;

use crate::{Config, Kind, Target};

const KEY_PREFIX: &str = "llm.key.";
const LEGACY_KEY: &str = "llm.api_key";
const NO_PROVIDER: &str = "还没有配置模型服务";
const COLUMNS: &str = "id, name, kind, base_url, models, is_default, last_test";

fn secret_key(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

fn secrets(ctx: &Context) -> SecretsClient {
    SecretsClient(ctx.service("secrets"))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProviderRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    models: String,
    is_default: i64,
    #[serde(default)]
    last_test: Option<String>,
}

impl ProviderRow {
    pub fn models(&self) -> Vec<String> {
        serde_json::from_str(&self.models).unwrap_or_default()
    }

    /// 按档位取默认模型：小用第一个，大用第二个（没有则第一个）。
    fn tier_model(&self, tier: Tier) -> Option<String> {
        let models = self.models();
        match tier {
            Tier::Small => models.into_iter().next(),
            Tier::Large => {
                let first = models.first().cloned();
                models.into_iter().nth(1).or(first)
            }
        }
    }

    fn info(&self, has_key: bool) -> ProviderInfo {
        ProviderInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind.clone(),
            base_url: self.base_url.clone(),
            models: self.models(),
            has_key,
            is_default: self.is_default != 0,
            last_test: self
                .last_test
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
        }
    }
}

pub(crate) async fn rows(ctx: &Context) -> Result<Vec<ProviderRow>, ServiceError> {
    ctx.store()
        .query_as(
            &format!("SELECT {COLUMNS} FROM providers ORDER BY created_at, id"),
            vec![],
        )
        .await
}

async fn find(ctx: &Context, id: &str) -> Result<ProviderRow, ServiceError> {
    let mut v: Vec<ProviderRow> = ctx
        .store()
        .query_as(
            &format!("SELECT {COLUMNS} FROM providers WHERE id = ?"),
            vec![json!(id)],
        )
        .await?;
    v.pop()
        .ok_or_else(|| ServiceError::invalid_params(format!("模型服务不存在：{id}")))
}

/// 已设置密钥的 secrets 键。
async fn configured_keys(ctx: &Context) -> Result<Vec<String>, ServiceError> {
    Ok(secrets(ctx)
        .list()
        .await?
        .into_iter()
        .filter(|e| e.configured)
        .map(|e| e.key)
        .collect())
}

pub(crate) async fn list(ctx: &Context) -> Result<ProviderList, ServiceError> {
    let keys = configured_keys(ctx).await?;
    let providers = rows(ctx)
        .await?
        .iter()
        .map(|r| r.info(keys.contains(&secret_key(&r.id))))
        .collect();
    let assignments = ctx
        .store()
        .query_as(
            "SELECT purpose, provider_id, model FROM assignments ORDER BY purpose",
            vec![],
        )
        .await?;
    Ok(ProviderList {
        providers,
        assignments,
    })
}

fn new_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("p{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn clean_models(models: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in models {
        let m = m.trim();
        if !m.is_empty() && !out.iter().any(|x| x == m) {
            out.push(m.to_string());
        }
    }
    out
}

pub(crate) async fn save(
    ctx: &Context,
    input: ProviderInput,
) -> Result<ProviderInfo, ServiceError> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ServiceError::invalid_params("名称不能为空"));
    }
    if input.kind != provider_kinds::OPENAI && input.kind != provider_kinds::ANTHROPIC {
        return Err(ServiceError::invalid_params(
            "接口类型只能是 openai 或 anthropic",
        ));
    }
    let base_url = input.base_url.trim().to_string();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ServiceError::invalid_params(
            "接口地址必须以 http:// 或 https:// 开头",
        ));
    }
    let models = clean_models(&input.models);
    if models.is_empty() {
        return Err(ServiceError::invalid_params("至少需要一个模型"));
    }
    let new_key = input.api_key.filter(|k| !k.trim().is_empty());
    let models_json = json!(models).to_string();

    let id = match &input.id {
        Some(id) => {
            let old = find(ctx, id).await?;
            // 地址或接口类型变了，旧的测试结果作废
            let reset = old.kind != input.kind || old.base_url != base_url;
            let mut stmts = vec![Statement::new(
                "UPDATE providers SET name = ?, kind = ?, base_url = ?, models = ?, \
                 last_test = CASE WHEN ? THEN NULL ELSE last_test END WHERE id = ?",
                vec![
                    json!(name),
                    json!(input.kind),
                    json!(base_url),
                    json!(models_json),
                    json!(i64::from(reset)),
                    json!(old.id),
                ],
            )];
            // 模型列表缩小后，指向已删除模型的分配一并清掉
            for purpose in assignments_outside(ctx, &old.id, &models).await? {
                stmts.push(Statement::new(
                    "DELETE FROM assignments WHERE purpose = ?",
                    vec![json!(purpose)],
                ));
            }
            ctx.store().batch(stmts).await?;
            if let Some(key) = &new_key {
                secrets(ctx).set(&secret_key(&old.id), key).await?;
            }
            old.id
        }
        None => {
            let key = new_key
                .as_deref()
                .ok_or_else(|| ServiceError::invalid_params("新建模型服务必须填写密钥"))?;
            let id = new_id();
            // 先存密钥，再写服务行，失败时不留下孤立的服务
            secrets(ctx).set(&secret_key(&id), key).await?;
            let first = rows(ctx).await?.is_empty();
            let inserted = ctx
                .store()
                .execute(
                    "INSERT INTO providers (id, name, kind, base_url, models, is_default, \
                     last_test, created_at) VALUES (?, ?, ?, ?, ?, ?, NULL, ?)",
                    vec![
                        json!(id),
                        json!(name),
                        json!(input.kind),
                        json!(base_url),
                        json!(models_json),
                        json!(i64::from(first)),
                        json!(now_rfc3339()),
                    ],
                )
                .await;
            if let Err(e) = inserted {
                let _ = secrets(ctx).delete(&secret_key(&id)).await;
                return Err(e);
            }
            id
        }
    };
    let row = find(ctx, &id).await?;
    let has_key = configured_keys(ctx).await?.contains(&secret_key(&id));
    Ok(row.info(has_key))
}

/// 该服务下，模型已不在 `models` 里的分配所对应的用途。
async fn assignments_outside(
    ctx: &Context,
    provider_id: &str,
    models: &[String],
) -> Result<Vec<String>, ServiceError> {
    let rows = ctx
        .store()
        .query(
            "SELECT purpose, model FROM assignments WHERE provider_id = ?",
            vec![json!(provider_id)],
        )
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let model = r.get("model")?.as_str()?;
            let purpose = r.get("purpose")?.as_str()?;
            (!models.iter().any(|m| m == model)).then(|| purpose.to_string())
        })
        .collect())
}

pub(crate) async fn delete(ctx: &Context, id: &str) -> Result<(), ServiceError> {
    let row = find(ctx, id).await?;
    if row.is_default != 0 {
        return Err(ServiceError::invalid_params(
            "不能删除默认服务，请先把别的服务设为默认",
        ));
    }
    ctx.store()
        .batch(vec![
            Statement::new(
                "DELETE FROM assignments WHERE provider_id = ?",
                vec![json!(id)],
            ),
            Statement::new("DELETE FROM providers WHERE id = ?", vec![json!(id)]),
        ])
        .await?;
    if let Err(e) = secrets(ctx).delete(&secret_key(id)).await {
        tracing::warn!("llm：删除模型服务密钥失败：{}", e.message);
    }
    Ok(())
}

pub(crate) async fn set_default(ctx: &Context, id: &str) -> Result<(), ServiceError> {
    find(ctx, id).await?;
    ctx.store()
        .batch(vec![
            Statement::new("UPDATE providers SET is_default = 0", vec![]),
            Statement::new(
                "UPDATE providers SET is_default = 1 WHERE id = ?",
                vec![json!(id)],
            ),
        ])
        .await?;
    Ok(())
}

pub(crate) async fn assign(ctx: &Context, a: Assignment) -> Result<(), ServiceError> {
    if !purposes::ALL.iter().any(|(p, _)| *p == a.purpose) {
        return Err(ServiceError::invalid_params(format!(
            "未知用途：{}",
            a.purpose
        )));
    }
    let row = find(ctx, &a.provider_id).await?;
    if !row.models().contains(&a.model) {
        return Err(ServiceError::invalid_params(format!(
            "模型 {} 不在服务「{}」的模型列表里",
            a.model, row.name
        )));
    }
    ctx.store()
        .execute(
            "INSERT INTO assignments (purpose, provider_id, model) VALUES (?, ?, ?) \
             ON CONFLICT (purpose) DO UPDATE SET provider_id = excluded.provider_id, \
             model = excluded.model",
            vec![json!(a.purpose), json!(a.provider_id), json!(a.model)],
        )
        .await?;
    Ok(())
}

/// 选出本次调用用的服务和模型：先看用途分配，再回到默认服务按档位取。
pub(crate) async fn resolve(
    ctx: &Context,
    purpose: Option<&str>,
    tier: Tier,
) -> Result<(ProviderRow, String), ServiceError> {
    if let Some(p) = purpose {
        let a: Vec<Assignment> = ctx
            .store()
            .query_as(
                "SELECT purpose, provider_id, model FROM assignments WHERE purpose = ?",
                vec![json!(p)],
            )
            .await?;
        if let Some(a) = a.into_iter().next() {
            if let Ok(row) = find(ctx, &a.provider_id).await {
                return Ok((row, a.model));
            }
        }
    }
    let row = default_row(ctx).await?;
    let model = row
        .tier_model(tier)
        .ok_or_else(|| ServiceError::unavailable("模型服务没有可用的模型"))?;
    Ok((row, model))
}

async fn default_row(ctx: &Context) -> Result<ProviderRow, ServiceError> {
    let mut v: Vec<ProviderRow> = ctx
        .store()
        .query_as(
            &format!(
                "SELECT {COLUMNS} FROM providers ORDER BY is_default DESC, created_at, id LIMIT 1"
            ),
            vec![],
        )
        .await?;
    v.pop()
        .ok_or_else(|| ServiceError::unavailable(NO_PROVIDER))
}

/// 测试连接的对象：指定服务，或默认服务；用它的第一个模型。没有任何服务时为 None。
pub(crate) async fn test_subject(
    ctx: &Context,
    provider_id: Option<&str>,
) -> Result<Option<(ProviderRow, String)>, ServiceError> {
    let row = match provider_id {
        Some(id) => find(ctx, id).await?,
        None => match default_row(ctx).await {
            Ok(r) => r,
            Err(e) if e.code == ServiceError::UNAVAILABLE => return Ok(None),
            Err(e) => return Err(e),
        },
    };
    Ok(row.tier_model(Tier::Small).map(|m| (row, m)))
}

/// 取密钥并组装成调用目标。
pub(crate) async fn target(
    ctx: &Context,
    row: &ProviderRow,
    model: String,
) -> Result<Target, ServiceError> {
    let kind = Kind::parse(&row.kind)
        .ok_or_else(|| ServiceError::unavailable("模型服务的接口类型无效"))?;
    let key = secrets(ctx)
        .get(&secret_key(&row.id))
        .await
        .map_err(|_| ServiceError::unavailable("读取模型密钥失败"))?
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| ServiceError::unavailable("未配置模型密钥"))?;
    Ok(Target {
        kind,
        base_url: row.base_url.clone(),
        model,
        key,
    })
}

pub(crate) async fn save_test(
    ctx: &Context,
    id: &str,
    result: &TestResult,
) -> Result<(), ServiceError> {
    let text = serde_json::to_string(result).map_err(ServiceError::internal)?;
    ctx.store()
        .execute(
            "UPDATE providers SET last_test = ? WHERE id = ?",
            vec![json!(text), json!(id)],
        )
        .await?;
    Ok(())
}

/// 是否有可用的默认服务（有模型且已设置密钥）。
pub(crate) async fn ready(ctx: &Context) -> Result<bool, ServiceError> {
    let Ok(row) = default_row(ctx).await else {
        return Ok(false);
    };
    if row.models().is_empty() {
        return Ok(false);
    }
    Ok(configured_keys(ctx).await?.contains(&secret_key(&row.id)))
}

/// 旧配置迁移：没有任何服务、旧配置里有地址并且有密钥时，导入成默认服务。
/// 只在 providers 表为空时发生，导入后表非空，所以只做一次。
pub(crate) async fn migrate_legacy(ctx: &Context, cfg: &Config) -> Result<(), ServiceError> {
    if !rows(ctx).await?.is_empty() {
        return Ok(());
    }
    let small = cfg.small_model.trim();
    if cfg.base_url.trim().is_empty() || small.is_empty() {
        return Ok(());
    }
    let key = match secrets(ctx).get(LEGACY_KEY).await? {
        Some(k) if !k.trim().is_empty() => k,
        _ => match cfg.api_key.as_deref().map(str::trim) {
            Some(k) if !k.is_empty() => k.to_string(),
            _ => return Ok(()),
        },
    };
    let large = cfg.large_model.trim();
    let mut models = vec![small.to_string()];
    let has_large = !large.is_empty() && large != small;
    if has_large {
        models.push(large.to_string());
    }
    let id = new_id();
    secrets(ctx).set(&secret_key(&id), &key).await?;
    let mut stmts = vec![Statement::new(
        "INSERT INTO providers (id, name, kind, base_url, models, is_default, last_test, \
         created_at) VALUES (?, '默认服务', ?, ?, ?, 1, NULL, ?)",
        vec![
            json!(id),
            json!(cfg.provider.as_str()),
            json!(cfg.base_url.trim()),
            json!(json!(models).to_string()),
            json!(now_rfc3339()),
        ],
    )];
    if has_large {
        for (purpose, model) in [
            (purposes::SCREEN, small),
            (purposes::ANALYZE, large),
            (purposes::AGENT, large),
        ] {
            stmts.push(Statement::new(
                "INSERT INTO assignments (purpose, provider_id, model) VALUES (?, ?, ?)",
                vec![json!(purpose), json!(id), json!(model)],
            ));
        }
    }
    ctx.store().batch(stmts).await?;
    tracing::info!("llm：已把旧配置导入为默认模型服务");
    Ok(())
}

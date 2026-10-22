//! 进程内插件与 hub 的连接：权限检查、服务路由，以及 hub 自己提供的 `hub-admin` 服务。

use std::sync::Arc;

use contracts::hub_admin::{self, PluginConfig, PluginList};
use contracts::store::{self, dispatch_store_call, TableAccess};
use contracts::{new_trace_id, parse_params, to_result, Caller, ServiceError, OWNER};
use plugin_sdk::{BoxFuture, EventFn, HubLink, InterceptFn, PluginError, StopFn};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{db, events, Cmd, Entry, Inner, Phase, State, HUB};

/// 一次启动对应一个 Link；插件重启后旧 Link 的登记全部作废。
pub(crate) struct Link {
    pub(crate) inner: Arc<Inner>,
    pub(crate) name: String,
    pub(crate) generation: u64,
}

impl Link {
    /// 本次启动是否仍有效。
    fn alive<'a>(&self, st: &'a mut State) -> Option<&'a mut Entry> {
        st.plugins
            .get_mut(&self.name)
            .filter(|e| e.generation == self.generation && e.phase != Phase::Stopped)
    }

    fn stale(&self) -> PluginError {
        PluginError::new(format!("插件 {} 已停止", self.name))
    }

    /// 登记一个随插件停止而中止的任务。
    pub(crate) fn add_task(&self, h: tokio::task::AbortHandle) {
        let mut st = self.inner.state.lock().unwrap();
        match self.alive(&mut st).and_then(|e| e.run.as_mut()) {
            Some(run) => run.tasks.push(h),
            None => h.abort(),
        }
    }

    /// 报告插件失败，由监督任务清理并退避重启。
    pub(crate) fn fail(&self, error: String) {
        self.inner.send(Cmd::Failed {
            name: self.name.clone(),
            generation: self.generation,
            error,
        });
    }

    /// 清单里是否声明了。
    fn declared(&self, f: impl FnOnce(&Entry) -> bool) -> Result<(), PluginError> {
        let mut st = self.inner.state.lock().unwrap();
        let e = self.alive(&mut st).ok_or_else(|| self.stale())?;
        if f(e) {
            Ok(())
        } else {
            Err(PluginError::new("清单里没有声明"))
        }
    }
}

impl HubLink for Link {
    fn call(
        &self,
        service: &str,
        provider: Option<&str>,
        method: &str,
        params: Value,
    ) -> BoxFuture<'static, Result<Value, ServiceError>> {
        let inner = self.inner.clone();
        let (name, service, method) = (self.name.clone(), service.to_string(), method.to_string());
        let provider = provider.map(str::to_string);
        Box::pin(async move {
            call_as(
                &inner,
                &name,
                &service,
                provider.as_deref(),
                &method,
                params,
            )
            .await
        })
    }

    fn providers(&self, service: &str) -> Vec<String> {
        let st = self.inner.state.lock().unwrap();
        st.services
            .get(service)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn publish(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<i64, PluginError>> {
        let inner = self.inner.clone();
        let (name, kind) = (self.name.clone(), kind.to_string());
        let ok = self
            .declared(|e| e.manifest.events.publish.contains(&kind))
            .map_err(|e| PluginError::new(format!("发出事件 {kind}：{e}")));
        Box::pin(async move {
            ok?;
            events::publish(&inner, &name, &kind, payload, trace_id).await
        })
    }

    fn subscribe(&self, kind: &str, f: EventFn) -> Result<(), PluginError> {
        self.declared(|e| e.manifest.events.subscribe.iter().any(|k| k == kind))
            .map_err(|e| PluginError::new(format!("订阅事件 {kind}：{e}")))?;
        let ephemeral = self.inner.is_ephemeral(kind);
        self.inner
            .state
            .lock()
            .unwrap()
            .subscribers
            .insert((kind.to_string(), self.name.clone()), f.clone());
        if !ephemeral {
            let fut = events::deliver_loop(self.inner.clone(), self.name.clone(), kind.into(), f);
            self.spawn(&format!("deliver:{kind}"), Box::pin(fut));
        }
        Ok(())
    }

    fn intercept(&self, kind: &str, f: InterceptFn) -> Result<(), PluginError> {
        self.declared(|e| e.manifest.events.intercept.iter().any(|k| k == kind))
            .map_err(|e| PluginError::new(format!("拦截事件 {kind}：{e}")))?;
        self.inner
            .state
            .lock()
            .unwrap()
            .interceptors
            .insert((kind.to_string(), self.name.clone()), f);
        Ok(())
    }

    fn emit_intercept(
        &self,
        kind: &str,
        payload: Value,
        trace_id: Option<String>,
    ) -> BoxFuture<'static, Result<Option<Value>, PluginError>> {
        let inner = self.inner.clone();
        let (name, kind) = (self.name.clone(), kind.to_string());
        let ok = self
            .declared(|e| e.manifest.events.publish.contains(&kind))
            .map_err(|e| PluginError::new(format!("发出事件 {kind}：{e}")));
        Box::pin(async move {
            ok?;
            Ok(events::emit_intercept(&inner, &name, &kind, payload, trace_id).await)
        })
    }

    fn provide(
        &self,
        service: &str,
        h: Arc<dyn contracts::ServiceHandler>,
    ) -> Result<(), PluginError> {
        self.declared(|e| e.manifest.provides.contains_key(service))
            .map_err(|e| PluginError::new(format!("提供服务 {service}：{e}")))?;
        self.inner
            .state
            .lock()
            .unwrap()
            .services
            .entry(service.to_string())
            .or_default()
            .insert(self.name.clone(), h);
        Ok(())
    }

    fn spawn(&self, name: &str, fut: BoxFuture<'static, Result<(), PluginError>>) {
        let task = tokio::spawn(fut);
        let abort = task.abort_handle();
        let link = Link {
            inner: self.inner.clone(),
            name: self.name.clone(),
            generation: self.generation,
        };
        let label = name.to_string();
        let watcher = tokio::spawn(async move {
            let err = match task.await {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e.0,
                Err(e) if e.is_panic() => "panic".to_string(),
                Err(_) => return,
            };
            link.fail(format!("任务 {label} 失败：{err}"));
        });
        self.add_task(abort);
        self.add_task(watcher.abort_handle());
    }

    fn on_stop(&self, f: StopFn) {
        let mut st = self.inner.state.lock().unwrap();
        if let Some(run) = self.alive(&mut st).and_then(|e| e.run.as_mut()) {
            run.stops.push(f);
        }
    }
}

/// 以 `caller` 的身份调用服务。hub 自己调用时不受限。
pub(crate) async fn call_as(
    inner: &Arc<Inner>,
    caller: &str,
    service: &str,
    provider: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, ServiceError> {
    let access = if caller == HUB {
        TableAccess::All
    } else {
        let st = inner.state.lock().unwrap();
        let e = st
            .plugins
            .get(caller)
            .ok_or_else(|| ServiceError::forbidden(format!("未知插件 {caller}")))?;
        if service == store::SERVICE {
            if !e.manifest.permissions.iter().any(|p| p == "store") {
                return Err(ServiceError::forbidden(format!(
                    "插件 {caller} 没有 store 权限"
                )));
            }
        } else if !e.manifest.requires.contains_key(service) {
            return Err(ServiceError::forbidden(format!(
                "插件 {caller} 的清单没有依赖服务 {service}"
            )));
        }
        TableAccess::Only(e.manifest.tables.clone())
    };
    if service == store::SERVICE {
        return dispatch_store_call(&*inner.store, access, caller, method, params).await;
    }
    if service == hub_admin::SERVICE {
        return admin(inner, method, params).await;
    }
    let h = {
        let st = inner.state.lock().unwrap();
        let ps = st.services.get(service);
        match provider {
            Some(p) => ps.and_then(|ps| ps.get(p)).cloned(),
            None => ps.and_then(|ps| ps.values().next()).cloned(),
        }
    };
    let h = h.ok_or_else(|| ServiceError::unavailable(format!("服务 {service} 不可用")))?;
    let c = Caller {
        plugin: caller.to_string(),
        user_id: OWNER.to_string(),
        trace_id: new_trace_id(),
    };
    h.call(c, method, params).await
}

/// `hub-admin` 1.2。改动只更新状态并通知监督任务，不等待插件启停，
/// 避免调用方（比如 web）自己被停止时互相等待。
async fn admin(inner: &Arc<Inner>, method: &str, params: Value) -> Result<Value, ServiceError> {
    #[derive(Deserialize)]
    struct Name {
        name: String,
    }
    #[derive(Deserialize)]
    struct Enabled {
        name: String,
        enabled: bool,
    }
    let unknown = |n: &str| ServiceError::invalid_params(format!("没有插件 {n}"));
    match method {
        "plugins" => {
            let st = inner.state.lock().unwrap();
            to_result(&PluginList {
                plugins: st.plugins.values().map(Entry::info).collect(),
            })
        }
        "grant" => {
            let p: Name = parse_params(params)?;
            {
                let mut st = inner.state.lock().unwrap();
                let e = st
                    .plugins
                    .get_mut(&p.name)
                    .ok_or_else(|| unknown(&p.name))?;
                e.granted = true;
            }
            db::persist(inner).await?;
            inner.send(Cmd::Reconcile(None));
            Ok(json!({ "ok": true }))
        }
        "set_enabled" => {
            let p: Enabled = parse_params(params)?;
            {
                let mut st = inner.state.lock().unwrap();
                let e = st
                    .plugins
                    .get_mut(&p.name)
                    .ok_or_else(|| unknown(&p.name))?;
                e.enabled = p.enabled;
                if p.enabled {
                    e.retry_at = None;
                    e.failures = 0;
                }
            }
            db::persist(inner).await?;
            inner.send(Cmd::Reconcile(None));
            Ok(json!({ "ok": true }))
        }
        "config_get" => {
            let p: Name = parse_params(params)?;
            let manifest = {
                let st = inner.state.lock().unwrap();
                st.plugins
                    .get(&p.name)
                    .ok_or_else(|| unknown(&p.name))?
                    .manifest
                    .clone()
            };
            let mut config = inner.config_for(&manifest);
            // 密钥字段不经 hub 保存，这里也不返回任何可能的值
            if let Value::Object(m) = &mut config {
                for (k, f) in &manifest.config.fields {
                    if f.ty == "secret" {
                        m.remove(k);
                    }
                }
            }
            let overrides = inner
                .overrides
                .lock()
                .unwrap()
                .get(&p.name)
                .cloned()
                .unwrap_or_default();
            to_result(&PluginConfig {
                name: p.name,
                config,
                overrides: Value::Object(overrides),
            })
        }
        "config_set" => {
            #[derive(Deserialize)]
            struct Set {
                name: String,
                values: serde_json::Map<String, Value>,
            }
            let p: Set = parse_params(params)?;
            {
                let st = inner.state.lock().unwrap();
                let e = st.plugins.get(&p.name).ok_or_else(|| unknown(&p.name))?;
                for (k, v) in &p.values {
                    let f = e.manifest.config.fields.get(k).ok_or_else(|| {
                        ServiceError::invalid_params(format!("插件 {} 没有配置项 {k}", p.name))
                    })?;
                    check_value(k, &f.ty, v)?;
                }
            }
            for (k, v) in &p.values {
                db::set_override(inner, &p.name, k, v).await?;
            }
            {
                let mut ov = inner.overrides.lock().unwrap();
                let m = ov.entry(p.name.clone()).or_default();
                for (k, v) in p.values {
                    if v.is_null() {
                        m.remove(&k);
                    } else {
                        m.insert(k, v);
                    }
                }
            }
            restart(inner, &p.name);
            Ok(json!({ "ok": true }))
        }
        "restart" => {
            let p: Name = parse_params(params)?;
            if !inner.state.lock().unwrap().plugins.contains_key(&p.name) {
                return Err(unknown(&p.name));
            }
            restart(inner, &p.name);
            Ok(json!({ "ok": true }))
        }
        "app_get" => {
            #[derive(Deserialize)]
            struct Key {
                key: String,
            }
            let p: Key = parse_params(params)?;
            if matches!(
                p.key.as_str(),
                hub_admin::app_keys::SHELL | hub_admin::app_keys::UPDATE_STATUS
            ) {
                let v = inner.runtime.lock().unwrap().get(&p.key).cloned();
                return Ok(json!({ "value": v.unwrap_or(Value::Null) }));
            }
            let v = db::get_app(&inner.store, &p.key).await?;
            let value = match v {
                Some(v) => v,
                None if p.key == hub_admin::app_keys::PROXY => json!(inner.proxy),
                None => Value::Null,
            };
            Ok(json!({ "value": value }))
        }
        "app_set" => {
            #[derive(Deserialize)]
            struct KeyValue {
                key: String,
                value: Value,
            }
            let p: KeyValue = parse_params(params)?;
            use hub_admin::app_keys;
            match p.key.as_str() {
                app_keys::PROXY => {
                    if !(p.value.is_null() || p.value.is_string()) {
                        return Err(ServiceError::invalid_params("代理地址必须是字符串"));
                    }
                }
                app_keys::AUTOSTART | app_keys::CLOSE_TO_TRAY | app_keys::AUTO_UPDATE => {
                    if !(p.value.is_null() || p.value.is_boolean()) {
                        return Err(ServiceError::invalid_params("该设置必须是布尔值"));
                    }
                }
                app_keys::UPDATE_ACTION => {
                    if !matches!(p.value["kind"].as_str(), Some("check" | "install"))
                        || !p.value["request_id"].is_string()
                    {
                        return Err(ServiceError::invalid_params("无效的更新操作"));
                    }
                }
                _ => {
                    return Err(ServiceError::invalid_params(format!(
                        "未知的应用设置 {}",
                        p.key
                    )));
                }
            }
            db::set_app(inner, &p.key, &p.value).await?;
            Ok(json!({ "ok": true }))
        }
        other => Err(ServiceError::method_not_found(other)),
    }
}

/// 清掉退避状态后交给监督任务重启。
fn restart(inner: &Arc<Inner>, name: &str) {
    if let Some(e) = inner.state.lock().unwrap().plugins.get_mut(name) {
        e.retry_at = None;
        e.failures = 0;
    }
    inner.send(Cmd::Restart(name.to_string()));
}

/// 按清单里的类型检查一个配置值。null 表示恢复默认，总是允许。密钥不能经这里修改。
fn check_value(key: &str, ty: &str, v: &Value) -> Result<(), ServiceError> {
    let ok = v.is_null()
        || match ty {
            "string" => v.is_string(),
            "integer" => v.is_i64(),
            "boolean" => v.is_boolean(),
            "string_list" => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            "secret" => {
                return Err(ServiceError::invalid_params(format!(
                    "配置项 {key} 是密钥，不能在这里修改"
                )))
            }
            _ => false,
        };
    if ok {
        Ok(())
    } else {
        Err(ServiceError::invalid_params(format!(
            "配置项 {key} 应为 {ty}"
        )))
    }
}

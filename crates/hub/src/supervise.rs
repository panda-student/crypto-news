//! 监督任务：所有启停都在这里串行执行，按依赖顺序启动，失败后退避重启。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plugin_sdk::manifest::version_satisfies;
use plugin_sdk::{Context, HubLink, Plugin, StopFn};
use tokio::sync::mpsc;

use crate::link::Link;
use crate::{db, ext, redact, Cmd, Inner, Phase, Runner, Running};

/// 不由插件提供的服务。
const BUILTIN: &[&str] = &[contracts::store::SERVICE, contracts::hub_admin::SERVICE];

pub(crate) async fn run(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Reconcile(done) => {
                if !inner.state.lock().unwrap().stopped {
                    reconcile(&inner).await;
                }
                if let Some(d) = done {
                    let _ = d.send(());
                }
            }
            Cmd::Failed {
                name,
                generation,
                error,
            } => {
                let current = {
                    let st = inner.state.lock().unwrap();
                    !st.stopped
                        && st.plugins.get(&name).is_some_and(|e| {
                            e.generation == generation && e.phase != Phase::Stopped
                        })
                };
                if current {
                    tracing::warn!("插件 {name} 失败：{}", redact(&error));
                    stop_plugin(&inner, &name).await;
                    mark_failed(&inner, &name, &error);
                    reconcile(&inner).await;
                }
            }
            Cmd::Restart(name) => {
                if !inner.state.lock().unwrap().stopped {
                    stop_plugin(&inner, &name).await;
                    reconcile(&inner).await;
                }
            }
            Cmd::Shutdown(done) => {
                let order = topo(&inner).0;
                for name in order.iter().rev() {
                    stop_plugin(&inner, name).await;
                }
                inner.state.lock().unwrap().stopped = true;
                if let Err(e) = db::persist(&inner).await {
                    tracing::warn!("保存插件状态失败：{}", redact(&e.message));
                }
                let _ = done.send(());
                break;
            }
        }
    }
}

/// 必需依赖决定启动顺序；可选依赖只优先让提供者启动，不能形成硬环。
/// 返回顺序，以及必需依赖环与被该环阻塞的插件。
fn topo(inner: &Inner) -> (Vec<String>, Vec<String>) {
    let st = inner.state.lock().unwrap();
    // 服务 → 提供它的插件
    let mut providers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in st.plugins.values() {
        for svc in e.manifest.provides.keys() {
            providers
                .entry(svc.as_str())
                .or_default()
                .push(e.manifest.name.as_str());
        }
    }
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut hard_deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for e in st.plugins.values() {
        let name = e.manifest.name.as_str();
        let d = deps.entry(name).or_default();
        let hard = hard_deps.entry(name).or_default();
        for (svc, req) in &e.manifest.requires {
            for p in providers.get(svc.as_str()).into_iter().flatten() {
                if *p != name {
                    d.insert(p);
                    if !req.optional() {
                        hard.insert(p);
                    }
                }
            }
        }
    }
    let mut order = Vec::new();
    let mut left = deps.clone();
    loop {
        let mut ready: Vec<&str> = left
            .iter()
            .filter(|(_, d)| d.iter().all(|x| !left.contains_key(x)))
            .map(|(n, _)| *n)
            .collect();
        if ready.is_empty() {
            // 没有满足全部顺序偏好的节点时，只解除一个可选约束。
            // 每轮重新检查完整依赖，尽量保留独立可选提供者先启动的旧顺序。
            if let Some(name) = left
                .keys()
                .find(|name| hard_deps[*name].iter().all(|dep| !left.contains_key(dep)))
            {
                ready.push(name);
            } else {
                break;
            }
        }
        for n in ready {
            left.remove(n);
            order.push(n.to_string());
        }
    }
    let cyclic: Vec<String> = left.keys().map(|s| s.to_string()).collect();
    (order, cyclic)
}

async fn reconcile(inner: &Arc<Inner>) {
    let (order, cyclic) = topo(inner);
    // 计算应该运行的插件
    let desired: BTreeSet<String> = {
        let mut st = inner.state.lock().unwrap();
        let now = Instant::now();
        for n in &cyclic {
            if let Some(e) = st.plugins.get_mut(n) {
                e.missing = vec!["循环依赖".to_string()];
            }
        }
        let mut desired: BTreeSet<String> = BTreeSet::new();
        for name in &order {
            // 先只读算出缺失依赖
            let missing: Vec<String> = {
                let e = &st.plugins[name];
                let mut missing = Vec::new();
                for (svc, req) in &e.manifest.requires {
                    if BUILTIN.contains(&svc.as_str()) || req.optional() || req.all() {
                        continue;
                    }
                    let ok = st.plugins.values().any(|p| {
                        desired.contains(&p.manifest.name)
                            && p.manifest
                                .provides
                                .get(svc)
                                .is_some_and(|v| version_satisfies(v, req.version()))
                    });
                    if !ok {
                        missing.push(format!("{svc} {}", req.version()));
                    }
                }
                missing
            };
            let e = st.plugins.get_mut(name).unwrap();
            e.missing = missing;
            if e.retry_at.is_some_and(|t| t <= now) {
                e.retry_at = None;
            }
            if e.enabled && e.granted && e.retry_at.is_none() && e.missing.is_empty() {
                desired.insert(name.clone());
            }
        }
        desired
    };

    // 先按反序停掉不该运行的
    for name in order.iter().rev() {
        let running = inner.state.lock().unwrap().plugins[name].phase != Phase::Stopped;
        if running && !desired.contains(name) {
            stop_plugin(inner, name).await;
        }
    }
    // 再按顺序启动
    for name in &order {
        let stopped = inner.state.lock().unwrap().plugins[name].phase == Phase::Stopped;
        if stopped && desired.contains(name) {
            if let Err(err) = start_plugin(inner, name).await {
                tracing::warn!("插件 {name} 启动失败：{}", redact(&err));
                mark_failed(inner, name, &err);
                // 依赖它的插件在下一轮里会因缺依赖而停下
                inner.send(Cmd::Reconcile(None));
                break;
            }
        }
    }
    if let Err(e) = db::persist(inner).await {
        tracing::warn!("保存插件状态失败：{}", redact(&e.message));
    }
}

async fn start_plugin(inner: &Arc<Inner>, name: &str) -> Result<(), String> {
    let (generation, runner, manifest) = {
        let mut st = inner.state.lock().unwrap();
        let e = st.plugins.get_mut(name).ok_or("插件不存在")?;
        e.generation += 1;
        e.phase = Phase::Starting;
        e.run = Some(Running::default());
        e.started_at = Some(Instant::now());
        (e.generation, e.runner.clone(), e.manifest.clone())
    };
    let link = Arc::new(Link {
        inner: inner.clone(),
        name: name.to_string(),
        generation,
    });
    let config = inner.config_for(&manifest);
    let r: Result<(), String> = match runner {
        Runner::Official(p) => start_official(inner, link, p, config).await,
        Runner::Extension { dir, exe } => ext::start(link, &dir, &exe, config).await.map(|h| {
            let mut st = inner.state.lock().unwrap();
            if let Some(run) = st.plugins.get_mut(name).and_then(|e| e.run.as_mut()) {
                run.ext = Some(h);
            }
        }),
    };
    match r {
        Ok(()) => {
            let mut st = inner.state.lock().unwrap();
            if let Some(e) = st.plugins.get_mut(name) {
                if e.generation == generation && e.phase == Phase::Starting {
                    e.phase = Phase::Running;
                    e.last_error = None;
                }
            }
            tracing::info!("插件 {name} 已启动");
            Ok(())
        }
        Err(e) => {
            stop_plugin(inner, name).await;
            Err(e)
        }
    }
}

async fn start_official(
    inner: &Arc<Inner>,
    link: Arc<Link>,
    p: Arc<dyn Plugin>,
    config: serde_json::Value,
) -> Result<(), String> {
    let data_dir = inner.data_dir.join("plugin-data").join(&link.name);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        return Err(format!("创建数据目录失败：{e}"));
    }
    let name = link.name.clone();
    let hub_link: Arc<dyn HubLink> = link;
    let ctx = Context::new(hub_link, name, config, inner.http.clone(), data_dir);
    match tokio::time::timeout(Duration::from_secs(30), p.start(ctx)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.0),
        Err(_) => Err("启动超时".to_string()),
    }
}

pub(crate) async fn stop_plugin(inner: &Arc<Inner>, name: &str) {
    let run = {
        let mut st = inner.state.lock().unwrap();
        let Some(e) = st.plugins.get_mut(name) else {
            return;
        };
        let was = e.phase;
        e.phase = Phase::Stopped;
        let run = e.run.take();
        st.unregister(name);
        if was == Phase::Stopped && run.is_none() {
            return;
        }
        run
    };
    let Some(run) = run else { return };
    if let Some(h) = run.ext {
        h.stop().await;
    }
    let stops: Vec<StopFn> = run.stops;
    for f in stops.into_iter().rev() {
        if tokio::time::timeout(Duration::from_secs(5), f())
            .await
            .is_err()
        {
            tracing::warn!("插件 {name} 的停止回调超时");
        }
    }
    for t in run.tasks {
        t.abort();
    }
    tracing::info!("插件 {name} 已停止");
}

/// 记录失败并安排退避重启：1、2、4……秒，最长 5 分钟。运行超过一分钟再失败时从头计。
fn mark_failed(inner: &Arc<Inner>, name: &str, error: &str) {
    let delay = {
        let mut st = inner.state.lock().unwrap();
        let Some(e) = st.plugins.get_mut(name) else {
            return;
        };
        if e.started_at
            .is_some_and(|t| t.elapsed() > Duration::from_secs(60))
        {
            e.failures = 0;
        }
        e.failures += 1;
        let secs = 1u64 << (e.failures - 1).min(9);
        let delay = Duration::from_secs(secs.min(300));
        e.retry_at = Some(Instant::now() + delay);
        e.last_error = Some(redact(error));
        delay
    };
    let inner2 = inner.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        inner2.send(Cmd::Reconcile(None));
    });
}

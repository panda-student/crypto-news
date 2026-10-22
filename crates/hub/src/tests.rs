use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use contracts::{BoxFuture, Intercept, ServiceError};
use plugin_sdk::{Context, Plugin, PluginError};
use serde_json::{json, Value};

use super::*;

type StartFn = Arc<dyn Fn(Context) -> BoxFuture<'static, Result<(), PluginError>> + Send + Sync>;

struct TestPlugin {
    manifest: String,
    starts: Arc<AtomicUsize>,
    start: StartFn,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> &str {
        &self.manifest
    }
    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        (self.start)(ctx)
    }
}

fn plugin<F, Fut>(manifest: &str, f: F) -> (Arc<TestPlugin>, Arc<AtomicUsize>)
where
    F: Fn(Context) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), PluginError>> + Send + 'static,
{
    let starts = Arc::new(AtomicUsize::new(0));
    let p = Arc::new(TestPlugin {
        manifest: manifest.to_string(),
        starts: starts.clone(),
        start: Arc::new(move |ctx| Box::pin(f(ctx))),
    });
    (p, starts)
}

async fn new_hub() -> Hub {
    let store = store::open_memory().await.unwrap();
    let dir = std::env::temp_dir().join(format!("hub-test-{}", contracts::new_trace_id()));
    Hub::new(HubOptions {
        store,
        http: plugin_sdk::build_http_client("http://127.0.0.1:9").unwrap(),
        proxy: "http://127.0.0.1:9".into(),
        data_dir: dir,
        configs: BTreeMap::new(),
        event_retention_days: 7,
    })
}

fn status(hub: &Hub, name: &str) -> String {
    hub.plugins()
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.status)
        .unwrap()
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("等待超时");
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_in_dependency_order_and_reports_missing() {
    let hub = new_hub().await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let o = order.clone();
    let (user, _) = plugin(
        "name = \"user\"\nversion = \"0.1.0\"\n[requires]\necho = \"1.0\"\n",
        move |_| {
            o.lock().unwrap().push("user");
            async { Ok(()) }
        },
    );
    let o = order.clone();
    let (echo, _) = plugin(
        "name = \"echo\"\nversion = \"0.1.0\"\n[provides]\necho = \"1.2\"\n",
        move |ctx| {
            o.lock().unwrap().push("echo");
            async move {
                ctx.provide_fn("echo", |c, m, p| async move {
                    Ok(json!({ "caller": c.plugin, "method": m, "params": p }))
                })?;
                Ok(())
            }
        },
    );
    let (lonely, _) = plugin(
        "name = \"lonely\"\nversion = \"0.1.0\"\n[requires]\nnothing = \"1.0\"\n",
        |_| async { Ok(()) },
    );
    hub.add_official(user).unwrap();
    hub.add_official(echo).unwrap();
    hub.add_official(lonely).unwrap();
    hub.start().await.unwrap();

    assert_eq!(*order.lock().unwrap(), vec!["echo", "user"]);
    assert_eq!(status(&hub, "user"), "运行中");
    assert_eq!(status(&hub, "lonely"), "缺依赖");
    let r = hub.call("echo", "hi", json!(1)).await.unwrap();
    assert_eq!(r, json!({ "caller": "hub", "method": "hi", "params": 1 }));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_plugin_is_restarted_after_backoff() {
    let hub = new_hub().await;
    let (p, starts) = plugin(
        "name = \"flaky\"\nversion = \"0.1.0\"\n",
        |ctx| async move {
            let n = ctx.data_dir().join("n");
            let first = !n.exists();
            std::fs::write(&n, "1").unwrap();
            if first {
                ctx.spawn("boom", async { Err(PluginError::new("坏了")) });
            }
            Ok(())
        },
    );
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    // 第一次运行的任务失败，1 秒后重启
    wait_until(|| starts.load(Ordering::SeqCst) >= 2).await;
    wait_until(|| status(&hub, "flaky") == "运行中").await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn start_error_is_recorded_and_redacted() {
    let hub = new_hub().await;
    let (p, _) = plugin("name = \"bad\"\nversion = \"0.1.0\"\n", |_| async {
        Err(PluginError::new(
            "key=sk0123456789abcdefghijklmnopqrstuv 无效",
        ))
    });
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    let info = hub.plugins().into_iter().find(|p| p.name == "bad").unwrap();
    assert_eq!(info.status, "失败退避");
    let err = info.last_error.unwrap();
    assert!(err.contains("***"), "{err}");
    assert!(!err.contains("sk0123456789"), "{err}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn events_are_delivered_retried_and_cursor_advances() {
    let hub = new_hub().await;
    let seen = Arc::new(Mutex::new(Vec::<i64>::new()));
    let fails = Arc::new(AtomicUsize::new(0));
    let (s, f) = (seen.clone(), fails.clone());
    let (sub, _) = plugin(
        "name = \"sub\"\nversion = \"0.1.0\"\n[events]\nsubscribe = [\"ping\"]\n",
        move |ctx| {
            let (s, f) = (s.clone(), f.clone());
            async move {
                ctx.subscribe("ping", move |ev| {
                    let (s, f) = (s.clone(), f.clone());
                    async move {
                        // 第一个事件先失败一次
                        if ev.payload == json!(1) && f.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Err(PluginError::new("稍后再试"));
                        }
                        s.lock().unwrap().push(ev.payload.as_i64().unwrap());
                        Ok(())
                    }
                })
            }
        },
    );
    hub.add_official(sub).unwrap();
    hub.start().await.unwrap();
    hub.publish("ping", json!(1)).await.unwrap();
    hub.publish("ping", json!(2)).await.unwrap();
    wait_until(|| seen.lock().unwrap().len() == 2).await;
    assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    assert_eq!(fails.load(Ordering::SeqCst), 2);

    let rows = hub
        .inner
        .store
        .query(
            contracts::store::TableAccess::All,
            contracts::store::Statement::new(
                "SELECT last_id FROM event_cursors WHERE plugin = 'sub' AND kind = 'ping'",
                vec![],
            ),
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get("last_id"), Some(&json!(2)));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_are_enforced() {
    let hub = new_hub().await;
    let results = Arc::new(Mutex::new(Vec::<String>::new()));
    let r = results.clone();
    let (p, _) = plugin("name = \"nosy\"\nversion = \"0.1.0\"\n", move |ctx| {
        let r = r.clone();
        async move {
            let mut out = Vec::new();
            let e = ctx
                .store()
                .0
                .call("query", json!({ "sql": "SELECT 1", "params": [] }))
                .await;
            out.push(format!("store:{}", e.is_err()));
            let e = ctx.service("echo").call("x", json!(null)).await;
            out.push(format!("requires:{}", e.is_err()));
            out.push(format!(
                "publish:{}",
                ctx.publish("ping", &json!(1)).await.is_err()
            ));
            out.push(format!(
                "provide:{}",
                ctx.provide_fn("echo", |_, _, _| async { Ok(Value::Null) })
                    .is_err()
            ));
            out.push(format!(
                "subscribe:{}",
                ctx.subscribe("ping", |_| async { Ok(()) }).is_err()
            ));
            *r.lock().unwrap() = out;
            Ok(())
        }
    });
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    assert_eq!(
        *results.lock().unwrap(),
        vec![
            "store:true",
            "requires:true",
            "publish:true",
            "provide:true",
            "subscribe:true"
        ]
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_disables_and_reenables() {
    let hub = new_hub().await;
    let (p, starts) = plugin("name = \"toggle\"\nversion = \"0.1.0\"\n", |_| async {
        Ok(())
    });
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    assert_eq!(status(&hub, "toggle"), "运行中");

    hub.call(
        contracts::hub_admin::SERVICE,
        "set_enabled",
        json!({ "name": "toggle", "enabled": false }),
    )
    .await
    .unwrap();
    wait_until(|| status(&hub, "toggle") == "已停用").await;
    hub.call(
        contracts::hub_admin::SERVICE,
        "set_enabled",
        json!({ "name": "toggle", "enabled": true }),
    )
    .await
    .unwrap();
    wait_until(|| status(&hub, "toggle") == "运行中").await;
    assert_eq!(starts.load(Ordering::SeqCst), 2);

    let list = hub
        .call(contracts::hub_admin::SERVICE, "plugins", json!({}))
        .await
        .unwrap();
    assert_eq!(list["plugins"][0]["name"], "toggle");
    let e: ServiceError = hub
        .call(
            contracts::hub_admin::SERVICE,
            "grant",
            json!({ "name": "nope" }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::invalid_params("").code);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn intercept_runs_in_name_order_and_can_stop() {
    let hub = new_hub().await;
    let (a, _) = plugin(
        "name = \"a-add\"\nversion = \"0.1.0\"\n[events]\nintercept = [\"msg\"]\n",
        |ctx| async move {
            ctx.intercept("msg", |ev| async move {
                let s = ev.payload.as_str().unwrap_or("").to_string();
                if s == "stop" {
                    return Ok(Intercept::Stop);
                }
                Ok(Intercept::Continue(json!(format!("{s}a"))))
            })
        },
    );
    let (b, _) = plugin(
        "name = \"b-add\"\nversion = \"0.1.0\"\n[events]\nintercept = [\"msg\"]\n",
        |ctx| async move {
            ctx.intercept("msg", |ev| async move {
                let s = ev.payload.as_str().unwrap_or("").to_string();
                Ok(Intercept::Continue(json!(format!("{s}b"))))
            })
        },
    );
    let out = Arc::new(Mutex::new(Vec::new()));
    let o = out.clone();
    let (src, _) = plugin(
        "name = \"src\"\nversion = \"0.1.0\"\n[events]\npublish = [\"msg\"]\n",
        move |ctx| {
            let o = o.clone();
            async move {
                let x = ctx.emit_intercept("msg", json!("x"), None).await?;
                let y = ctx.emit_intercept("msg", json!("stop"), None).await?;
                o.lock().unwrap().extend([x, y]);
                Ok(())
            }
        },
    );
    hub.add_official(b).unwrap();
    hub.add_official(a).unwrap();
    hub.add_official(src).unwrap();
    hub.start().await.unwrap();
    assert_eq!(*out.lock().unwrap(), vec![Some(json!("xab")), None]);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ephemeral_events_skip_the_log() {
    let hub = new_hub().await;
    let got = Arc::new(AtomicUsize::new(0));
    let g = got.clone();
    let (sub, _) = plugin(
        "name = \"sub\"\nversion = \"0.1.0\"\n[events]\nsubscribe = [\"tick\"]\n",
        move |ctx| {
            let g = g.clone();
            async move {
                ctx.subscribe("tick", move |_| {
                    let g = g.clone();
                    async move {
                        g.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
            }
        },
    );
    let ids = Arc::new(Mutex::new(Vec::new()));
    let i = ids.clone();
    let (src, _) = plugin(
        "name = \"src\"\nversion = \"0.1.0\"\n[events]\npublish = [\"tick\"]\nephemeral = [\"tick\"]\n",
        move |ctx| {
            let i = i.clone();
            async move {
                // 订阅者按名字排在后面，事件在它启动之后再发
                let c = ctx.clone();
                ctx.spawn("tick", async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let id = c.publish("tick", &json!(1)).await?;
                    i.lock().unwrap().push(id);
                    std::future::pending::<()>().await;
                    Ok(())
                });
                Ok(())
            }
        },
    );
    hub.add_official(sub).unwrap();
    hub.add_official(src).unwrap();
    hub.start().await.unwrap();
    wait_until(|| got.load(Ordering::SeqCst) == 1).await;
    assert_eq!(*ids.lock().unwrap(), vec![0]);
    let rows = hub
        .inner
        .store
        .query(
            contracts::store::TableAccess::All,
            contracts::store::Statement::new("SELECT COUNT(*) AS n FROM event_log", vec![]),
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get("n"), Some(&json!(0)));
    hub.shutdown().await;
}

#[test]
fn duplicate_and_reserved_names_are_rejected() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let hub = new_hub().await;
        let (a, _) = plugin("name = \"dup\"\nversion = \"0.1.0\"\n", |_| async {
            Ok(())
        });
        let (b, _) = plugin("name = \"dup\"\nversion = \"0.1.0\"\n", |_| async {
            Ok(())
        });
        let (c, _) = plugin(
            "name = \"t\"\nversion = \"0.1.0\"\ntables = [\"event_log\"]\n",
            |_| async { Ok(()) },
        );
        let (d, _) = plugin(
            "name = \"s\"\nversion = \"0.1.0\"\n[provides]\nstore = \"1.0\"\n",
            |_| async { Ok(()) },
        );
        hub.add_official(a).unwrap();
        assert!(matches!(hub.add_official(b), Err(HubError::Duplicate(_))));
        assert!(matches!(hub.add_official(c), Err(HubError::Manifest(_))));
        assert!(matches!(hub.add_official(d), Err(HubError::Manifest(_))));
    });
}

#[test]
fn redact_hides_long_tokens() {
    assert_eq!(redact("短的 abc 保留"), "短的 abc 保留");
    assert_eq!(
        redact("key=ABCDEFGHIJKLMNOPQRSTUVWXYZ0123 end"),
        "key=*** end"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_config_set_restarts_with_new_values() {
    let hub = new_hub().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let (p, starts) = plugin(
        "name = \"cfg\"\nversion = \"0.1.0\"\ndescription = \"配置测试\"\n\
         [config.fields.interval]\ntype = \"integer\"\ndefault = 60\n\
         [config.fields.token]\ntype = \"secret\"\n",
        move |ctx| {
            s.lock().unwrap().push(ctx.config().clone());
            async { Ok(()) }
        },
    );
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    let admin = |m: &'static str, p: Value| hub.call(contracts::hub_admin::SERVICE, m, p);

    let info = &hub.plugins()[0];
    assert_eq!(info.description.as_deref(), Some("配置测试"));
    let c = admin("config_get", json!({ "name": "cfg" })).await.unwrap();
    assert_eq!(c["config"], json!({ "interval": 60 }));
    assert_eq!(c["overrides"], json!({}));

    admin(
        "config_set",
        json!({ "name": "cfg", "values": { "interval": 5 } }),
    )
    .await
    .unwrap();
    wait_until(|| starts.load(Ordering::SeqCst) == 2 && status(&hub, "cfg") == "运行中").await;
    assert_eq!(seen.lock().unwrap()[1], json!({ "interval": 5 }));
    let c = admin("config_get", json!({ "name": "cfg" })).await.unwrap();
    assert_eq!(c["overrides"], json!({ "interval": 5 }));

    // 类型不对、未知字段、密钥字段都拒绝，且不重启
    for values in [
        json!({ "interval": "x" }),
        json!({ "nope": 1 }),
        json!({ "token": "abc" }),
    ] {
        let e = admin("config_set", json!({ "name": "cfg", "values": values }))
            .await
            .unwrap_err();
        assert_eq!(e.code, ServiceError::invalid_params("").code);
    }

    // null 恢复默认
    admin(
        "config_set",
        json!({ "name": "cfg", "values": { "interval": null } }),
    )
    .await
    .unwrap();
    wait_until(|| starts.load(Ordering::SeqCst) == 3 && status(&hub, "cfg") == "运行中").await;
    assert_eq!(seen.lock().unwrap()[2], json!({ "interval": 60 }));

    admin("restart", json!({ "name": "cfg" })).await.unwrap();
    wait_until(|| starts.load(Ordering::SeqCst) == 4 && status(&hub, "cfg") == "运行中").await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_overrides_and_app_settings_persist() {
    let store: contracts::store::SharedStore = store::open_memory().await.unwrap();
    let opts = |store: contracts::store::SharedStore| HubOptions {
        store,
        http: plugin_sdk::build_http_client("http://127.0.0.1:9").unwrap(),
        proxy: "http://127.0.0.1:9".into(),
        data_dir: std::env::temp_dir().join(format!("hub-test-{}", contracts::new_trace_id())),
        configs: BTreeMap::from([("cfg".to_string(), json!({ "interval": 30 }))]),
        event_retention_days: 7,
    };
    let manifest = "name = \"cfg\"\nversion = \"0.1.0\"\n\
                    [config.fields.interval]\ntype = \"integer\"\ndefault = 60\n";

    let hub = Hub::new(opts(store.clone()));
    let (p, _) = plugin(manifest, |_| async { Ok(()) });
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    let admin = |m: &'static str, p: Value| hub.call(contracts::hub_admin::SERVICE, m, p);
    // 没设过代理时返回当前生效的值
    let v = admin("app_get", json!({ "key": "proxy" })).await.unwrap();
    assert_eq!(v["value"], "http://127.0.0.1:9");
    admin("app_set", json!({ "key": "proxy", "value": "" }))
        .await
        .unwrap();
    let e = admin("app_set", json!({ "key": "other", "value": 1 }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ServiceError::invalid_params("").code);
    // 桌面设置只接受布尔值；shell 是只读运行时信息
    admin("app_set", json!({ "key": "autostart", "value": true }))
        .await
        .unwrap();
    assert!(
        admin("app_set", json!({ "key": "close_to_tray", "value": "x" }))
            .await
            .is_err()
    );
    assert!(
        admin("app_set", json!({ "key": "shell", "value": "desktop" }))
            .await
            .is_err()
    );
    let v = admin("app_get", json!({ "key": "shell" })).await.unwrap();
    assert_eq!(v["value"], Value::Null);
    hub.set_runtime("shell", json!("desktop"));
    let v = admin("app_get", json!({ "key": "shell" })).await.unwrap();
    assert_eq!(v["value"], "desktop");
    let v = admin("app_get", json!({ "key": "autostart" }))
        .await
        .unwrap();
    assert_eq!(v["value"], true);
    admin(
        "config_set",
        json!({ "name": "cfg", "values": { "interval": 7 } }),
    )
    .await
    .unwrap();
    hub.shutdown().await;

    assert_eq!(
        read_app_setting(&store, "proxy").await.unwrap(),
        Some(json!(""))
    );
    let seen = Arc::new(Mutex::new(Value::Null));
    let s = seen.clone();
    let hub = Hub::new(opts(store));
    let (p, _) = plugin(manifest, move |ctx| {
        *s.lock().unwrap() = ctx.config().clone();
        async { Ok(()) }
    });
    hub.add_official(p).unwrap();
    hub.start().await.unwrap();
    assert_eq!(*seen.lock().unwrap(), json!({ "interval": 7 }));
    hub.shutdown().await;
}

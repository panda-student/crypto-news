//! 本机网页界面：以消息流为中心的动态页，加统计、设置，以及插件、信源等管理页。

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use plugin_sdk::contracts::auth::AuthClient;
use plugin_sdk::contracts::ServiceError;
use plugin_sdk::{BoxFuture, Context, Plugin, PluginError};
mod chat;
mod dash;
mod data;
mod feed;
mod focus;
mod mcp;
mod opener;
mod pages;
mod qr;
mod settings;
mod stats;
mod status;
mod workspace;

use serde::Deserialize;

const MANIFEST: &str = include_str!("../plugin.toml");
const HTMX: &str = include_str!("../assets/htmx.min.js");
const CSS: &str = include_str!("../assets/app.css");
const APP_ICON: &str = include_str!("../assets/app-icon.svg");
const SETTINGS_CSS: &str = include_str!("../assets/settings.css");
const DEFAULT_LISTEN: &str = "127.0.0.1:8730";

pub struct WebPlugin;

#[derive(Deserialize, Default)]
struct WebConfig {
    listen: Option<String>,
}

impl Plugin for WebPlugin {
    fn manifest(&self) -> &str {
        MANIFEST
    }

    fn start(&self, ctx: Context) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let cfg: WebConfig = ctx.config_as()?;
            let listen = cfg.listen.unwrap_or_else(|| DEFAULT_LISTEN.to_string());
            let addr: SocketAddr = listen
                .parse()
                .map_err(|_| PluginError::new(format!("listen 不是有效地址：{listen}")))?;
            if !addr.ip().is_loopback() {
                return Err(PluginError::new(format!(
                    "listen 必须是回环地址，当前为 {addr}"
                )));
            }
            ctx.store()
                .migrate(1, data::SCHEMA)
                .await
                .map_err(|e| PluginError::new(format!("初始化数据表失败：{}", e.message)))?;
            ctx.store().migrate(2, data::CONVERSATION_SCHEMA).await?;
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| PluginError::new(format!("监听 {addr} 失败：{e}")))?;
            if let Ok(a) = listener.local_addr() {
                tracing::info!("网页界面监听 http://{a}");
            }
            let app = try_router_with(ctx.clone(), Arc::new(opener::SystemOpener))?;
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
            ctx.spawn("http", async move {
                let r = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stop_rx.await;
                    })
                    .await;
                let _ = done_tx.send(());
                r.map_err(|e| PluginError::new(format!("HTTP 服务出错：{e}")))
            });
            ctx.on_stop(move || async move {
                let _ = stop_tx.send(());
                // 等端口释放；长连接不让它无限拖住停止流程
                let _ = tokio::time::timeout(Duration::from_secs(2), done_rx).await;
            });
            Ok(())
        })
    }
}

pub(crate) struct AppState {
    pub(crate) ctx: Context,
    pub(crate) opener: Arc<dyn opener::Opener>,
    pub(crate) chat: chat::ChatState,
    pub(crate) status: status::Coordinator,
}
pub(crate) type Shared = State<Arc<AppState>>;

/// 构建路由，便于测试。
pub fn router(ctx: Context) -> Router {
    router_with(ctx, Arc::new(opener::SystemOpener))
}

fn router_with(ctx: Context, opener: Arc<dyn opener::Opener>) -> Router {
    try_router_with(ctx, opener)
        .expect("workspace-state service must be declared in web/plugin.toml")
}

fn try_router_with(ctx: Context, opener: Arc<dyn opener::Opener>) -> Result<Router, PluginError> {
    let chat = chat::ChatState::default();
    chat.subscribe(&ctx);
    let st = Arc::new(AppState {
        ctx,
        opener,
        chat,
        status: status::Coordinator::default(),
    });
    workspace::register(&st)?;
    let sync_state = st.clone();
    st.ctx.spawn("conversation-sync", async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = chat::sync_shared(&sync_state).await;
        }
        #[allow(unreachable_code)]
        Ok(())
    });
    Ok(app_routes(st))
}

/// The GUI and confirmed assistant actions enter these same handlers and guards.
pub(crate) fn app_routes(st: Arc<AppState>) -> Router {
    business_routes(st.clone()).merge(
        Router::new()
            .route("/api/v1/mcp", post(mcp::rpc))
            .route("/api/v1/mcp/plans", get(mcp::plans))
            .route("/api/v1/mcp/plans/{id}/confirm", post(mcp::confirm))
            .route("/api/v1/mcp/plans/{id}/cancel", post(mcp::cancel))
            .route("/settings/mcp", get(mcp::settings))
            .with_state(st),
    )
}
pub(crate) fn business_routes(st: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(feed::home))
        .route("/feed/new", get(feed::feed_new))
        .route("/stats", get(stats::stats_page))
        .route("/settings", get(stats::settings_page))
        .route("/settings/panel", get(stats::settings_panel))
        .route("/api/v1/stats", get(stats::api_stats))
        .route(
            "/api/v1/prefs/watch_assets",
            get(stats::api_watch_get).post(stats::api_watch_assets),
        )
        .route("/api/v1/items/read_all", post(feed::api_read_all))
        .route("/api/v1/items/{id}/read", post(feed::api_read))
        .route("/api/v1/items/{id}/star", post(feed::api_star))
        .route("/api/v1/items/{id}/open", post(feed::api_open))
        .route("/api/v1/push/settings", get(focus::get).post(focus::save))
        .route("/api/v1/dash", get(dash::api_dash))
        .route("/api/v1/feed/history", get(dash::api_history))
        .route("/api/v1/chat", post(chat::api_chat))
        .route("/api/v1/chat/context", post(chat::api_context))
        .route("/api/v1/chat/sessions", get(chat::api_sessions))
        .route("/api/v1/chat/session/{id}", get(chat::api_session))
        .route("/api/v1/chat/stream/{id}", get(chat::api_stream))
        .route("/api/v1/chat/stream/{id}/cancel", post(chat::api_cancel))
        .route("/api/v1/chat/undo/{token}", post(chat::api_undo))
        .route("/api/v1/chat/{id}/rename", post(chat::api_rename))
        .route("/api/v1/chat/{id}/delete", post(chat::api_delete))
        .route("/rules", get(pages::rules_page))
        .route("/items", get(pages::items_page))
        .route("/channels", get(pages::channels_page))
        .route(
            "/channels/{id}/bind_status",
            get(pages::channel_bind_status),
        )
        .route("/outbox", get(pages::outbox_page))
        .route("/outbox/table", get(pages::outbox_table))
        .route("/digests", get(pages::digests_page))
        .route("/digests/preview", get(pages::digests_preview))
        .route("/secrets", get(pages::secrets_page))
        .route("/usage", get(pages::usage_page))
        .route(
            "/api/v1/rules",
            get(pages::api_rules_get).post(pages::api_rules_set),
        )
        .route("/api/v1/items", get(pages::api_items))
        .route("/api/v1/items/{id}/level", post(pages::api_item_level))
        .route("/api/v1/channels", get(pages::api_channels))
        .route("/api/v1/status", get(status::get))
        .route("/api/v1/channels/{id}/test", post(pages::api_channel_test))
        .route("/api/v1/channels/{id}/bind", post(pages::api_channel_bind))
        .route("/api/v1/outbox", get(pages::api_outbox))
        .route("/api/v1/outbox/{id}/retry", post(pages::api_outbox_retry))
        .route("/api/v1/digests", get(pages::api_digests))
        .route("/api/v1/digests/preview", get(pages::api_digests_preview))
        .route(
            "/api/v1/secrets",
            get(pages::api_secrets).post(pages::api_secret_set),
        )
        .route("/api/v1/secrets/delete", post(pages::api_secret_delete))
        .route("/api/v1/usage", get(pages::api_usage))
        .route(
            "/assets/htmx.min.js",
            get(|| async { asset("text/javascript", HTMX) }),
        )
        .route(
            "/assets/layout-resize.js",
            get(|| async {
                asset(
                    "text/javascript",
                    include_str!("../assets/layout-resize.js"),
                )
            }),
        )
        .route(
            "/assets/layout-resize.css",
            get(|| async { asset("text/css", include_str!("../assets/layout-resize.css")) }),
        )
        .route("/assets/app.css", get(|| async { asset("text/css", CSS) }))
        .route(
            "/assets/mcp-ui.js",
            get(|| async { asset("text/javascript", include_str!("../assets/mcp-ui.js")) }),
        )
        .route(
            "/assets/settings-ui.js",
            get(|| async { asset("text/javascript", include_str!("../assets/settings-ui.js")) }),
        )
        .route(
            "/assets/watch-assets.js",
            get(|| async { asset("text/javascript", include_str!("../assets/watch-assets.js")) }),
        )
        .route(
            "/assets/app-status.js",
            get(|| async {
                asset(
                    "application/javascript",
                    include_str!("../assets/app-status.js"),
                )
            }),
        )
        .route(
            "/assets/home.css",
            get(|| async { asset("text/css", include_str!("../assets/home.css")) }),
        )
        .route(
            "/assets/main-v17.css",
            get(|| async { asset("text/css", include_str!("../assets/main-v17.css")) }),
        )
        .route(
            "/assets/app-icon.svg",
            get(|| async { asset("image/svg+xml", APP_ICON) }),
        )
        .route(
            "/assets/settings.css",
            get(|| async { asset("text/css", SETTINGS_CSS) }),
        )
        .merge(settings::routes())
        .with_state(st)
}

fn asset(ct: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ct)], body).into_response()
}

fn env() -> &'static minijinja::Environment<'static> {
    static ENV: OnceLock<minijinja::Environment<'static>> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut e = minijinja::Environment::new();
        for (n, s) in [
            ("base.html", include_str!("../templates/base.html")),
            ("main.html", include_str!("../templates/main.html")),
            ("stats.html", include_str!("../templates/stats.html")),
            ("rules.html", include_str!("../templates/rules.html")),
            ("msg.html", include_str!("../templates/msg.html")),
            ("items.html", include_str!("../templates/items.html")),
            ("item_row.html", include_str!("../templates/item_row.html")),
            ("channels.html", include_str!("../templates/channels.html")),
            ("bind.html", include_str!("../templates/bind.html")),
            ("outbox.html", include_str!("../templates/outbox.html")),
            (
                "outbox_table.html",
                include_str!("../templates/outbox_table.html"),
            ),
            ("digests.html", include_str!("../templates/digests.html")),
            (
                "digest_preview.html",
                include_str!("../templates/digest_preview.html"),
            ),
            ("secrets.html", include_str!("../templates/secrets.html")),
            (
                "secret_row.html",
                include_str!("../templates/secret_row.html"),
            ),
            ("usage.html", include_str!("../templates/usage.html")),
        ] {
            e.add_template(n, s).expect("模板有效");
        }
        for (n, s) in settings::TEMPLATES {
            e.add_template(n, s).expect("模板有效");
        }
        e
    })
}

pub(crate) fn render(name: &str, ctx: minijinja::Value) -> Response {
    match env().get_template(name).and_then(|t| t.render(ctx)) {
        Ok(s) => Html(s).into_response(),
        Err(e) => {
            tracing::error!("模板渲染失败 {name}：{e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "页面渲染失败").into_response()
        }
    }
}

pub(crate) fn err_response(e: ServiceError) -> Response {
    let status = match e.code {
        ServiceError::UNAVAILABLE => StatusCode::SERVICE_UNAVAILABLE,
        ServiceError::FORBIDDEN => StatusCode::FORBIDDEN,
        ServiceError::INVALID_PARAMS => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.message).into_response()
}

/// 每个请求先过 auth；false 返回 403，auth 不可用返回 503。
pub(crate) async fn check(st: &AppState, action: &str) -> Option<Response> {
    match AuthClient(st.ctx.service("auth")).check(action).await {
        Ok(true) => None,
        Ok(false) => Some((StatusCode::FORBIDDEN, "没有权限").into_response()),
        Err(e) => {
            tracing::warn!("auth 检查失败：{e}");
            Some((StatusCode::SERVICE_UNAVAILABLE, "认证服务不可用").into_response())
        }
    }
}

#[cfg(test)]
mod feed_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    fn plugin_json() -> Value {
        json!({"name":"hello-ext","version":"0.1.0","kind":"extension","status":"待授权",
            "missing":["store"],"permissions":["network"],"granted":false,"enabled":true,
            "last_error":"<boom>"})
    }

    struct Fixture {
        base: String,
        calls: Arc<Mutex<Vec<String>>>,
        client: reqwest::Client,
        _hub: Arc<TestHub>,
    }

    async fn serve(hub: &Arc<TestHub>) -> String {
        let ctx = hub.context("web", json!({}), test_http_client());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(ctx);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn setup(allowed: bool, with_status: bool) -> Fixture {
        let hub = TestHub::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c = calls.clone();
        hub.register_fn("hub", "hub-admin", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("{m} {p}"));
                match m.as_str() {
                    "plugins" => Ok(json!({ "plugins": [plugin_json()] })),
                    _ => Ok(json!({})),
                }
            }
        });
        hub.register_fn("auth", "auth", move |_m, _p| async move {
            Ok(json!({ "allowed": allowed }))
        });
        if with_status {
            hub.register_fn("collect", "source-status", |_m, _p| async move {
                Ok(json!({"sources":[{"id":"rss:sec","name":"SEC","state":"ok",
                    "last_success_at":"2026-01-01T00:00:00Z","last_error":null,
                    "consecutive_failures":0,"total_items":7,"interval_secs":300}]}))
            });
        }
        let base = serve(&hub).await;
        Fixture {
            base,
            calls,
            client: test_http_client(),
            _hub: hub,
        }
    }

    #[tokio::test]
    async fn pages_and_json() {
        let f = setup(true, true).await;
        let get = |p: &str| f.client.get(format!("{}{p}", f.base)).send();
        let r = get("/settings/plugins").await.unwrap();
        assert_eq!(r.status(), 200);
        let body = r.text().await.unwrap();
        assert!(body.contains("hello-ext"));
        assert!(body.contains("/api/v1/plugins/hello-ext/grant"));
        assert!(body.contains("&lt;boom&gt;"), "应转义");
        let body = get("/settings/sources")
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("SEC") && body.contains("/settings/sources/rss%3Asec/retry"));
        let v: Value = get("/api/v1/plugins").await.unwrap().json().await.unwrap();
        assert_eq!(v["plugins"][0]["name"], "hello-ext");
        let v: Value = get("/api/v1/sources").await.unwrap().json().await.unwrap();
        assert_eq!(v["sources"][0]["total_items"], 7);
        assert_eq!(get("/assets/htmx.min.js").await.unwrap().status(), 200);
        assert_eq!(get("/assets/app.css").await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn grant_and_enabled_call_hub_admin() {
        let f = setup(true, true).await;
        let r = f
            .client
            .post(format!("{}/api/v1/plugins/hello-ext/grant", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("hello-ext"));
        let r = f
            .client
            .post(format!("{}/api/v1/plugins/hello-ext/enabled", f.base))
            .form(&[("enabled", "false")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let calls = f.calls.lock().unwrap().clone();
        assert!(calls
            .iter()
            .any(|c| c.starts_with("grant") && c.contains("hello-ext")));
        assert!(calls
            .iter()
            .any(|c| c.starts_with("set_enabled") && c.contains("false")));
    }

    #[tokio::test]
    async fn auth_denied_is_403() {
        let f = setup(false, true).await;
        for p in [
            "/",
            "/settings/plugins",
            "/settings/sources",
            "/settings/llm",
            "/settings/push",
            "/settings/system",
            "/api/v1/plugins",
            "/api/v1/sources",
        ] {
            let r = f.client.get(format!("{}{p}", f.base)).send().await.unwrap();
            assert_eq!(r.status(), 403, "{p}");
        }
        let r = f
            .client
            .post(format!("{}/api/v1/plugins/x/grant", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn auth_missing_is_503_and_status_missing_shows_note() {
        let hub = TestHub::new();
        let base = serve(&hub).await;
        let r = test_http_client()
            .get(format!("{base}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503);

        let f = setup(true, false).await;
        let r = f
            .client
            .get(format!("{}/settings/sources", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("不可用"));
    }

    #[tokio::test]
    async fn rejects_non_loopback_and_stops_cleanly() {
        let hub = TestHub::new();
        let ctx = hub.context("web", json!({"listen":"0.0.0.0:0"}), test_http_client());
        assert!(WebPlugin.start(ctx).await.is_err());

        hub.with_store(store::open_memory().await.unwrap());
        let ctx = hub.context("web", json!({"listen":"127.0.0.1:0"}), test_http_client());
        WebPlugin.start(ctx).await.unwrap();
        hub.stop().await;
        assert!(hub.failures().is_empty());
    }

    // ---------------------------------------------------------------- 阶段 2/3

    const FAKE_SECRET: &str = "sk-FAKE-SECRET-VALUE";

    struct Full {
        base: String,
        calls: Arc<Mutex<Vec<String>>>,
        client: reqwest::Client,
        _hub: Arc<TestHub>,
    }

    fn channel_info(id: &str, state: &str) -> Value {
        json!({"id":id,"name":format!("名字-{id}"),"format":"markdown","max_bytes":2000,
            "backup":id=="wechat-ilink","state":state,"quota_left":9,
            "expires_at":"2026-12-01T00:00:00Z","detail":"细节说明"})
    }

    async fn setup_full(allowed: bool) -> Full {
        let hub = TestHub::new();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        hub.register_fn("hub", "hub-admin", |_m, _p| async move {
            Ok(json!({ "plugins": [] }))
        });
        hub.register_fn("auth", "auth", move |_m, _p| async move {
            Ok(json!({ "allowed": allowed }))
        });
        let c = calls.clone();
        hub.register_fn("rulebook", "rulebook", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("rulebook.{m} {p}"));
                Ok(json!({"text":"规则<正文>","updated_at":"2026-01-02T03:04:05Z"}))
            }
        });
        let c = calls.clone();
        hub.register_fn("screen", "screening", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("screening.{m} {p}"));
                let s = json!({"item_id":1,"level":"important","score":0.8,"reason":"理由甲",
                    "impact":"影响乙","status":"done","method":"llm","attempts":0,
                    "backfill":false,"screened_at":"2026-01-01T00:00:00Z"});
                match m.as_str() {
                    "review" => Ok(s),
                    _ => Ok(json!({ "screenings": [s] })),
                }
            }
        });
        let c = calls.clone();
        hub.register_fn("store-items", "item-query", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("item-query.{m} {p}"));
                Ok(
                    json!({"items":[{"id":1,"source_id":"rss:sec","url":"https://e.com/a",
                    "title":"标题<甲>","summary":null,"published_at":null,
                    "collected_at":"2026-01-01T00:00:00Z"}]}),
                )
            }
        });
        let c = calls.clone();
        hub.register_fn("dispatch", "dispatch", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("dispatch.{m} {p}"));
                let e = json!({"id":5,"kind":"flash","channel":"wecom-webhook","status":"manual",
                    "attempts":3,"batch":1,"text":"推送正文","item_ids":[1],
                    "last_error":"发送失败","created_at":"2026-01-01T00:00:00Z"});
                match m.as_str() {
                    "outbox" => Ok(json!({ "entries": [e] })),
                    "retry" | "test_send" => Ok(e),
                    "digests" => Ok(
                        json!({"digests":[{"id":2,"date":"2026-01-01","slot":"08:00",
                        "item_count":4,"important_count":1,"outbox_ids":[5],
                        "created_at":"2026-01-01T00:00:00Z"}]}),
                    ),
                    "preview_digest" => Ok(json!({"markdown":["**预览md**"],"plain":["预览plain"],
                        "item_count":4})),
                    _ => Ok(json!({})),
                }
            }
        });
        let c = calls.clone();
        hub.register_fn("secrets", "secrets", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("secrets.{m} {p}"));
                match m.as_str() {
                    "list" => Ok(json!({"secrets":[{"key":"llm.api_key","configured":true,
                        "updated_at":"2026-01-01T00:00:00Z"}]})),
                    _ => Ok(json!({})),
                }
            }
        });
        hub.register_fn("llm", "llm", |m, _p| async move {
            match m.as_str() {
                "ready" => Ok(json!({ "ready": true })),
                "providers" => Ok(json!({"providers":[{"id":"p1","name":"深度求索",
                    "kind":"openai","base_url":"https://api.example/v1","models":["m"],
                    "has_key":true,"is_default":true,"last_test":null}],"assignments":[]})),
                _ => Ok(
                    json!({"rows":[{"day":"2026-01-01","model":"模型X","purpose":"screen",
                    "calls":3,"input_tokens":111,"output_tokens":222,"failures":1}]}),
                ),
            }
        });
        let c = calls.clone();
        hub.register_fn("wecom-webhook", "channel", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("wecom.{m} {p}"));
                Ok(channel_info("wecom-webhook", "ok"))
            }
        });
        let c = calls.clone();
        hub.register_fn("wechat-ilink", "channel", move |m, p| {
            let c = c.clone();
            async move {
                c.lock().unwrap().push(format!("wechat.{m} {p}"));
                match m.as_str() {
                    "bind" => Ok(json!({"state":"unbound",
                        "qrcode":"https://q.example/x?qrcode=abc123&bot_type=3","detail":"请扫码"})),
                    "bind_status" => Ok(json!({"state":"ok"})),
                    _ => Ok(channel_info("wechat-ilink", "unbound")),
                }
            }
        });
        let base = serve(&hub).await;
        Full {
            base,
            calls,
            client: test_http_client(),
            _hub: hub,
        }
    }

    impl Full {
        async fn text(&self, p: &str) -> (u16, String) {
            let r = self
                .client
                .get(format!("{}{p}", self.base))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
        fn has_call(&self, prefix: &str, needle: &str) -> bool {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with(prefix) && c.contains(needle))
        }
    }

    #[tokio::test]
    async fn stage2_pages_render_key_content() {
        let f = setup_full(true).await;
        let (s, b) = f.text("/rules").await;
        assert_eq!(s, 200);
        assert!(b.contains("规则&lt;正文&gt;") && b.contains("2026-01-02T03:04:05Z"));
        assert!(b.contains("/api/v1/rules"));
        let (s, b) = f.text("/items").await;
        assert_eq!(s, 200);
        assert!(b.contains("标题&lt;甲&gt;") && b.contains("理由甲") && b.contains("影响乙"));
        assert!(b.contains("e.com&#x2f;a") && b.contains("/api/v1/items/1/level"));
        let (s, b) = f.text("/items?level=important&text=甲").await;
        assert_eq!(s, 200);
        assert!(b.contains("标题&lt;甲&gt;"));
        assert!(f.has_call("screening.list", "important"));
        let (s, b) = f.text("/items?text=abc").await;
        assert_eq!(s, 200);
        assert!(f.has_call("item-query.list", "abc") && b.contains("理由甲"));
        let (s, b) = f.text("/channels").await;
        assert_eq!(s, 200);
        assert!(b.contains("wecom-webhook") && b.contains("wechat-ilink"));
        assert!(b.contains("/api/v1/channels/wechat-ilink/bind"));
        assert!(!b.contains("/api/v1/channels/wecom-webhook/bind"));
        assert!(b.contains("/api/v1/channels/wecom-webhook/test"));
        let (s, b) = f.text("/outbox").await;
        assert_eq!(s, 200);
        assert!(b.contains("推送正文") && b.contains("/api/v1/outbox/5/retry"));
        assert!(b.contains("every 30s"));
        let (s, b) = f.text("/outbox?status=sent").await;
        assert_eq!(s, 200);
        assert!(f.has_call("dispatch.outbox", "sent") && b.contains("status=sent"));
        let (s, b) = f.text("/digests").await;
        assert_eq!(s, 200);
        assert!(b.contains("08:00") && b.contains("/digests/preview"));
        let (s, b) = f.text("/digests/preview").await;
        assert_eq!(s, 200);
        assert!(b.contains("预览md") && b.contains("预览plain") && b.contains("<pre"));
        let (s, b) = f.text("/secrets").await;
        assert_eq!(s, 200);
        assert!(b.contains("llm.api_key") && b.contains("wecom-webhook.webhook_key"));
        assert!(b.contains("type=\"password\""));
        let (s, b) = f.text("/usage").await;
        assert_eq!(s, 200);
        assert!(b.contains("模型X") && b.contains("222"));
        assert!(!b.contains("尚未就绪"));
        let (_, home) = f.text("/").await;
        // llm 就绪且 wecom 渠道正常，不提示首次配置
        assert!(!home.contains("id=\"first-run\""));
        let (s, b) = f.text("/settings").await;
        assert_eq!(s, 200);
        assert!(b.contains("data-settings-url=\"/settings/push\""));
    }

    #[tokio::test]
    async fn stage2_json_endpoints() {
        let f = setup_full(true).await;
        for (p, k) in [
            ("/api/v1/rules", "text"),
            ("/api/v1/items", "items"),
            ("/api/v1/channels", "channels"),
            ("/api/v1/outbox", "entries"),
            ("/api/v1/digests", "digests"),
            ("/api/v1/secrets", "secrets"),
            ("/api/v1/usage", "rows"),
        ] {
            let r = f.client.get(format!("{}{p}", f.base)).send().await.unwrap();
            assert_eq!(r.status(), 200, "{p}");
            let v: Value = r.json().await.unwrap();
            assert!(v.get(k).is_some(), "{p} 缺 {k}");
        }
        let v: Value = f
            .client
            .get(format!("{}/api/v1/secrets", f.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let first = &v["secrets"][0];
        assert_eq!(first["key"], "llm.api_key");
        assert_eq!(first["configured"], true);
        assert_eq!(v["secrets"][1]["configured"], false);
        assert_eq!(first.as_object().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn channel_test_without_dispatch_explains_recovery_in_fragment() {
        let f = setup(true, false).await;
        for channel in ["wechat-ilink", "wecom-webhook"] {
            let r = f
                .client
                .post(format!("{}/api/v1/channels/{channel}/test", f.base))
                .send()
                .await
                .unwrap();
            // htmx swaps the actionable error into the clicked channel's result slot.
            assert_eq!(r.status(), 200);
            let body = r.text().await.unwrap();
            assert!(body.contains("测试消息未入队"));
            assert!(body.contains("推送调度开关"));
            assert!(!body.contains("服务 dispatch 不可用"));
        }
    }

    #[tokio::test]
    async fn stage2_writes_reach_services() {
        let f = setup_full(true).await;
        let post = |p: &str| f.client.post(format!("{}{p}", f.base));
        let r = post("/api/v1/rules")
            .form(&[("text", "新规则")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(f.has_call("rulebook.set", "新规则"));
        let r = post("/api/v1/rules")
            .json(&json!({"text":"j规则"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert!(v.get("text").is_some());
        assert!(f.has_call("rulebook.set", "j规则"));

        let r = post("/api/v1/items/1/level")
            .form(&[("level", "urgent")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let b = r.text().await.unwrap();
        assert!(b.contains("id=\"item-1\""));
        assert!(f.has_call("screening.review", "\"item_id\":1"));
        assert!(f.has_call("screening.review", "urgent"));
        let r = post("/api/v1/items/1/level")
            .form(&[("level", "不存在")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        let r = post("/api/v1/outbox/5/retry").send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(f.has_call("dispatch.retry", "\"id\":5"));

        let r = post("/api/v1/channels/wecom-webhook/test")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(f.has_call("dispatch.test_send", "wecom-webhook"));

        let r = post("/api/v1/channels/wechat-ilink/bind")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let b = r.text().await.unwrap();
        assert!(f.has_call("wechat.bind", ""));
        assert!(b.contains("<svg") && b.contains("every 3s"));
        assert!(b.contains("qrcode=abc123&amp;bot_type=3"));
        assert!(b.contains("复制链接"));
        let r = post("/api/v1/channels/nope/bind").send().await.unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn bind_status_stops_polling_when_ok() {
        let f = setup_full(true).await;
        let (s, b) = f.text("/channels/wechat-ilink/bind_status").await;
        assert_eq!(s, 200);
        assert!(b.contains("已绑定"));
        assert!(!b.contains("hx-trigger") && !b.contains("every 3s"));
    }

    #[tokio::test]
    async fn secrets_never_echo_value() {
        let f = setup_full(true).await;
        let r = f
            .client
            .post(format!("{}/api/v1/secrets", f.base))
            .form(&[("key", "llm.api_key"), ("value", FAKE_SECRET)])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(!r.text().await.unwrap().contains(FAKE_SECRET));
        assert!(f.has_call("secrets.set", FAKE_SECRET));
        let r = f
            .client
            .post(format!("{}/api/v1/secrets", f.base))
            .json(&json!({"key":"llm.api_key","value":FAKE_SECRET}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(!r.text().await.unwrap().contains(FAKE_SECRET));
        for p in ["/secrets", "/api/v1/secrets", "/usage", "/channels", "/"] {
            assert!(!f.text(p).await.1.contains(FAKE_SECRET), "{p}");
        }
        let r = f
            .client
            .post(format!("{}/api/v1/secrets", f.base))
            .form(&[("key", "llm.api_key"), ("value", "")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let r = f
            .client
            .post(format!("{}/api/v1/secrets/delete", f.base))
            .form(&[("key", "llm.api_key")])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(f.has_call("secrets.delete", "llm.api_key"));
    }

    #[tokio::test]
    async fn missing_optional_services_show_note() {
        let f = setup(true, true).await;
        for p in [
            "/rules",
            "/items",
            "/channels",
            "/outbox",
            "/digests",
            "/secrets",
            "/usage",
        ] {
            let r = f.client.get(format!("{}{p}", f.base)).send().await.unwrap();
            assert_eq!(r.status(), 200, "{p}");
            assert!(r.text().await.unwrap().contains("未启用"), "{p}");
        }
        let r = f.client.get(format!("{}/", f.base)).send().await.unwrap();
        assert!(r.text().await.unwrap().contains("id=\"first-run\""));
    }

    #[tokio::test]
    async fn stage2_writes_denied_without_permission() {
        let f = setup_full(false).await;
        let posts = [
            "/api/v1/rules",
            "/api/v1/items/1/level",
            "/api/v1/outbox/5/retry",
            "/api/v1/channels/wecom-webhook/test",
            "/api/v1/channels/wechat-ilink/bind",
            "/api/v1/secrets",
            "/api/v1/secrets/delete",
        ];
        for p in posts {
            let r = f
                .client
                .post(format!("{}{p}", f.base))
                .form(&[
                    ("text", "x"),
                    ("level", "urgent"),
                    ("key", "k"),
                    ("value", "v"),
                ])
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{p}");
        }
        for p in [
            "/rules",
            "/items",
            "/channels",
            "/outbox",
            "/digests",
            "/secrets",
            "/usage",
        ] {
            assert_eq!(f.text(p).await.0, 403, "{p}");
        }
        assert!(f.calls.lock().unwrap().is_empty());
    }
}

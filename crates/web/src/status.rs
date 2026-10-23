//! 状态协调层：状态仍由各插件维护，这里只聚合快照，不保存业务状态或历史。
use std::time::Instant;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use plugin_sdk::contracts::{
    channel::ChannelClient, dispatch::DispatchClient, hub_admin::HubAdminClient, llm::LlmClient,
    now_rfc3339, source::SourceStatusClient,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{check, AppState, Shared};

#[derive(Default)]
pub(crate) struct Coordinator(pub Mutex<Option<(Instant, Value)>>);

async fn channels(st: &AppState) -> Value {
    if check(st, "channels.read").await.is_some() {
        return json!({"ok":false,"error":"无渠道查看权限"});
    }
    let mut values = Vec::new();
    let mut services = st.ctx.services("channel");
    services.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, svc) in services {
        match tokio::time::timeout(
            std::time::Duration::from_millis(750),
            ChannelClient(svc).info(),
        )
        .await
        {
            Ok(Ok(info)) => {
                let mut value = serde_json::to_value(&info).unwrap_or_default();
                value["state_label"] = json!(crate::pages::channel_state_label(&id, info.state));
                values.push(value);
            }
            _ => values
                .push(json!({"id":id,"state":"unknown","state_label":"同步异常","error":true})),
        }
    }
    json!({"ok":true,"data":values})
}

async fn bounded(future: impl std::future::Future<Output = Value>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .unwrap_or_else(|_| json!({"ok":false,"error":"状态读取超时"}))
}

fn reply(mut value: Value, can_queue: bool) -> Response {
    if !can_queue {
        value["queue"] = json!({"ok":false,"error":"无队列查看权限"});
    }
    Json(value).into_response()
}

pub(crate) async fn get(State(st): Shared) -> Response {
    if let Some(response) = check(&st, "web.view").await {
        return response;
    }
    if let Some(response) = check(&st, "channels.read").await {
        return response;
    }
    let can_queue = check(&st, "outbox.read").await.is_none();
    let mut cache = st.status.0.lock().await;
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed().as_millis() < 1000 {
            return reply(value.clone(), can_queue);
        }
    }
    let (channels, sources, plugins, model, queue) = tokio::join!(
        bounded(channels(&st)),
        bounded(async {
            match SourceStatusClient(st.ctx.service("source-status"))
                .list()
                .await
            {
                Ok(data) => json!({"ok":true,"data":data}),
                Err(_) => json!({"ok":false,"error":"信源状态暂不可用"}),
            }
        }),
        bounded(async {
            match HubAdminClient(st.ctx.service("hub-admin")).plugins().await {
                Ok(data) => {
                    json!({"ok":true,"data":data.into_iter().map(|p|json!({"name":p.name,"status":p.status,"enabled":p.enabled,"last_error":p.last_error})).collect::<Vec<_>>()})
                }
                Err(_) => json!({"ok":false,"error":"插件状态暂不可用"}),
            }
        }),
        bounded(async {
            match LlmClient(st.ctx.service("llm")).ready().await {
                Ok(ready) => json!({"ok":true,"data":{"ready":ready}}),
                Err(_) => json!({"ok":false,"error":"模型状态暂不可用"}),
            }
        }),
        bounded(async {
            match DispatchClient(st.ctx.service("dispatch"))
                .queue_summary()
                .await
            {
                Ok(data) => json!({"ok":true,"data":data}),
                Err(_) => json!({"ok":false,"error":"推送调度未运行或队列状态暂不可用"}),
            }
        })
    );
    let mcp = bounded(async {
        match crate::mcp::pending(&st).await {
            Ok(data) => json!({"ok":true,"data":data}),
            Err(_) => json!({"ok":false,"error":"应用操作服务暂不可用"}),
        }
    })
    .await;
    let updates = HubAdminClient(st.ctx.service("hub-admin"))
        .app_get(plugin_sdk::contracts::hub_admin::app_keys::UPDATE_STATUS)
        .await
        .unwrap_or(Value::Null);
    let value = json!({"updated_at":now_rfc3339(),"channels":channels,"sources":sources,"plugins":plugins,"model":model,"queue":queue,"mcp":mcp,"updates":{"ok":true,"data":updates}});
    *cache = Some((Instant::now(), value.clone()));
    reply(value, can_queue)
}

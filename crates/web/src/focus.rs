use crate::{check, err_response, Shared};
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use plugin_sdk::contracts::{call_typed, focus::PushSettings};
pub(crate) async fn get(State(st): Shared) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    match call_typed::<_, PushSettings>(
        &*st.ctx.service("dispatch"),
        "focus_settings",
        &serde_json::json!({}),
    )
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}
pub(crate) async fn save(State(st): Shared, Json(p): Json<PushSettings>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    if let Err(e) = p.validate() {
        return err_response(e);
    }
    match call_typed::<_, PushSettings>(&*st.ctx.service("dispatch"), "save_focus_settings", &p)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

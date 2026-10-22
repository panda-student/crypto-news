//! 追问：会话、流式问答、重命名、删除与撤销。回答来自 agent 服务（ask_stream），
//! 会话存在 web 自己的表里。
//!
//! 流式输出：后台任务调用 `ask_stream`，llm 的 `llm.delta` 事件按 stream_id 累积到内存里；
//! 浏览器用 EventSource 连 `GET /api/v1/chat/stream/{id}`。axum 没开 sse 特性，也没有流式
//! 响应体，所以每次响应是一小段有限的 `text/event-stream`（带 `retry: 50` 和 `id`），
//! 有新内容或结束就立刻返回，浏览器按 retry 自动重连并带上 `Last-Event-ID`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plugin_sdk::contracts::agent::{AgentClient, AskRequest, AskStreamRequest, ChatTurn};
use plugin_sdk::contracts::llm::{LlmCancel, LlmDelta, LLM_CANCEL, LLM_DELTA};
use plugin_sdk::contracts::screen::ScreeningClient;
use plugin_sdk::contracts::source::ItemQueryClient;
use plugin_sdk::contracts::{parse_rfc3339, ServiceError};
use plugin_sdk::Context;
use serde::Deserialize;
use serde_json::{json, Value};
use time::{Duration as TDuration, OffsetDateTime, UtcOffset};
use tokio::sync::Notify;

use crate::data::{Db, Session, TurnRow};
use crate::feed::{db, tz_offset};
use crate::pages::Body;
use crate::{check, err_response, AppState, Shared};

const QUESTION_MAX: usize = 500;
const TITLE_MAX: usize = 24;
const HISTORY: usize = 5;
const POLL_WAIT: Duration = Duration::from_secs(25);
const KEEP_AFTER_DONE: Duration = Duration::from_secs(120);
const TRASH_MAX: usize = 20;
const UNAVAILABLE_HINT: &str = "追问功能当前不可用（agent 服务未启用或模型未就绪）。";

fn clip(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

// ---------------------------------------------------------------- 流状态

#[derive(Default)]
struct StreamInner {
    text: String,
    done: Option<Value>,
    cancel_requested: bool,
}

struct StreamState {
    inner: Mutex<StreamInner>,
    notify: Notify,
    session_id: i64,
}

/// 撤销用的回收站条目：令牌、会话、全部回合。
type TrashEntry = (String, Session, Vec<TurnRow>);

/// 内存里的流注册表与删除回收站。
#[derive(Clone, Default)]
pub(crate) struct ChatState {
    streams: Arc<Mutex<HashMap<String, Arc<StreamState>>>>,
    trash: Arc<Mutex<Vec<TrashEntry>>>,
    counter: Arc<AtomicU64>,
    sync: Arc<tokio::sync::Mutex<()>>,
    pub(crate) operations: Arc<tokio::sync::Mutex<()>>,
}

impl ChatState {
    fn get(&self, id: &str) -> Option<Arc<StreamState>> {
        self.streams.lock().ok()?.get(id).cloned()
    }

    pub(crate) fn new_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let t = OffsetDateTime::now_utc().unix_timestamp_nanos();
        format!("s{t:x}-{n:x}")
    }

    fn live_for_session(&self, sid: i64) -> Vec<String> {
        let Ok(m) = self.streams.lock() else {
            return Vec::new();
        };
        m.iter()
            .filter(|(_, s)| {
                s.session_id == sid && s.inner.lock().map(|i| i.done.is_none()).unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// 订阅 `llm.delta`，把片段累积到对应的流。
    pub(crate) fn subscribe(&self, ctx: &Context) {
        let me = self.clone();
        let r = ctx.subscribe(LLM_DELTA, move |e| {
            let me = me.clone();
            async move {
                if let Ok(d) = serde_json::from_value::<LlmDelta>(e.payload) {
                    if let Some(s) = me.get(&d.stream_id) {
                        if let Ok(mut i) = s.inner.lock() {
                            if i.done.is_none() {
                                i.text.push_str(&d.text);
                            }
                        }
                        s.notify.notify_waiters();
                    }
                }
                Ok(())
            }
        });
        if let Err(e) = r {
            tracing::warn!("订阅 {LLM_DELTA} 失败，追问将不能流式显示：{e}");
        }
    }
}

// ---------------------------------------------------------------- 回答渲染

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// 正文转 HTML：转义、换行、`[n]` 变成可点的上标。
fn body_html(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() && j - i <= 4 {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && chars[j] == ']' {
                let n: String = chars[i + 1..j].iter().collect();
                o.push_str(&format!("<sup class=\"cite\" data-n=\"{n}\">[{n}]</sup>"));
                i = j + 1;
                continue;
            }
        }
        match c {
            '\n' => o.push_str("<br>"),
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
        i += 1;
    }
    o
}

const ARROW_LEFT: &str = "<svg viewBox=\"0 0 24 24\" width=\"14\" height=\"14\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M5 12h14M5 12l6 6M5 12l6-6\"/></svg>";

/// 完整回答的 HTML（正文 + 引用脚注 + 停止/错误标记）。
fn reply_html(r: &Value) -> String {
    let mut h = String::new();
    if let Some(operation) = r.get("operation") {
        return crate::mcp::operation_html(operation);
    }
    if let Some(e) = r.get("error").and_then(Value::as_str) {
        let partial = r.get("answer").and_then(Value::as_str).unwrap_or("");
        if !partial.is_empty() {
            h.push_str(&body_html(partial));
        }
        h.push_str(&format!("<div class=\"cerr\">{}</div>", esc(e)));
        return h;
    }
    h.push_str(&body_html(&answer_text(r)));
    if let Some(cs) = r.get("citations").and_then(Value::as_array) {
        let mut foot = String::new();
        for (i, c) in cs.iter().enumerate() {
            let n = c.get("n").and_then(Value::as_u64).unwrap_or(i as u64 + 1);
            let id = c.get("item_id").and_then(Value::as_i64).unwrap_or(0);
            let t = c.get("title").and_then(Value::as_str).unwrap_or("");
            foot.push_str(&format!(
                "<div class=\"cl\">[{n}] <span class=\"lk\" data-cite=\"{id}\">{}</span> <span class=\"ic\" data-loc=\"{id}\" title=\"在左边定位\">{ARROW_LEFT}</span></div>",
                esc(t)
            ));
        }
        if !foot.is_empty() {
            h.push_str(&format!("<div class=\"cites\">{foot}</div>"));
        }
    }
    if r.get("stopped").and_then(Value::as_bool) == Some(true) {
        h.push_str("<div class=\"cstop\">已停止</div>");
    }
    h
}

/// 存档的回答正文；兼容旧版（conclusion / bullish / bearish / unconfirmed）。
fn answer_text(r: &Value) -> String {
    if let Some(a) = r.get("answer").and_then(Value::as_str) {
        return a.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(c) = r.get("conclusion").and_then(Value::as_str) {
        if !c.is_empty() {
            parts.push(c.to_string());
        }
    }
    for (k, label) in [
        ("bullish", "利好"),
        ("bearish", "利空"),
        ("unconfirmed", "未证实"),
    ] {
        if let Some(a) = r.get(k).and_then(Value::as_array) {
            for x in a.iter().filter_map(Value::as_str) {
                parts.push(format!("{label}：{x}"));
            }
        }
    }
    parts.join("\n")
}

fn stored_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

fn parse_reply(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------- 会话列表

fn fmt_time(updated: &str, tz: i32) -> (String, &'static str) {
    let off = UtcOffset::from_whole_seconds(tz * 60).unwrap_or(UtcOffset::UTC);
    let today = OffsetDateTime::now_utc().to_offset(off).date();
    let yesterday = today - TDuration::days(1);
    let Some(t) = parse_rfc3339(updated).map(|t| t.to_offset(off)) else {
        return (String::new(), "更早");
    };
    let hm = format!("{:02}:{:02}", t.hour(), t.minute());
    if t.date() == today {
        (format!("今天 {hm}"), "今天")
    } else if t.date() == yesterday {
        (format!("昨天 {hm}"), "昨天")
    } else {
        (format!("{:02}-{:02}", t.month() as u8, t.day()), "更早")
    }
}

pub(crate) async fn sync_shared(st: &AppState) -> Result<(), ServiceError> {
    let _guard = st.chat.sync.lock().await;
    let d = db(st);
    let agent = AgentClient(st.ctx.service("agent"));
    for t in d.pending_shared().await? {
        let mut r = parse_reply(&t.reply_json);
        let record = plugin_sdk::contracts::agent::RecordConversationTurn {
            turn_key: t.remote_key.clone().unwrap_or_default(),
            conversation_key: r["shared_conversation"].as_str().unwrap_or_default().into(),
            channel: r["shared_channel"].as_str().unwrap_or_default().into(),
            origin: "desktop".into(),
            question: t.question,
            reply_json: t.reply_json,
        };
        agent.record_conversation_turn(&record).await?;
        r["sync_pending"] = json!(false);
        d.update_turn(t.id, &stored_json(&r)).await?;
    }
    let mut after = d
        .pref("shared_conversation_cursor")
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    for _ in 0..10 {
        let turns = agent.conversation_turns(after).await?;
        let full = turns.len() == 100;
        for t in turns {
            d.import_conversation_turn(&t).await?;
            after = t.id;
            d.set_pref("shared_conversation_cursor", &after.to_string())
                .await?;
        }
        if !full {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn api_sessions(State(st): Shared, headers: HeaderMap) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let sync_error = sync_shared(&st)
        .await
        .err()
        .map(|_| "渠道会话暂未同步，将自动重试");
    let tz = tz_offset(&headers);
    match db(&st).sessions().await {
        Ok(list) => {
            let v: Vec<Value> = list
                .into_iter()
                .map(|s| {
                    let (time, group) = fmt_time(&s.updated_at, tz);
                    json!({ "id": s.id, "title": s.title, "time": time, "group": group, "updated_at":s.updated_at, "shared":s.remote_key.is_some() })
                })
                .collect();
            Json(json!({ "sessions": v, "sync_error":sync_error })).into_response()
        }
        Err(e) => err_response(e),
    }
}

async fn item_head(st: &AppState, id: Option<i64>) -> Option<Value> {
    let id = id?;
    let items = ItemQueryClient(st.ctx.service("item-query"))
        .get(&[id])
        .await
        .ok()?;
    let i = items.into_iter().next()?;
    let screening = ScreeningClient(st.ctx.service("screening"))
        .get(&[id])
        .await
        .ok()
        .and_then(|v| v.into_iter().next());
    let title = screening
        .as_ref()
        .and_then(|s| s.title_zh.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(i.title.clone());
    Some(
        json!({ "id": i.id, "title": title, "t": title, "summary": i.summary,
        "impact": screening.as_ref().and_then(|s| s.impact.clone()), "assets": screening.as_ref().map(|s| s.assets.clone()).unwrap_or_default(),
        "at": i.published_at.unwrap_or(i.collected_at), "sn": i.source_id, "lv": screening.as_ref().and_then(|s|s.level).map(|l| match l {plugin_sdk::contracts::screen::Level::Urgent=>"u",plugin_sdk::contracts::screen::Level::Important=>"i",plugin_sdk::contracts::screen::Level::Archive=>"x",_=>"n"}).unwrap_or("n"),
        "dc": screening.as_ref().and_then(|s|s.direction).map(|d|match d {plugin_sdk::contracts::screen::Direction::Up=>"up",plugin_sdk::contracts::screen::Direction::Down=>"dn",_=>"nu"}).unwrap_or(""), "url": i.url }),
    )
}

pub(crate) async fn api_session(
    State(st): Shared,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let d = db(&st);
    let s = match d.session(id).await {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, "会话不存在").into_response(),
        Err(e) => return err_response(e),
    };
    if s.remote_key.is_some() {
        let _ = sync_shared(&st).await;
    }
    let rows = match d.turns(id).await {
        Ok(r) => r,
        Err(e) => return err_response(e),
    };
    let mut turns = Vec::new();
    for t in rows {
        let r = parse_reply(&t.reply_json);
        if r.get("pending").and_then(Value::as_bool) == Some(true) {
            let sid = r.get("stream_id").and_then(Value::as_str).unwrap_or("");
            if !sid.is_empty() && st.chat.get(sid).is_some() {
                turns.push(json!({ "id": t.id, "question": t.question, "html": "",
                    "pending": true, "stream_id": sid, "item_ids": r.get("item_ids") }));
            } else {
                turns.push(json!({ "id": t.id, "question": t.question,
                    "html": "<div class=\"cerr\">（回答已中断）</div>", "pending": false }));
            }
            continue;
        }
        turns.push(json!({ "id": t.id, "question": t.question,
            "html": reply_html(&r), "pending": false, "item_ids": r.get("item_ids"), "origin":r.get("origin") }));
    }
    let mut context: Vec<Value> = d
        .pref(&format!("chat_context_{}", s.id))
        .await
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_else(|| {
            s.item_id
                .map(|id| vec![json!({"id":id,"after":0})])
                .unwrap_or_default()
        });
    for entry in &mut context {
        entry["item"] = item_head(&st, entry["id"].as_i64())
            .await
            .unwrap_or(Value::Null);
    }
    let (time, _) = fmt_time(&s.updated_at, tz_offset(&headers));
    Json(json!({
        "id": s.id, "title": s.title, "time": time,
        "item": item_head(&st, s.item_id).await,
        "turns": turns,
        "context": context,
        "shared":s.remote_key.is_some(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct ContextForm {
    session_id: Option<i64>,
    items: Vec<Value>,
}
pub(crate) async fn api_context(
    State(st): Shared,
    Body { v: f, .. }: Body<ContextForm>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    if f.items.len() > 20
        || f.items.iter().any(|v| {
            v.get("id").and_then(Value::as_i64).is_none_or(|id| id <= 0)
                || v.get("after").and_then(Value::as_u64).is_none()
        })
    {
        return (StatusCode::BAD_REQUEST, "资料格式不正确或超过 20 条").into_response();
    }
    let ids: Vec<i64> = f.items.iter().filter_map(|v| v["id"].as_i64()).collect();
    if ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
        return (StatusCode::BAD_REQUEST, "不能重复加入同一条消息").into_response();
    }
    let source_items = match ItemQueryClient(st.ctx.service("item-query"))
        .get(&ids)
        .await
    {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    if ids
        .iter()
        .any(|id| !source_items.iter().any(|it| it.id == *id))
    {
        return (StatusCode::BAD_REQUEST, "消息已不存在").into_response();
    }
    let title = source_items
        .first()
        .map(|it| clip(&it.title, TITLE_MAX))
        .unwrap_or_else(|| "消息分析".into());
    let d = db(&st);
    let id = if let Some(id) = f.session_id {
        match d.session(id).await {
            Ok(Some(_)) => id,
            Ok(None) => return (StatusCode::NOT_FOUND, "会话不存在").into_response(),
            Err(e) => return err_response(e),
        }
    } else {
        match d.create_session(&title, None).await {
            Ok(id) => id,
            Err(e) => return err_response(e),
        }
    };
    let items: Vec<Value> = f
        .items
        .iter()
        .map(|v| json!({"id": v["id"], "after": v["after"]}))
        .collect();
    if let Err(e) = d
        .set_pref(&format!("chat_context_{id}"), &stored_json(&json!(items)))
        .await
    {
        return err_response(e);
    }
    Json(json!({"session_id": id, "title":title})).into_response()
}

// ---------------------------------------------------------------- 提问

#[derive(Deserialize)]
pub(crate) struct AskForm {
    question: String,
    session_id: Option<String>,
    item_id: Option<String>,
    item_ids: Option<Vec<i64>>,
}

fn opt_id(s: &Option<String>) -> Option<i64> {
    s.as_deref()
        .and_then(|s| s.trim().parse().ok())
        .filter(|i| *i > 0)
}

/// 历史里只带完成的轮次的回答正文。
fn history_of(rows: Vec<TurnRow>) -> Vec<ChatTurn> {
    let done: Vec<ChatTurn> = rows
        .into_iter()
        .filter_map(|t| {
            let r = parse_reply(&t.reply_json);
            if r.get("pending").is_some() || r.get("error").is_some() {
                return None;
            }
            let a = answer_text(&r);
            (!a.is_empty()).then_some(ChatTurn {
                question: t.question,
                answer: a,
            })
        })
        .collect();
    let skip = done.len().saturating_sub(HISTORY);
    done.into_iter().skip(skip).collect()
}

pub(crate) async fn api_chat(State(st): Shared, body: Body<AskForm>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let f = body.v;
    let question = clip(f.question.trim(), QUESTION_MAX);
    if question.is_empty() {
        return err_response(ServiceError::invalid_params("问题不能为空"));
    }
    if crate::mcp::contains_secret(&question) {
        return err_response(ServiceError::invalid_params(
            "请不要在对话中发送密钥。先描述配置需求，再在确认卡片的安全输入框中填写密钥。",
        ));
    }
    let d = db(&st);
    let sess = match opt_id(&f.session_id) {
        Some(id) => match d.session(id).await {
            Ok(Some(s)) => Some(s),
            Ok(None) => return (StatusCode::NOT_FOUND, "会话不存在").into_response(),
            Err(e) => return err_response(e),
        },
        None => None,
    };
    let shared_key = sess.as_ref().and_then(|s| s.remote_key.clone());
    if shared_key.is_some() && sync_shared(&st).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "会话暂未同步，请稍后重试，避免遗漏微信上下文",
        )
            .into_response();
    }
    let item_id = match &sess {
        Some(s) => s.item_id,
        None => opt_id(&f.item_id),
    };
    let mut item_ids = f.item_ids;
    if item_ids.is_none() {
        if let Some(s) = &sess {
            if let Ok(Some(raw)) = d.pref(&format!("chat_context_{}", s.id)).await {
                if let Ok(entries) = serde_json::from_str::<Vec<Value>>(&raw) {
                    item_ids = Some(entries.iter().filter_map(|v| v["id"].as_i64()).collect());
                }
            }
        }
    }
    if item_ids
        .as_ref()
        .is_some_and(|ids| ids.len() > 20 || ids.iter().any(|id| *id <= 0))
    {
        return (StatusCode::BAD_REQUEST, "最多选择 20 条消息").into_response();
    }
    let history = match &sess {
        Some(s) => match d.turns(s.id).await {
            Ok(rows) => history_of(rows),
            Err(e) => return err_response(e),
        },
        None => Vec::new(),
    };
    let new_session = sess.is_none();
    let (sid, title) = match sess {
        Some(s) => (s.id, s.title),
        None => {
            let title = clip(&question, TITLE_MAX);
            match d.create_session(&title, item_id).await {
                Ok(id) => (id, title),
                Err(e) => return err_response(e),
            }
        }
    };
    let stream_id = st.chat.new_id();
    let turn_id = match d
        .add_turn(
            sid,
            &question,
            &stored_json(&json!({ "pending": true, "stream_id": stream_id, "item_ids": item_ids })),
        )
        .await
    {
        Ok(id) => id,
        Err(e) => return err_response(e),
    };
    if shared_key.is_some() {
        if let Err(e) = d
            .mark_remote_turn(turn_id, &format!("desktop:{sid}:{turn_id}"))
            .await
        {
            return err_response(e);
        }
    }
    let shared_state = st.clone();
    let state = Arc::new(StreamState {
        inner: Mutex::new(StreamInner::default()),
        notify: Notify::new(),
        session_id: sid,
    });
    if let Ok(mut m) = st.chat.streams.lock() {
        m.insert(stream_id.clone(), state.clone());
    }
    let ctx = st.ctx.clone();
    let chat = st.chat.clone();
    let key = stream_id.clone();
    let req = AskStreamRequest {
        request: AskRequest {
            question,
            item_id,
            item_ids,
            history,
        },
        stream_id: stream_id.clone(),
    };
    st.ctx.spawn("chat-stream", async move {
        let operation = crate::mcp::ask(&shared_state, sid, &req.request).await;
        let result = if operation.is_none() {
            Some(AgentClient(ctx.service("agent")).ask_stream(&req).await)
        } else {
            None
        };
        let (partial, cancel_requested) = state
            .inner
            .lock()
            .map(|i| (i.text.clone(), i.cancel_requested))
            .unwrap_or_default();
        let mut stored = match result {
            None => operation.unwrap_or_default(),
            Some(Ok(r)) => json!({
                "answer": r.answer, "citations": r.citations, "found": r.found,
                "stopped": r.cancelled || cancel_requested,
            }),
            Some(Err(e)) => {
                if cancel_requested && !partial.is_empty() {
                    json!({ "answer": partial, "citations": [], "found": true, "stopped": true })
                } else {
                    let msg = if e.code == ServiceError::UNAVAILABLE {
                        UNAVAILABLE_HINT.to_string()
                    } else {
                        format!("追问失败：{}", e.message)
                    };
                    json!({ "answer": partial, "error": msg })
                }
            }
        };
        stored["item_ids"] = json!(req.request.item_ids);
        stored["origin"] = json!("desktop");
        if let Some(key) = shared_key {
            let parts: Vec<String> = serde_json::from_str(&key).unwrap_or_default();
            stored["shared_conversation"] = json!(key);
            stored["shared_channel"] = json!(parts.first().cloned().unwrap_or_default());
            stored["sync_pending"] = json!(true);
        }
        let done = json!({
            "html": reply_html(&stored),
            "stopped": stored.get("stopped").and_then(Value::as_bool).unwrap_or(false),
            "error": stored.get("error"),
            "answer": answer_text(&stored),
        });
        let _ = Db(ctx.store())
            .update_turn(turn_id, &stored_json(&stored))
            .await;
        if stored["sync_pending"] == true {
            if let Err(e) = sync_shared(&shared_state).await {
                tracing::warn!("会话同步待重试：{}", e.message);
            }
        }
        if let Ok(mut i) = state.inner.lock() {
            i.done = Some(done);
        }
        state.notify.notify_waiters();
        tokio::time::sleep(KEEP_AFTER_DONE).await;
        if let Ok(mut m) = chat.streams.lock() {
            m.remove(&key);
        }
        Ok(())
    });
    Json(json!({
        "session_id": sid, "turn_id": turn_id, "stream_id": stream_id,
        "title": title, "new_session": new_session,
    }))
    .into_response()
}

// ---------------------------------------------------------------- 流

#[derive(Deserialize, Default)]
pub(crate) struct StreamParams {
    from: Option<usize>,
}

fn sse(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn api_stream(
    State(st): Shared,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(p): Query<StreamParams>,
) -> Response {
    if let Some(r) = check(&st, "web.view").await {
        return r;
    }
    let Some(s) = st.chat.get(&id) else {
        return (StatusCode::NOT_FOUND, "流不存在或已结束").into_response();
    };
    let from = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .or(p.from)
        .unwrap_or(0);
    let deadline = tokio::time::Instant::now() + POLL_WAIT;
    loop {
        let notified = s.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let Ok(i) = s.inner.lock() else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            let len = i.text.len();
            if len > from && i.text.is_char_boundary(from) {
                let chunk = &i.text[from..];
                return sse(format!(
                    "retry: 50\nid: {len}\nevent: delta\ndata: {}\n\n",
                    json!(chunk)
                ));
            }
            if let Some(d) = &i.done {
                return sse(format!("retry: 50\nevent: done\ndata: {d}\n\n"));
            }
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return sse("retry: 50\n: keepalive\n\n".to_string());
        }
    }
}

pub(crate) async fn api_cancel(State(st): Shared, Path(id): Path<String>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    if let Some(s) = st.chat.get(&id) {
        if let Ok(mut i) = s.inner.lock() {
            i.cancel_requested = true;
        }
    }
    match st
        .ctx
        .publish(LLM_CANCEL, &LlmCancel { stream_id: id })
        .await
    {
        Ok(_) => Json(json!({ "cancelled": true })).into_response(),
        Err(e) => err_response(ServiceError::internal(e.to_string())),
    }
}

// ---------------------------------------------------------------- 重命名、删除、撤销

#[derive(Deserialize)]
pub(crate) struct RenameForm {
    title: String,
}

pub(crate) async fn api_rename(
    State(st): Shared,
    Path(id): Path<i64>,
    body: Body<RenameForm>,
) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let title = clip(body.v.title.trim(), 60);
    if title.is_empty() {
        return err_response(ServiceError::invalid_params("标题不能为空"));
    }
    match db(&st).rename_session(id, &title).await {
        Ok(false) => (StatusCode::NOT_FOUND, "会话不存在").into_response(),
        Ok(true) => Json(json!({ "session_id": id, "title": title })).into_response(),
        Err(e) => err_response(e),
    }
}

pub(crate) async fn api_delete(State(st): Shared, Path(id): Path<i64>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let d = db(&st);
    let s = match d.session(id).await {
        Ok(Some(s)) => s,
        Ok(None) => return (StatusCode::NOT_FOUND, "会话不存在").into_response(),
        Err(e) => return err_response(e),
    };
    let turns = match d.turns(id).await {
        Ok(t) => t,
        Err(e) => return err_response(e),
    };
    for sid in st.chat.live_for_session(id) {
        if let Some(x) = st.chat.get(&sid) {
            if let Ok(mut i) = x.inner.lock() {
                i.cancel_requested = true;
            }
        }
        let _ = st
            .ctx
            .publish(LLM_CANCEL, &LlmCancel { stream_id: sid })
            .await;
    }
    if let Err(e) = d.delete_session(id).await {
        return err_response(e);
    }
    let token = st.chat.new_id();
    if let Ok(mut t) = st.chat.trash.lock() {
        t.push((token.clone(), s, turns));
        if t.len() > TRASH_MAX {
            t.remove(0);
        }
    }
    Json(json!({ "session_id": id, "deleted": true, "undo": token })).into_response()
}

pub(crate) async fn api_undo(State(st): Shared, Path(token): Path<String>) -> Response {
    if let Some(r) = check(&st, "web.manage").await {
        return r;
    }
    let entry = st.chat.trash.lock().ok().and_then(|mut t| {
        let pos = t.iter().position(|(k, _, _)| *k == token)?;
        Some(t.remove(pos))
    });
    let Some((_, s, turns)) = entry else {
        return (StatusCode::NOT_FOUND, "无法撤销：记录已过期").into_response();
    };
    match db(&st).restore_session(&s, &turns).await {
        Ok(()) => Json(json!({ "session_id": s.id })).into_response(),
        Err(e) => err_response(e),
    }
}

use crate::{render, select, Dispatch};
use plugin_sdk::contracts::{
    channel,
    dispatch::OutboxStatus,
    events::PushItem,
    focus::{self, PushSettings},
    format_rfc3339, parse_rfc3339,
    screen::{Level, ScreeningClient},
    store::Statement,
    ServiceError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use time::Duration;
pub const SCHEMA:&str="CREATE TABLE focus_settings(id INTEGER PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE focus_receipts(revision TEXT PRIMARY KEY, event_key TEXT NOT NULL, published_at TEXT NOT NULL); CREATE TABLE focus_pending(event_key TEXT PRIMARY KEY, payload TEXT NOT NULL, due TEXT NOT NULL);";
#[derive(Deserialize)]
struct SettingRow {
    value: String,
}
#[derive(Deserialize)]
struct PendingRow {
    event_key: String,
    payload: String,
}
impl Dispatch {
    pub(crate) async fn focus_settings(&self) -> Result<PushSettings, ServiceError> {
        let rows: Vec<SettingRow> = self
            .store
            .query_as("SELECT value FROM focus_settings WHERE id=1", vec![])
            .await?;
        match rows.first() {
            Some(r) => serde_json::from_str(&r.value).map_err(Into::into),
            None => Ok(PushSettings::default()),
        }
    }
    pub(crate) async fn save_focus_settings(
        &self,
        mut p: PushSettings,
    ) -> Result<PushSettings, ServiceError> {
        p.validate()?;
        if !self.cfg.focus_mode {
            return Err(ServiceError::invalid_params(
                "当前为兼容推送模式，请在高级配置启用 focus_mode",
            ));
        }
        let _guard = self.send_lock.lock().await;
        p.effective_at = self.now_s();
        self.store.batch(vec![Statement::new("INSERT INTO focus_settings(id,value) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET value=excluded.value",vec![json!(serde_json::to_string(&p)?)]),Statement::new("DELETE FROM focus_pending",vec![]),Statement::new("UPDATE outbox SET status='dropped', last_error='推送设置已变更，旧队列不补推' WHERE kind='flash' AND status IN ('pending','manual')",vec![])]).await?;
        Ok(p)
    }
    pub(crate) async fn receive_focus(
        &self,
        items: Vec<PushItem>,
        defer: Option<String>,
    ) -> Result<(), ServiceError> {
        let _guard = self.send_lock.lock().await;
        let p = self.focus_settings().await?;
        if !p.enabled {
            return Ok(());
        }
        let screens = ScreeningClient(self.ctx.service("screening"))
            .get(&items.iter().map(|i| i.item_id).collect::<Vec<_>>())
            .await?;
        for mut item in items {
            let Some(s) = screens.iter().find(|s| s.item_id == item.item_id) else {
                continue;
            };
            if !focus::in_window(
                item.level,
                s.direction,
                item.published_at.as_deref(),
                self.now(),
            ) || !p.accepts(item.level, &s.assets)
                || item
                    .published_at
                    .as_deref()
                    .zip(parse_rfc3339(&p.effective_at))
                    .is_some_and(|(t, e)| parse_rfc3339(t).is_none_or(|t| t < e))
            {
                continue;
            }
            let key = focus::identity(
                &item.title,
                item.published_at.as_deref(),
                s.focus.as_ref(),
                item.item_id,
            );
            let prior = self
                .store
                .query(
                    "SELECT revision,published_at FROM focus_receipts WHERE event_key=?",
                    vec![json!(key)],
                )
                .await?;
            if prior.iter().any(|r| {
                !focus::in_window(
                    item.level,
                    s.direction,
                    r.get("published_at").and_then(Value::as_str),
                    self.now(),
                )
            }) {
                continue;
            }
            let update = s
                .focus
                .as_ref()
                .filter(|f| f.priority < 2 && !f.change.is_empty() && !f.update_key.is_empty())
                .map(|f| f.update_key.as_str());
            if !prior.is_empty() && update.is_none() {
                continue;
            }
            let revision = format!("{}|{}", key, update.unwrap_or("initial"));
            if prior
                .iter()
                .any(|r| r.get("revision").and_then(Value::as_str) == Some(&revision))
            {
                continue;
            }
            let delay = if item.level == Level::Urgent {
                0
            } else {
                p.interval as i64
            };
            let mut due = self.now() + Duration::minutes(delay);
            if delay > 0 {
                let pending = self
                    .store
                    .query("SELECT due,payload FROM focus_pending", vec![])
                    .await?;
                for r in pending {
                    if r.get("payload")
                        .and_then(Value::as_str)
                        .and_then(|s| serde_json::from_str::<PushItem>(s).ok())
                        .is_some_and(|i| i.level == Level::Important)
                    {
                        if let Some(t) =
                            r.get("due").and_then(Value::as_str).and_then(parse_rfc3339)
                        {
                            due = due.min(t)
                        }
                    }
                }
            }
            if !prior.is_empty() {
                if let Some(f) = s.focus.as_ref() {
                    item.title = format!("【事件更新】{}：{}", item.title, f.change);
                }
            }
            if let Some(d) = defer.as_deref().and_then(parse_rfc3339) {
                due = due.max(d)
            }
            self.store.batch(vec![Statement::new("INSERT OR IGNORE INTO focus_receipts(revision,event_key,published_at) VALUES(?,?,?)",vec![json!(revision),json!(key),json!(item.published_at)]),Statement::new("INSERT INTO focus_pending(event_key,payload,due) VALUES(?,?,?) ON CONFLICT(event_key) DO UPDATE SET payload=excluded.payload,due=MIN(focus_pending.due,excluded.due)",vec![json!(key),json!(serde_json::to_string(&item)?),json!(format_rfc3339(due))])]).await?;
        }
        Ok(())
    }
    pub(crate) async fn flush_focus(&self) -> Result<(), ServiceError> {
        let rows: Vec<PendingRow> = self
            .store
            .query_as(
                "SELECT event_key,payload FROM focus_pending WHERE due<=? ORDER BY due",
                vec![json!(self.now_s())],
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let p = self.focus_settings().await?;
        let parsed: Vec<PushItem> = rows
            .iter()
            .filter_map(|r| serde_json::from_str(&r.payload).ok())
            .collect();
        let screens = ScreeningClient(self.ctx.service("screening"))
            .get(&parsed.iter().map(|i| i.item_id).collect::<Vec<_>>())
            .await?;
        let items: Vec<PushItem> = parsed
            .into_iter()
            .filter(|i| {
                screens
                    .iter()
                    .find(|s| s.item_id == i.item_id)
                    .is_some_and(|s| {
                        focus::in_window(
                            i.level,
                            s.direction,
                            i.published_at.as_deref(),
                            self.now(),
                        ) && !matches!(s.level, Some(Level::Normal | Level::Archive))
                            && p.accepts(i.level, &s.assets)
                    })
            })
            .collect();
        let mut stmts = vec![];
        if !items.is_empty() {
            let chans = self.channels().await;
            let infos = chans.iter().map(|c| c.info.clone()).collect::<Vec<_>>();
            let targets = select::flash_channels(&infos);
            let targets: Vec<_> = if targets.is_empty() {
                vec![(
                    String::new(),
                    channel::TextFormat::Plain,
                    2000,
                    OutboxStatus::Manual,
                )]
            } else {
                targets
                    .iter()
                    .map(|c| (c.id.clone(), c.format, c.max_bytes, OutboxStatus::Pending))
                    .collect()
            };
            for (id, fmt, max, status) in targets {
                for (b, n) in render::render_flash(&items, fmt, max).into_iter().zip(1..) {
                    let part = &items[b.items];
                    stmts.push(Statement::new("INSERT INTO outbox(kind,channel,status,attempts,batch,text,items,item_ids,created_at,next_at,last_error) VALUES('flash',?,?,0,?,?,?,?,?,?,?)",vec![json!(id),json!(crate::outbox::enum_str(&status)),json!(n),json!(b.text),json!(serde_json::to_string(part)?),json!(serde_json::to_string(&part.iter().map(|i|i.item_id).collect::<Vec<_>>())?),json!(self.now_s()),json!(self.now_s()),json!(if id.is_empty(){Some("无可用渠道")}else{None})]));
                }
            }
        }
        for row in rows {
            stmts.push(Statement::new(
                "DELETE FROM focus_pending WHERE event_key=?",
                vec![json!(row.event_key)],
            ));
        }
        self.store.batch(stmts).await?;
        Ok(())
    }
    pub(crate) async fn fresh_flash(
        &self,
        items: &[PushItem],
    ) -> Result<Vec<PushItem>, ServiceError> {
        let p = self.focus_settings().await?;
        let screens = ScreeningClient(self.ctx.service("screening"))
            .get(&items.iter().map(|i| i.item_id).collect::<Vec<_>>())
            .await?;
        Ok(items
            .iter()
            .filter(|i| {
                screens
                    .iter()
                    .find(|s| s.item_id == i.item_id)
                    .is_some_and(|s| {
                        focus::in_window(
                            i.level,
                            s.direction,
                            i.published_at.as_deref(),
                            self.now(),
                        ) && !matches!(s.level, Some(Level::Normal | Level::Archive))
                            && p.accepts(i.level, &s.assets)
                    })
            })
            .cloned()
            .collect())
    }
}

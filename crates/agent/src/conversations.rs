use crate::Agent;
use plugin_sdk::contracts::agent::{ChatTurn, ConversationTurn, RecordConversationTurn};
use plugin_sdk::contracts::{now_rfc3339, OWNER};
use plugin_sdk::ServiceError;
use serde_json::{json, Value};

pub const SCHEMA: &str = "CREATE TABLE conversation_turns (
 id INTEGER PRIMARY KEY AUTOINCREMENT, user_id TEXT NOT NULL,
 turn_key TEXT NOT NULL, conversation_key TEXT NOT NULL, channel TEXT NOT NULL,
 origin TEXT NOT NULL, question TEXT NOT NULL, reply_json TEXT NOT NULL, created_at TEXT NOT NULL,
 UNIQUE(user_id,turn_key));
 CREATE INDEX conversation_history ON conversation_turns(user_id,conversation_key,id);";
impl Agent {
    pub(crate) async fn record_turn(
        &self,
        t: &RecordConversationTurn,
    ) -> Result<i64, ServiceError> {
        self.ctx.store().execute("INSERT OR IGNORE INTO conversation_turns (user_id,turn_key,conversation_key,channel,origin,question,reply_json,created_at) VALUES (?,?,?,?,?,?,?,?)",vec![json!(OWNER),json!(t.turn_key),json!(t.conversation_key),json!(t.channel),json!(t.origin),json!(t.question),json!(t.reply_json),json!(now_rfc3339())]).await?;
        let rows = self
            .ctx
            .store()
            .query(
                "SELECT id FROM conversation_turns WHERE user_id=? AND turn_key=?",
                vec![json!(OWNER), json!(t.turn_key)],
            )
            .await?;
        Ok(rows[0]["id"].as_i64().unwrap_or_default())
    }
    pub(crate) async fn conversation_turns(
        &self,
        after: i64,
    ) -> Result<Vec<ConversationTurn>, ServiceError> {
        self.ctx.store().query_as("SELECT id,turn_key,conversation_key,channel,origin,question,reply_json,created_at FROM conversation_turns WHERE user_id=? AND id>? ORDER BY id LIMIT 100",vec![json!(OWNER),json!(after)]).await
    }
    pub(crate) async fn conversation_material_ids(
        &self,
        key: &str,
    ) -> Result<Vec<i64>, ServiceError> {
        let rows=self.ctx.store().query("SELECT reply_json FROM conversation_turns WHERE user_id=? AND conversation_key=? ORDER BY id DESC LIMIT 1",vec![json!(OWNER),json!(key)]).await?;
        let Some(raw) = rows.first().and_then(|r| r["reply_json"].as_str()) else {
            return Ok(vec![]);
        };
        let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        if let Some(ids) = v["item_ids"].as_array() {
            return Ok(ids.iter().filter_map(Value::as_i64).take(20).collect());
        }
        Ok(v["citations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["item_id"].as_i64())
            .take(20)
            .collect())
    }
    pub(crate) async fn conversation_history(
        &self,
        key: &str,
    ) -> Result<Vec<ChatTurn>, ServiceError> {
        let mut rows:Vec<ConversationTurn>=self.ctx.store().query_as("SELECT id,turn_key,conversation_key,channel,origin,question,reply_json,created_at FROM conversation_turns WHERE user_id=? AND conversation_key=? ORDER BY id DESC LIMIT 5",vec![json!(OWNER),json!(key)]).await?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .filter_map(|t| {
                let v: Value = serde_json::from_str(&t.reply_json).ok()?;
                Some(ChatTurn {
                    question: t.question,
                    answer: v["answer"].as_str()?.to_string(),
                })
            })
            .collect())
    }
}

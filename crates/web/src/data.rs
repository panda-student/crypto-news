//! web 自己的表：已读与关注、偏好、追问会话。所有访问经 store 服务，所有表带 user_id。

use std::collections::HashMap;

use plugin_sdk::contracts::store::{Statement, StoreClient};
use plugin_sdk::contracts::{now_rfc3339, ServiceError, OWNER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const SCHEMA: &str = "
CREATE TABLE item_states (
    user_id TEXT NOT NULL,
    item_id INTEGER NOT NULL,
    read_at TEXT,
    starred_at TEXT,
    PRIMARY KEY (user_id, item_id)
);
CREATE TABLE prefs (
    user_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (user_id, key)
);
CREATE TABLE chat_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    title TEXT NOT NULL,
    item_id INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_chat_sessions_user ON chat_sessions (user_id, updated_at);
CREATE TABLE chat_turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    session_id INTEGER NOT NULL,
    question TEXT NOT NULL,
    reply_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_chat_turns_session ON chat_turns (session_id, id);
";

pub(crate) const CONVERSATION_SCHEMA: &str = "
ALTER TABLE chat_sessions ADD COLUMN remote_key TEXT;
ALTER TABLE chat_turns ADD COLUMN remote_key TEXT;
CREATE UNIQUE INDEX chat_remote_session ON chat_sessions(user_id,remote_key);
CREATE UNIQUE INDEX chat_remote_turn ON chat_turns(user_id,remote_key);
";

pub(crate) const PREF_WATCH: &str = "watch_assets";
pub(crate) const PREF_LAST_SEEN: &str = "last_seen_at";
pub(crate) const DEFAULT_WATCH: &str = "";

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ItemState {
    pub read: bool,
    pub starred: bool,
}

#[derive(Deserialize)]
struct StateRow {
    item_id: i64,
    read_at: Option<String>,
    starred_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    pub id: i64,
    pub title: String,
    pub remote_key: Option<String>,
    pub item_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TurnRow {
    pub id: i64,
    pub question: String,
    pub reply_json: String,
    pub remote_key: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
struct ValueRow {
    value: String,
}

#[derive(Deserialize)]
struct StarRow {
    starred_at: Option<String>,
}

pub(crate) struct Db(pub StoreClient);

fn read_stmt(id: i64, now: &str) -> Statement {
    Statement::new(
        "INSERT INTO item_states (user_id, item_id, read_at) VALUES (?, ?, ?) \
         ON CONFLICT (user_id, item_id) DO UPDATE SET read_at = COALESCE(read_at, excluded.read_at)",
        vec![json!(OWNER), json!(id), json!(now)],
    )
}

impl Db {
    pub async fn import_conversation_turn(
        &self,
        t: &plugin_sdk::contracts::agent::ConversationTurn,
    ) -> Result<(), ServiceError> {
        let mut reply: Value = serde_json::from_str(&t.reply_json).unwrap_or(Value::Null);
        reply["origin"] = json!(t.origin);
        let prefix = if t.channel == "wechat-ilink" {
            "微信"
        } else {
            "渠道"
        };
        let title = format!(
            "{prefix} · {}",
            t.question.chars().take(18).collect::<String>()
        );
        let mut statements=vec![Statement::new(
            "INSERT OR IGNORE INTO chat_sessions(user_id,title,created_at,updated_at,remote_key) VALUES(?,?,?,?,?)",
            vec![json!(OWNER),json!(title),json!(t.created_at),json!(t.created_at),json!(t.conversation_key)]
        )];
        statements.push(Statement::new(
            "INSERT OR IGNORE INTO chat_turns(user_id,session_id,question,reply_json,created_at,remote_key) SELECT ?,id,?,?,?,? FROM chat_sessions WHERE user_id=? AND remote_key=?",
            vec![json!(OWNER),json!(t.question),json!(reply.to_string()),json!(t.created_at),json!(t.turn_key),json!(OWNER),json!(t.conversation_key)]
        ));
        statements.push(Statement::new("UPDATE chat_sessions SET updated_at=MAX(updated_at,?) WHERE user_id=? AND remote_key=?",vec![json!(t.created_at),json!(OWNER),json!(t.conversation_key)]));
        self.0.batch(statements).await?;
        Ok(())
    }
    pub async fn pending_shared(&self) -> Result<Vec<TurnRow>, ServiceError> {
        self.0.query_as("SELECT id,question,reply_json,created_at,remote_key FROM chat_turns WHERE user_id=? AND reply_json LIKE ? ORDER BY id LIMIT 100",vec![json!(OWNER),json!("%\"sync_pending\":true%")]).await
    }
    pub async fn mark_remote_turn(&self, id: i64, key: &str) -> Result<(), ServiceError> {
        self.0
            .execute(
                "UPDATE chat_turns SET remote_key=? WHERE user_id=? AND id=?",
                vec![json!(key), json!(OWNER), json!(id)],
            )
            .await?;
        Ok(())
    }

    pub async fn states(&self, ids: &[i64]) -> Result<HashMap<i64, ItemState>, ServiceError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut params: Vec<Value> = vec![json!(OWNER)];
        params.extend(ids.iter().map(|i| json!(i)));
        let rows: Vec<StateRow> = self
            .0
            .query_as(
                &format!(
                    "SELECT item_id, read_at, starred_at FROM item_states \
                     WHERE user_id = ? AND item_id IN ({marks})"
                ),
                params,
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.item_id,
                    ItemState {
                        read: r.read_at.is_some(),
                        starred: r.starred_at.is_some(),
                    },
                )
            })
            .collect())
    }

    pub async fn mark_read(&self, ids: &[i64]) -> Result<(), ServiceError> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = now_rfc3339();
        self.0
            .batch(ids.iter().map(|i| read_stmt(*i, &now)).collect())
            .await?;
        Ok(())
    }

    /// 切换关注，返回切换后是否关注。
    pub async fn toggle_star(&self, id: i64) -> Result<bool, ServiceError> {
        self.0
            .execute(
                "INSERT INTO item_states (user_id, item_id, starred_at) VALUES (?, ?, ?) \
                 ON CONFLICT (user_id, item_id) DO UPDATE SET starred_at = \
                 CASE WHEN starred_at IS NULL THEN excluded.starred_at ELSE NULL END",
                vec![json!(OWNER), json!(id), json!(now_rfc3339())],
            )
            .await?;
        let rows: Vec<StarRow> = self
            .0
            .query_as(
                "SELECT starred_at FROM item_states WHERE user_id = ? AND item_id = ?",
                vec![json!(OWNER), json!(id)],
            )
            .await?;
        Ok(rows.first().is_some_and(|r| r.starred_at.is_some()))
    }

    pub async fn pref(&self, key: &str) -> Result<Option<String>, ServiceError> {
        let rows: Vec<ValueRow> = self
            .0
            .query_as(
                "SELECT value FROM prefs WHERE user_id = ? AND key = ?",
                vec![json!(OWNER), json!(key)],
            )
            .await?;
        Ok(rows.into_iter().next().map(|r| r.value))
    }

    pub async fn set_pref(&self, key: &str, value: &str) -> Result<(), ServiceError> {
        self.0
            .execute(
                "INSERT INTO prefs (user_id, key, value) VALUES (?, ?, ?) \
                 ON CONFLICT (user_id, key) DO UPDATE SET value = excluded.value",
                vec![json!(OWNER), json!(key), json!(value)],
            )
            .await?;
        Ok(())
    }

    /// 关注币种；没设置过时用默认值。
    pub async fn watch_assets(&self) -> Vec<String> {
        let raw = match self.pref(PREF_WATCH).await {
            Ok(Some(v)) => v,
            _ => DEFAULT_WATCH.to_string(),
        };
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    // ------------------------------------------------------------ 追问会话

    pub async fn create_session(
        &self,
        title: &str,
        item_id: Option<i64>,
    ) -> Result<i64, ServiceError> {
        let now = now_rfc3339();
        let r = self
            .0
            .execute(
                "INSERT INTO chat_sessions (user_id, title, item_id, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
                vec![
                    json!(OWNER),
                    json!(title),
                    json!(item_id),
                    json!(now),
                    json!(now),
                ],
            )
            .await?;
        Ok(r.last_insert_id)
    }

    pub async fn session(&self, id: i64) -> Result<Option<Session>, ServiceError> {
        let rows: Vec<Session> = self
            .0
            .query_as(
                "SELECT id, title, item_id, created_at, updated_at, remote_key FROM chat_sessions \
                 WHERE user_id = ? AND id = ?",
                vec![json!(OWNER), json!(id)],
            )
            .await?;
        Ok(rows.into_iter().next())
    }

    pub async fn sessions(&self) -> Result<Vec<Session>, ServiceError> {
        self.0
            .query_as(
                "SELECT id, title, item_id, created_at, updated_at, remote_key FROM chat_sessions \
                 WHERE user_id = ? ORDER BY updated_at DESC, id DESC LIMIT 200",
                vec![json!(OWNER)],
            )
            .await
    }

    pub async fn rename_session(&self, id: i64, title: &str) -> Result<bool, ServiceError> {
        let r = self
            .0
            .execute(
                "UPDATE chat_sessions SET title = ? WHERE user_id = ? AND id = ?",
                vec![json!(title), json!(OWNER), json!(id)],
            )
            .await?;
        Ok(r.rows_affected > 0)
    }

    pub async fn delete_session(&self, id: i64) -> Result<(), ServiceError> {
        self.0
            .batch(vec![
                Statement::new(
                    "DELETE FROM chat_turns WHERE user_id = ? AND session_id = ?",
                    vec![json!(OWNER), json!(id)],
                ),
                Statement::new(
                    "DELETE FROM chat_sessions WHERE user_id = ? AND id = ?",
                    vec![json!(OWNER), json!(id)],
                ),
            ])
            .await?;
        Ok(())
    }

    pub async fn turns(&self, session_id: i64) -> Result<Vec<TurnRow>, ServiceError> {
        self.0
            .query_as(
                "SELECT id, question, reply_json, created_at, remote_key FROM chat_turns \
                 WHERE user_id = ? AND session_id = ? ORDER BY id",
                vec![json!(OWNER), json!(session_id)],
            )
            .await
    }

    /// 新增一轮问答，返回这一轮的 id。
    pub async fn add_turn(
        &self,
        session_id: i64,
        question: &str,
        reply_json: &str,
    ) -> Result<i64, ServiceError> {
        let now = now_rfc3339();
        let r = self
            .0
            .execute(
                "INSERT INTO chat_turns (user_id, session_id, question, reply_json, created_at)                  VALUES (?, ?, ?, ?, ?)",
                vec![
                    json!(OWNER),
                    json!(session_id),
                    json!(question),
                    json!(reply_json),
                    json!(now),
                ],
            )
            .await?;
        self.touch_session(session_id).await?;
        Ok(r.last_insert_id)
    }

    pub async fn touch_session(&self, session_id: i64) -> Result<(), ServiceError> {
        self.0
            .execute(
                "UPDATE chat_sessions SET updated_at = ? WHERE user_id = ? AND id = ?",
                vec![json!(now_rfc3339()), json!(OWNER), json!(session_id)],
            )
            .await?;
        Ok(())
    }

    /// 回答完成或被停止后，写回这一轮的内容。
    pub async fn update_turn(&self, turn_id: i64, reply_json: &str) -> Result<(), ServiceError> {
        self.0
            .execute(
                "UPDATE chat_turns SET reply_json = ? WHERE user_id = ? AND id = ?",
                vec![json!(reply_json), json!(OWNER), json!(turn_id)],
            )
            .await?;
        Ok(())
    }

    /// 撤销删除：用原来的 id 把会话和它的所有轮次放回去。
    pub async fn restore_session(
        &self,
        s: &Session,
        turns: &[TurnRow],
    ) -> Result<(), ServiceError> {
        let mut stmts = vec![Statement::new(
            "INSERT OR REPLACE INTO chat_sessions (id, user_id, title, item_id, created_at, updated_at, remote_key)              VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                json!(s.id),
                json!(OWNER),
                json!(s.title),
                json!(s.item_id),
                json!(s.created_at),
                json!(s.updated_at),
                json!(s.remote_key),
            ],
        )];
        for t in turns {
            stmts.push(Statement::new(
                "INSERT OR REPLACE INTO chat_turns (id, user_id, session_id, question, reply_json, created_at, remote_key)                  VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![
                    json!(t.id),
                    json!(OWNER),
                    json!(s.id),
                    json!(t.question),
                    json!(t.reply_json),
                    json!(t.created_at),
                    json!(t.remote_key),
                ],
            ));
        }
        self.0.batch(stmts).await?;
        Ok(())
    }
}

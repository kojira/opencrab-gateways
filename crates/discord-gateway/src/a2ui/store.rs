//! Pending A2UI surfaces of the Discord gateway (D-1110). They live in the gateway's own DB
//! (the `database_path` of the placement), not in core: a click after a restart still finds its
//! UI, and a UI that expired while the gateway was down is timed out when it comes back.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension as _};

/// One `send_ui` surface waiting for its first answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUi {
    /// The uuid in `interaction:{uuid}` (custom ids and the agent's `interaction_id`).
    pub ui_id: String,
    pub agent_id: String,
    /// The core session key the answer goes to.
    pub session_key: String,
    pub channel_id: String,
    pub message_id: Option<String>,
    /// The component tree as given to `send_ui` (re-read for buttons' context and the modal).
    pub components_json: String,
    pub expires_at_ms: i64,
}

#[derive(Clone)]
pub struct A2uiStore {
    conn: Arc<Mutex<Connection>>,
}

impl A2uiStore {
    /// Wraps a connection; the daemon and the instance processes share the DB file, so WAL and a
    /// busy timeout are set here as for the activity routes.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; \
             CREATE TABLE IF NOT EXISTS a2ui_pending ( \
               ui_id TEXT PRIMARY KEY, \
               agent_id TEXT NOT NULL, \
               session_key TEXT NOT NULL, \
               channel_id TEXT NOT NULL, \
               message_id TEXT, \
               components_json TEXT NOT NULL, \
               expires_at_ms INTEGER NOT NULL, \
               created_at_ms INTEGER NOT NULL);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("a2ui store lock poisoned"))?;
        f(&conn)
    }

    pub fn insert(&self, ui: &PendingUi) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO a2ui_pending (ui_id, agent_id, session_key, channel_id, message_id, \
                 components_json, expires_at_ms, created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    ui.ui_id,
                    ui.agent_id,
                    ui.session_key,
                    ui.channel_id,
                    ui.message_id,
                    ui.components_json,
                    ui.expires_at_ms,
                    chrono::Utc::now().timestamp_millis()
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_message_id(&self, ui_id: &str, message_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE a2ui_pending SET message_id=?2 WHERE ui_id=?1",
                params![ui_id, message_id],
            )?;
            Ok(())
        })
    }

    pub fn get(&self, ui_id: &str) -> Result<Option<PendingUi>> {
        self.with_conn(|conn| {
            let ui = conn
                .query_row(
                    "SELECT ui_id, agent_id, session_key, channel_id, message_id, components_json, \
                     expires_at_ms FROM a2ui_pending WHERE ui_id=?1",
                    params![ui_id],
                    row_to_pending,
                )
                .optional()?;
            Ok(ui)
        })
    }

    /// Removes the UI and returns it when it was still pending: the caller that gets `Some` is the
    /// one first answer (click, selection, modal or timeout); every later one gets `None`.
    pub fn take(&self, ui_id: &str) -> Result<Option<PendingUi>> {
        self.with_conn(|conn| {
            let ui = conn
                .query_row(
                    "DELETE FROM a2ui_pending WHERE ui_id=?1 RETURNING ui_id, agent_id, \
                     session_key, channel_id, message_id, components_json, expires_at_ms",
                    params![ui_id],
                    row_to_pending,
                )
                .optional()?;
            Ok(ui)
        })
    }

    /// The pending UIs of one agent (for the expiry timers after a start).
    pub fn list_for_agent(&self, agent_id: &str) -> Result<Vec<PendingUi>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ui_id, agent_id, session_key, channel_id, message_id, components_json, \
                 expires_at_ms FROM a2ui_pending WHERE agent_id=?1 ORDER BY expires_at_ms",
            )?;
            let rows = stmt
                .query_map(params![agent_id], row_to_pending)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

fn row_to_pending(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingUi> {
    Ok(PendingUi {
        ui_id: row.get(0)?,
        agent_id: row.get(1)?,
        session_key: row.get(2)?,
        channel_id: row.get(3)?,
        message_id: row.get(4)?,
        components_json: row.get(5)?,
        expires_at_ms: row.get(6)?,
    })
}

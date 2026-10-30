//! D-1099: the web gateway's own record of its conversations: session key → the instance that owns
//! it. Core keeps sessions only; which agent instance a web conversation belongs to is gateway
//! state, so the gateway keeps it across restarts.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension as _};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS conversations (
  session_key TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  created_at TEXT NOT NULL
);";

/// Session key → owning instance. In memory when no file is configured (tests, ad-hoc runs).
pub struct Conversations {
    conn: Mutex<Connection>,
}

impl Conversations {
    pub fn open(path: Option<&Path>) -> Result<Self> {
        let conn = match path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Connection::open(path)?
            }
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, session_key: &str, instance_id: &str, session_id: &str) -> Result<()> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("conversations poisoned"))?
            .execute(
                "INSERT INTO conversations(session_key,instance_id,session_id,created_at)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(session_key) DO UPDATE SET session_id=excluded.session_id",
                params![
                    session_key,
                    instance_id,
                    session_id,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
        Ok(())
    }

    /// The instance that owns `session_key`, if this gateway created it.
    pub fn owner(&self, session_key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("conversations poisoned"))?
            .query_row(
                "SELECT instance_id FROM conversations WHERE session_key=?1",
                [session_key],
                |row| row.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d1099_a_recorded_conversation_survives_a_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("conversations.db");
        let conversations = Conversations::open(Some(&path)).unwrap();
        conversations.record("web-1", "instance-a", "web-1").unwrap();
        assert_eq!(conversations.owner("web-2").unwrap(), None);
        drop(conversations);
        let reopened = Conversations::open(Some(&path)).unwrap();
        assert_eq!(
            reopened.owner("web-1").unwrap().as_deref(),
            Some("instance-a")
        );
    }
}

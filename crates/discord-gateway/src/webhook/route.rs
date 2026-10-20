//! セッション単位の活動 webhook 宛先（D-1087）。
//!
//! 宛先は **gateway の DB にだけ**持つ（core は宛先を知らない）。単位はセッション =
//! (agent_id, channel_id)。`/webhook set` が書き、`/webhook clear` が消し、配送は
//! ここを読んで「設定があるときだけ動く」。再起動・チャンネル再登録でも行が残るので、
//! 復元は「DB から読み直す」だけで足りる。

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 宛先 1 行（セッションに 1 つ）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRoute {
    pub agent_id: String,
    pub channel_id: String,
    /// 配送先 URL。応答・ログには生値を出さない（[`super::redact_webhook_url`]）。
    pub url: String,
    /// 送信対象イベント名（None = 全部）。
    pub events: Option<Vec<String>>,
    pub enabled: bool,
}

impl SessionRoute {
    /// 配送に使う設定へ変換する（無効なら None）。
    pub fn config(&self) -> Option<super::WebhookConfig> {
        if !self.enabled {
            return None;
        }
        super::WebhookConfig::from_parts(self.url.clone(), self.events.clone())
    }
}

/// 宛先ストア。gateway の DB を開いた接続を共有する。
///
/// daemon と instance（別プロセス）が同じ DB を開くため、WAL と busy timeout を
/// このハンドルで必ず設定する（読み書きは行単位で短く、長いトランザクションは持たない）。
#[derive(Clone)]
pub struct RouteStore {
    conn: Arc<Mutex<Connection>>,
}

impl RouteStore {
    /// 既に開いた接続を包む（daemon 側）。
    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; \
             CREATE TABLE IF NOT EXISTS session_activity_routes ( \
               agent_id TEXT NOT NULL, \
               channel_id TEXT NOT NULL, \
               url TEXT NOT NULL, \
               events_json TEXT, \
               enabled INTEGER NOT NULL, \
               created_at_ms INTEGER NOT NULL, \
               updated_at_ms INTEGER NOT NULL, \
               PRIMARY KEY(agent_id, channel_id));",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 自分の DB を開く（instance プロセス側）。WAL なので daemon と併存できる。
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// テスト用の in-memory ストア。
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("route store lock poisoned"))?;
        f(&conn)
    }

    /// このセッションの宛先。無ければ None。
    pub fn get(&self, agent_id: &str, channel_id: &str) -> Result<Option<SessionRoute>> {
        self.with_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT agent_id, channel_id, url, events_json, enabled \
                     FROM session_activity_routes WHERE agent_id=?1 AND channel_id=?2",
                    params![agent_id, channel_id],
                    |row| {
                        let events_json: Option<String> = row.get(3)?;
                        Ok(SessionRoute {
                            agent_id: row.get(0)?,
                            channel_id: row.get(1)?,
                            url: row.get(2)?,
                            events: events_json.map(|raw| parse_events(&raw)),
                            enabled: row.get::<_, i64>(4)? != 0,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    /// 宛先を upsert する（`/webhook set` と既存行の再有効化の両方）。
    pub fn set(&self, route: &SessionRoute, now_ms: i64) -> Result<()> {
        let events = route
            .events
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO session_activity_routes \
                   (agent_id, channel_id, url, events_json, enabled, created_at_ms, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
                 ON CONFLICT(agent_id, channel_id) DO UPDATE SET \
                   url = excluded.url, events_json = excluded.events_json, \
                   enabled = excluded.enabled, updated_at_ms = excluded.updated_at_ms",
                params![
                    route.agent_id,
                    route.channel_id,
                    route.url,
                    events,
                    i64::from(route.enabled),
                    now_ms
                ],
            )?;
            Ok(())
        })
    }

    /// 宛先を消す。消した行があったかを返す（`clear` の応答に使う）。
    pub fn clear(&self, agent_id: &str, channel_id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let removed = conn.execute(
                "DELETE FROM session_activity_routes WHERE agent_id=?1 AND channel_id=?2",
                params![agent_id, channel_id],
            )?;
            Ok(removed > 0)
        })
    }
}

fn parse_events(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

/// 宛先が無いときの記録を間引くための最小限のレート制限。
///
/// 「設定が無いから送らない」は正常な状態なので、活動のたびに WARN を出すとログが埋まる。
/// セッションごとに一定間隔でだけ debug 1 行を残す（黙って捨てない）。
pub struct AbsenceLog {
    interval: Duration,
    last: Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl AbsenceLog {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 記録すべきなら true（呼び出し側が 1 行出す）。
    pub fn should_log(&self, agent_id: &str, channel_id: &str) -> bool {
        let key = format!("{agent_id}\u{1f}{channel_id}");
        let Ok(mut last) = self.last.lock() else {
            return false;
        };
        let now = std::time::Instant::now();
        match last.get(&key) {
            Some(previous) if now.duration_since(*previous) < self.interval => false,
            _ => {
                last.insert(key, now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> SessionRoute {
        SessionRoute {
            agent_id: "agent-1".into(),
            channel_id: "channel-9".into(),
            url: "https://discord.com/api/webhooks/1/token".into(),
            events: None,
            enabled: true,
        }
    }

    #[test]
    fn set_then_get_round_trips_and_survives_reopen() {
        let store = RouteStore::in_memory().unwrap();
        assert!(store.get("agent-1", "channel-9").unwrap().is_none());
        store.set(&route(), 1).unwrap();
        let got = store.get("agent-1", "channel-9").unwrap().unwrap();
        assert_eq!(got.url, route().url);
        assert!(got.enabled);
        assert_eq!(got.events, None);
    }

    #[test]
    fn set_is_per_session_not_per_agent() {
        let store = RouteStore::in_memory().unwrap();
        store.set(&route(), 1).unwrap();
        assert!(store.get("agent-1", "other-channel").unwrap().is_none());
        assert!(store.get("other-agent", "channel-9").unwrap().is_none());
    }

    #[test]
    fn clear_removes_only_that_session_and_reports_whether_a_row_existed() {
        let store = RouteStore::in_memory().unwrap();
        store.set(&route(), 1).unwrap();
        assert!(store.clear("agent-1", "channel-9").unwrap());
        assert!(!store.clear("agent-1", "channel-9").unwrap());
        assert!(store.get("agent-1", "channel-9").unwrap().is_none());
    }

    #[test]
    fn events_filter_round_trips() {
        let store = RouteStore::in_memory().unwrap();
        let mut with_events = route();
        with_events.events = Some(vec!["started".into(), "completed".into()]);
        store.set(&with_events, 1).unwrap();
        let got = store.get("agent-1", "channel-9").unwrap().unwrap();
        assert_eq!(
            got.events,
            Some(vec!["started".to_string(), "completed".to_string()])
        );
    }

    #[test]
    fn disabled_routes_resolve_to_no_config() {
        let store = RouteStore::in_memory().unwrap();
        let mut disabled = route();
        disabled.enabled = false;
        store.set(&disabled, 1).unwrap();
        assert!(store
            .get("agent-1", "channel-9")
            .unwrap()
            .unwrap()
            .config()
            .is_none());
    }

    #[test]
    fn absence_log_is_rate_limited_per_session() {
        let log = AbsenceLog::new(Duration::from_secs(60));
        assert!(log.should_log("agent-1", "channel-9"));
        assert!(!log.should_log("agent-1", "channel-9"));
        assert!(log.should_log("agent-1", "channel-8"));
    }
}

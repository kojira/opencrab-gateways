//! Discord-owned durable configuration, policy, identity, credential, and lifecycle store.

use crate::secret_store;
use anyhow::{Context as _, Result};
use opencrab_process_supervisor::lifecycle::{LifecycleState, PersistedLifecycle};
use rusqlite::{params, Connection, OptionalExtension as _};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

const SCHEMA: &str = r#"
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS instances (
  instance_id TEXT PRIMARY KEY,
  agent_id TEXT NOT NULL UNIQUE,
  config_b64 TEXT NOT NULL,
  addresses_json TEXT NOT NULL,
  credential_envelope TEXT NOT NULL,
  enabled INTEGER NOT NULL,
  desired_generation INTEGER NOT NULL,
  applied_generation INTEGER,
  lifecycle_state TEXT NOT NULL,
  process_id INTEGER,
  process_nonce TEXT,
  failure_count INTEGER NOT NULL DEFAULT 0,
  retry_at_unix_ms INTEGER,
  last_exit TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS endpoints (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  channel_id TEXT NOT NULL,
  guild_id TEXT,
  readable INTEGER NOT NULL,
  writable INTEGER NOT NULL,
  policy_json TEXT NOT NULL,
  PRIMARY KEY(instance_id, channel_id)
);
CREATE TABLE IF NOT EXISTS identity_projections (
  instance_id TEXT NOT NULL REFERENCES instances(instance_id) ON DELETE CASCADE,
  role TEXT NOT NULL,
  external_id TEXT NOT NULL,
  relationship_id TEXT,
  relationship_revision INTEGER,
  PRIMARY KEY(instance_id, role, external_id)
);
CREATE TABLE IF NOT EXISTS legacy_identity_sources (
  instance_id TEXT NOT NULL,
  id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  permission TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  display_name TEXT NOT NULL,
  platform TEXT NOT NULL,
  PRIMARY KEY(instance_id, id)
);
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow {
    pub instance_id: String,
    pub agent_id: String,
    pub config_b64: String,
    pub addresses: Vec<String>,
    pub credential_envelope: String,
    pub enabled: bool,
    pub desired_generation: u64,
    pub applied_generation: Option<u64>,
    pub lifecycle_state: LifecycleState,
    pub process_id: Option<u32>,
    pub process_nonce: Option<String>,
    pub failure_count: u32,
    pub retry_at_unix_ms: Option<i64>,
}

impl InstanceRow {
    pub fn lifecycle(&self) -> PersistedLifecycle {
        PersistedLifecycle {
            desired_generation: self.desired_generation,
            applied_generation: self.applied_generation,
            enabled: self.enabled,
            state: self.lifecycle_state,
            retry_at_unix_ms: self.retry_at_unix_ms,
            process_nonce: self.process_nonce.clone(),
        }
    }
}

pub struct DiscordStore {
    path: PathBuf,
    conn: Connection,
}

impl DiscordStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        drop_core_columns(&conn)?;
        Ok(Self {
            path: path.into(),
            conn,
        })
    }

    /// Runs `f` in one transaction: every write in it commits, or none does.
    pub fn in_transaction<T>(&self, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_desired(
        &self,
        instance_id: &str,
        agent_id: &str,
        config_b64: &str,
        addresses: &[String],
        credential: &str,
        enabled: bool,
        key: &[u8; secret_store::MASTER_KEY_LEN],
    ) -> Result<u64> {
        let existing = self.get(instance_id)?;
        let generation = existing
            .as_ref()
            .map_or(1, |row| row.desired_generation + 1);
        let envelope = if credential.is_empty() {
            existing
                .as_ref()
                .map(|row| row.credential_envelope.clone())
                .context("credential is required for a new instance")?
        } else {
            secret_store::encrypt(credential.as_bytes(), key)?
        };
        self.conn.execute(
            "INSERT INTO instances
             (instance_id, agent_id, config_b64, addresses_json, credential_envelope,
              enabled, desired_generation, applied_generation, lifecycle_state, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,'pending',?8)
             ON CONFLICT(instance_id) DO UPDATE SET
               agent_id=excluded.agent_id,
               config_b64=excluded.config_b64, addresses_json=excluded.addresses_json,
               credential_envelope=excluded.credential_envelope, enabled=excluded.enabled,
               desired_generation=excluded.desired_generation, lifecycle_state='pending',
               process_id=NULL, process_nonce=NULL, retry_at_unix_ms=NULL,
               updated_at=excluded.updated_at",
            params![
                instance_id,
                agent_id,
                config_b64,
                serde_json::to_string(addresses)?,
                envelope,
                enabled,
                generation,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(generation)
    }

    pub fn get(&self, instance_id: &str) -> Result<Option<InstanceRow>> {
        self.conn.query_row(
            "SELECT instance_id,agent_id,config_b64,addresses_json,credential_envelope,
                    enabled,desired_generation,applied_generation,lifecycle_state,
                    process_id,process_nonce,failure_count,retry_at_unix_ms
             FROM instances WHERE instance_id=?1",
            params![instance_id],
            |row| {
                let state: String = row.get(8)?;
                let desired: i64 = row.get(6)?;
                let applied: Option<i64> = row.get(7)?;
                let failures: i64 = row.get(11)?;
                Ok(InstanceRow {
                    instance_id: row.get(0)?, agent_id: row.get(1)?,
                    config_b64: row.get(2)?,
                    addresses: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                    credential_envelope: row.get(4)?, enabled: row.get(5)?,
                    desired_generation: u64::try_from(desired).unwrap_or(0),
                    applied_generation: applied.and_then(|v| u64::try_from(v).ok()),
                    lifecycle_state: LifecycleState::from_str(&state).unwrap_or(LifecycleState::Error),
                    process_id: row.get::<_, Option<i64>>(9)?.and_then(|value| u32::try_from(value).ok()),
                    process_nonce: row.get(10)?,
                    failure_count: u32::try_from(failures).unwrap_or(u32::MAX),
                    retry_at_unix_ms: row.get(12)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    /// D-1097/D-1099: records one more channel address on the same generation, so the running
    /// child keeps running. Which channels a bot listens to is gateway state only.
    pub fn add_address(&self, instance_id: &str, address: &str) -> Result<bool> {
        let row = self.get(instance_id)?.context("instance unknown")?;
        if row.addresses.iter().any(|a| a == address) {
            return Ok(false);
        }
        let mut addresses = row.addresses.clone();
        addresses.push(address.to_string());
        self.conn.execute(
            "UPDATE instances SET addresses_json=?2, updated_at=?3
             WHERE instance_id=?1 AND desired_generation=?4",
            params![
                instance_id,
                serde_json::to_string(&addresses)?,
                chrono::Utc::now().to_rfc3339(),
                row.desired_generation
            ],
        )?;
        Ok(true)
    }

    /// The last exit code recorded for the instance (`child_exit`, `reconciliation_failed`, ...).
    pub fn last_exit(&self, instance_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_exit FROM instances WHERE instance_id=?1",
                params![instance_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// The instance of `agent_id`, if this gateway has one.
    pub fn get_by_agent(&self, agent_id: &str) -> Result<Option<InstanceRow>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|row| row.agent_id == agent_id))
    }

    pub fn retry_due_error(
        &self,
        instance_id: &str,
        generation: u64,
        now_unix_ms: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='pending',
             process_id=NULL,process_nonce=NULL,retry_at_unix_ms=NULL
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state='error'
               AND (retry_at_unix_ms IS NULL OR retry_at_unix_ms<=?3)",
            params![instance_id, generation, now_unix_ms],
        )? == 1)
    }

    pub fn list(&self) -> Result<Vec<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT instance_id FROM instances ORDER BY instance_id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| self.get(&id)?.context("instance vanished"))
            .collect()
    }

    pub fn mark_provisioning(&self, instance_id: &str, generation: u64) -> Result<bool> {
        self.transition(instance_id, generation, "pending", "provisioning")
    }

    /// The gateway accepted the generation: the child may start (or stay stopped when disabled).
    pub fn mark_verified(&self, instance_id: &str, generation: u64, enabled: bool) -> Result<bool> {
        let state = if enabled { "ready" } else { "disabled" };
        Ok(self.conn.execute(
            "UPDATE instances SET applied_generation=?2,lifecycle_state=?3,
             process_id=NULL,process_nonce=NULL,
             failure_count=0,retry_at_unix_ms=NULL,updated_at=?4
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state='provisioning'
               AND enabled=?5",
            params![
                instance_id,
                generation,
                state,
                chrono::Utc::now().to_rfc3339(),
                enabled
            ],
        )? == 1)
    }

    pub fn record_started(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET process_id=?3,process_nonce=?4,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2 AND applied_generation=?2
             AND lifecycle_state='ready' AND enabled=1",
            params![
                instance_id,
                generation,
                pid,
                nonce,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn mark_running(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='running',updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2
             AND applied_generation=?2 AND lifecycle_state='ready' AND enabled=1
             AND process_id=?3 AND process_nonce=?4",
            params![
                instance_id,
                generation,
                pid,
                nonce,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn mark_pending(&self, instance_id: &str, generation: u64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='pending',process_id=NULL,process_nonce=NULL,
             updated_at=?3 WHERE instance_id=?1 AND desired_generation=?2",
            params![instance_id, generation, chrono::Utc::now().to_rfc3339()],
        )? == 1)
    }

    pub fn mark_error(
        &self,
        instance_id: &str,
        generation: u64,
        code: &str,
        retry_at: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state='error',process_id=NULL,process_nonce=NULL,
             failure_count=failure_count+1,retry_at_unix_ms=?4,last_exit=?3,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2",
            params![
                instance_id,
                generation,
                code,
                retry_at,
                chrono::Utc::now().to_rfc3339()
            ],
        )? == 1)
    }

    pub fn decrypt_credential(
        &self,
        instance_id: &str,
        key: &[u8; 32],
    ) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let row = self.get(instance_id)?.context("instance unknown")?;
        secret_store::decrypt(&row.credential_envelope, key)
    }

    fn transition(&self, id: &str, generation: u64, from: &str, to: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE instances SET lifecycle_state=?4,updated_at=?5
             WHERE instance_id=?1 AND desired_generation=?2 AND lifecycle_state=?3",
            params![id, generation, from, to, chrono::Utc::now().to_rfc3339()],
        )? == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s5_discord_store_encrypts_secret_and_persists_lifecycle_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("discord.db");
        let key = [7_u8; 32];
        let store = DiscordStore::open(&path).unwrap();
        let generation = store
            .upsert_desired(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                "Y29uZmln",
                &["opaque-address".into()],
                "token-secret",
                true,
                &key,
            )
            .unwrap();
        let row = store
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert_eq!(row.lifecycle_state, LifecycleState::Pending);
        assert!(!row.credential_envelope.contains("token-secret"));
        assert!(store
            .mark_provisioning(&row.instance_id, generation)
            .unwrap());
        assert!(store
            .mark_verified(&row.instance_id, generation, true)
            .unwrap());
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes
            .windows(b"token-secret".len())
            .any(|window| window == b"token-secret"));
        assert!(!bytes
            .windows(b"grant-secret-at-rest".len())
            .any(|window| window == b"grant-secret-at-rest"));
        let reopened = DiscordStore::open(&path).unwrap();
        let row = reopened
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert!(row.lifecycle().child_may_start());
        assert_eq!(
            &*reopened.decrypt_credential(&row.instance_id, &key).unwrap(),
            b"token-secret"
        );
    }

    #[test]
    fn s5_discord_disabled_and_nonready_instances_never_eligible() {
        let temp = tempfile::tempdir().unwrap();
        let store = DiscordStore::open(&temp.path().join("discord.db")).unwrap();
        let generation = store
            .upsert_desired(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "agent",
                "Y29uZmln",
                &[],
                "token",
                false,
                &[8; 32],
            )
            .unwrap();
        let pending = store
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap();
        assert!(!pending.lifecycle().child_may_start());
        store
            .mark_provisioning(&pending.instance_id, generation)
            .unwrap();
        store
            .mark_verified(&pending.instance_id, generation, false)
            .unwrap();
        let disabled = store.get(&pending.instance_id).unwrap().unwrap();
        assert_eq!(disabled.lifecycle_state, LifecycleState::Disabled);
        assert!(!disabled.lifecycle().child_may_start());
    }
}

/// D-1099: core keeps no per-instance state for gateways any more, so the store drops the
/// columns that mirrored core (revision, digest, binding ids). Idempotent.
fn drop_core_columns(conn: &Connection) -> Result<()> {
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(instances)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    for column in ["core_revision", "core_digest", "binding_inventory_json"] {
        if columns.iter().any(|c| c == column) {
            conn.execute_batch(&format!("ALTER TABLE instances DROP COLUMN {column}"))?;
        }
    }
    Ok(())
}

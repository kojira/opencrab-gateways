//! D-1097 N1: settings edits on an existing instance.
//!
//! Every edit runs in one transaction that also bumps `desired_generation` and returns the row
//! to `pending`. The daemon's reconcile then stops the old child and starts one with the new
//! settings, so no process has to be restarted by hand. Edits never create an instance.

use crate::config::AccessConfig;
use crate::secret_store;
use crate::store::NostrStore;
use anyhow::{Context as _, Result};
use rusqlite::{params, Transaction};

impl NostrStore {
    /// Replace the gateway-owned runtime config (relays, filter, name, self_pubkey).
    /// The caller passes canonical config; the credential and access are kept.
    pub fn update_config(&self, instance_id: &str, config_b64: &str) -> Result<u64> {
        self.edit(instance_id, |tx| {
            tx.execute(
                "UPDATE instances SET config_b64=?2 WHERE instance_id=?1",
                params![instance_id, config_b64],
            )?;
            Ok(())
        })
    }

    /// Replace the whole access set (`identity_projections`) for the instance.
    pub fn set_access(&self, instance_id: &str, access: &AccessConfig) -> Result<u64> {
        self.edit(instance_id, |tx| {
            tx.execute(
                "DELETE FROM identity_projections WHERE instance_id=?1",
                params![instance_id],
            )?;
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO identity_projections
                 (instance_id, role, external_id, relationship_id, relationship_revision)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
            )?;
            for owner in &access.owner {
                insert.execute(params![instance_id, "owner", owner, None::<String>])?;
            }
            for trusted in &access.trusted_users {
                insert.execute(params![instance_id, "trusted_user", trusted, None::<String>])?;
            }
            for followee in &access.followees {
                insert.execute(params![instance_id, "followee", followee, None::<String>])?;
            }
            for (pubkey, agent_id) in &access.co_agents {
                insert.execute(params![instance_id, "co_agent", pubkey, agent_id])?;
            }
            Ok(())
        })
    }

    /// Replace the encrypted Nostr secret key. The plaintext never reaches the database.
    pub fn set_credential(
        &self,
        instance_id: &str,
        credential: &str,
        key: &[u8; secret_store::MASTER_KEY_LEN],
    ) -> Result<u64> {
        let credential = credential.trim();
        anyhow::ensure!(!credential.is_empty(), "credential must be nonempty");
        let envelope = secret_store::encrypt(credential.as_bytes(), key)?;
        self.edit(instance_id, |tx| {
            tx.execute(
                "UPDATE instances SET credential_envelope=?2 WHERE instance_id=?1",
                params![instance_id, envelope],
            )?;
            Ok(())
        })
    }

    pub fn set_enabled(&self, instance_id: &str, enabled: bool) -> Result<u64> {
        self.edit(instance_id, |tx| {
            tx.execute(
                "UPDATE instances SET enabled=?2 WHERE instance_id=?1",
                params![instance_id, enabled],
            )?;
            Ok(())
        })
    }

    fn edit(
        &self,
        instance_id: &str,
        change: impl FnOnce(&Transaction<'_>) -> Result<()>,
    ) -> Result<u64> {
        let tx = self.conn().unchecked_transaction()?;
        let current: i64 = tx
            .query_row(
                "SELECT desired_generation FROM instances WHERE instance_id=?1",
                params![instance_id],
                |row| row.get(0),
            )
            .map_err(|_| anyhow::anyhow!("instance unknown"))?;
        change(&tx)?;
        let generation = u64::try_from(current)
            .context("stored generation is negative")?
            .saturating_add(1);
        tx.execute(
            "UPDATE instances SET desired_generation=?2, lifecycle_state='pending',
             process_id=NULL, process_nonce=NULL, retry_at_unix_ms=NULL, updated_at=?3
             WHERE instance_id=?1",
            params![instance_id, generation, chrono::Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(generation)
    }
}

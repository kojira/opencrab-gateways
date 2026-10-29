//! D-1097 D3: one-time import of the instance-mode placements and token files into the daemon
//! store. It runs only while the store is empty, so after the first start the placements are
//! never read again and the store is the only authority.
//!
//! Every file is checked before anything is written, and the rows are written in one
//! transaction: either all instances are imported or none.

use crate::config::{canonicalize_config_b64, decode_config_b64, parse_instance_config, Placement};
use crate::store::DiscordStore;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyImport {
    /// The instance-mode placements, one instance each.
    pub placements: Vec<PathBuf>,
    /// Holds `<agent_id>.token` (mode 0600) for every placement.
    pub token_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    StoreNotEmpty,
    Imported(usize),
}

/// Core could not be asked yet (it may still be starting). The daemon retries these; every
/// other import error stops the daemon.
#[derive(Debug)]
pub struct CoreUnavailable(pub anyhow::Error);

impl std::fmt::Display for CoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "core unavailable for the legacy import: {:#}", self.0)
    }
}

impl std::error::Error for CoreUnavailable {}

/// The core side of an instance: `(kind_id, subject_id)`, or `None` when core has no instance.
#[async_trait::async_trait]
pub trait SubjectLookup: Send + Sync {
    async fn core_instance(&self, instance_id: &str) -> Result<Option<(String, i64)>>;
}

impl LegacyImport {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.placements.is_empty(),
            "legacy_import.placements must be nonempty"
        );
        for path in &self.placements {
            anyhow::ensure!(
                path.is_absolute(),
                "legacy_import.placements must be absolute"
            );
        }
        anyhow::ensure!(
            self.token_dir.is_absolute(),
            "legacy_import.token_dir must be absolute"
        );
        Ok(())
    }
}

struct Planned {
    instance_id: String,
    agent_id: String,
    config_b64: String,
    addresses: Vec<String>,
    token: Zeroizing<String>,
}

pub async fn import_legacy(
    store: &DiscordStore,
    import: &LegacyImport,
    core: &dyn SubjectLookup,
    key: &[u8; 32],
) -> Result<ImportOutcome> {
    if !store.list()?.is_empty() {
        return Ok(ImportOutcome::StoreNotEmpty);
    }
    import.validate()?;
    let planned = plan(import)?;
    let mut subjects = Vec::with_capacity(planned.len());
    for item in &planned {
        let observed = core
            .core_instance(&item.instance_id)
            .await
            .map_err(|error| anyhow::Error::new(CoreUnavailable(error)))?;
        let (kind, subject_id) = observed
            .with_context(|| format!("core has no instance {} to import", item.instance_id))?;
        anyhow::ensure!(
            kind == "discord",
            "core instance {} is not a Discord instance",
            item.instance_id
        );
        subjects.push(subject_id);
    }
    store.in_transaction(|store| {
        for (item, subject_id) in planned.iter().zip(&subjects) {
            store.upsert_desired(
                &item.instance_id,
                &item.agent_id,
                *subject_id,
                &item.config_b64,
                &item.addresses,
                &item.token,
                None,
                true,
                key,
            )?;
        }
        Ok(())
    })?;
    Ok(ImportOutcome::Imported(planned.len()))
}

fn plan(import: &LegacyImport) -> Result<Vec<Planned>> {
    let mut planned = Vec::new();
    let mut instance_ids = std::collections::BTreeSet::new();
    let mut agent_ids = std::collections::BTreeSet::new();
    for path in &import.placements {
        let place = Placement::load(path)
            .with_context(|| format!("legacy placement {}", path.display()))?;
        anyhow::ensure!(
            place.declared_run_env().is_empty(),
            "legacy placement {} declares run_env, which the store cannot carry",
            path.display()
        );
        let instance = place
            .instances
            .first()
            .context("placement has no instance")?;
        let config_b64 = canonicalize_config_b64(&instance.config_b64)?;
        let config = parse_instance_config(&decode_config_b64(&config_b64)?)?;
        anyhow::ensure!(
            instance_ids.insert(instance.instance_id.clone()),
            "instance {} appears twice",
            instance.instance_id
        );
        anyhow::ensure!(
            agent_ids.insert(config.agent_id.clone()),
            "agent {} appears twice",
            config.agent_id
        );
        let token = read_token(&import.token_dir.join(format!("{}.token", config.agent_id)))?;
        planned.push(Planned {
            instance_id: instance.instance_id.clone(),
            agent_id: config.agent_id,
            config_b64,
            addresses: instance.addresses.clone(),
            token,
        });
    }
    Ok(planned)
}

fn read_token(path: &Path) -> Result<Zeroizing<String>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("token file {} is missing", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "token file {} must be a regular file",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o777 == 0o600,
            "token file {} must be mode 0600",
            path.display()
        );
    }
    let raw = Zeroizing::new(std::fs::read_to_string(path)?);
    let token = Zeroizing::new(raw.trim().to_string());
    anyhow::ensure!(!token.is_empty(), "token file {} is empty", path.display());
    Ok(token)
}

#[cfg(test)]
#[path = "legacy_import_tests.rs"]
mod tests;

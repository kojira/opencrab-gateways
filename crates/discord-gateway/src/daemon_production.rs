//! The production child spawner: writes the daemon-owned child placement and starts the child.

use super::SpawnerFactory;
use crate::store::InstanceRow;
use anyhow::{Context as _, Result};
use opencrab_process_supervisor::ChildSpawner;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) struct ProductionFactory {
    pub(crate) child_binary: PathBuf,
    pub(crate) core_socket: PathBuf,
    pub(crate) placement_dir: PathBuf,
    pub(crate) attachment_spool_root: Option<PathBuf>,
    pub(crate) attachment_outbox_root: Option<PathBuf>,
    /// Gateway store DB. The instance opens it for the per-session activity webhook routes
    /// (D-1087); the daemon owns the schema and the master key.
    pub(crate) database_path: PathBuf,
    /// D-1097: the shared VC settings file the settings page writes.
    pub(crate) voice_settings: PathBuf,
}

impl SpawnerFactory for ProductionFactory {
    fn observe_process(
        &self,
        row: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        use opencrab_process_supervisor::lifecycle::ProcessObservation;
        let Some(pid) = row.process_id else {
            return Ok(ProcessObservation::Missing);
        };
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            let placement = self.placement_dir.join(format!("{}.json", row.instance_id));
            let exact_nonce = std::fs::read(&placement)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| {
                    value
                        .get("start_nonce")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .is_some_and(|nonce| Some(nonce.as_str()) == row.process_nonce.as_deref());
            return Ok(if exact_nonce {
                ProcessObservation::ExactLive
            } else {
                ProcessObservation::StaleLive
            });
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(ProcessObservation::Missing),
            _ => Ok(ProcessObservation::Unknown),
        }
    }

    fn reap_stale(&self, row: &InstanceRow) -> Result<()> {
        let placement = self.placement_dir.join(format!("{}.json", row.instance_id));
        let owned = std::fs::read(&placement)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get("start_nonce")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|nonce| Some(nonce.as_str()) == row.process_nonce.as_deref());
        if owned {
            if let Some(pid) = row.process_id {
                if unsafe { libc::kill(pid as i32, 0) } == 0 {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                }
            }
        }
        Ok(())
    }

    fn adopted_child(
        &self,
        row: &InstanceRow,
    ) -> Result<Option<Box<dyn opencrab_process_supervisor::SupervisedChild>>> {
        Ok(row.process_id.map(|pid| {
            Box::new(opencrab_process_supervisor::AdoptedPidChild::new(pid))
                as Box<dyn opencrab_process_supervisor::SupervisedChild>
        }))
    }

    fn cleanup_artifacts(&self, row: &InstanceRow) -> Result<()> {
        for path in [
            self.placement_dir.join(format!("{}.json", row.instance_id)),
            self.placement_dir
                .join(format!("{}.control.sock", row.instance_id)),
            crate::channel_request::socket_path(&self.placement_dir, &row.instance_id),
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn channel_socket(&self, row: &InstanceRow) -> Option<PathBuf> {
        Some(crate::channel_request::socket_path(
            &self.placement_dir,
            &row.instance_id,
        ))
    }

    fn control_socket(&self, row: &InstanceRow, _: &str) -> Option<PathBuf> {
        Some(
            self.placement_dir
                .join(format!("{}.control.sock", row.instance_id)),
        )
    }

    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        start_nonce: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        let revision = row.core_revision.context("verified revision missing")?;
        std::fs::create_dir_all(&self.placement_dir)?;
        let path = self.placement_dir.join(format!("{}.json", row.instance_id));
        let control_socket = self
            .control_socket(row, "")
            .expect("production control socket");
        let value = serde_json::json!({
            "core_socket": self.core_socket,
            "attachment_spool_root": self.attachment_spool_root,
            "attachment_outbox_root": self.attachment_outbox_root,
            "database_path": self.database_path,
            "voice_settings": self.voice_settings,
            "channel_socket": self.channel_socket(row),
            "control_socket": control_socket,
            "start_nonce": start_nonce,
            "instances": [{"instance_id":row.instance_id,"revision":revision,
                "addresses":row.addresses,"config_b64":row.config_b64,
                "core_config_b64": super::core_config_b64_for_runtime_config(&row.config_b64)?}]
        });
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&value)?)?;
        std::fs::rename(temporary, &path)?;
        let secret = String::from_utf8(credential.to_vec()).context("credential is not UTF-8")?;
        Ok(Arc::new(
            opencrab_process_supervisor::ExternalProcessSpawner::with_secret_env(
                self.child_binary.clone(),
                path,
                secret,
                crate::secret::TOKEN_ENV,
                "discord-adapter",
                row.instance_id.clone(),
            ),
        ))
    }
}

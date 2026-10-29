//! Discord-owned lifecycle daemon. Core is reached only through a generic control trait.

use crate::store::{DiscordStore, InstanceRow};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use opencrab_process_supervisor::{
    ChildSpawner, ProcessObserver, ProcessSupervisorSet, SupervisorConfig,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub database_path: PathBuf,
    pub admin_socket: PathBuf,
    /// Core's gate socket: the daemon provisions instances on it and the children connect to it.
    pub core_socket: PathBuf,
    pub child_binary: PathBuf,
    pub placement_dir: PathBuf,
    #[serde(default)]
    pub attachment_spool_root: Option<PathBuf>,
    /// Core-owned outbox of workspace files to attach (D-1079), passed to each child placement.
    #[serde(default)]
    pub attachment_outbox_root: Option<PathBuf>,
    #[serde(default = "default_reconcile_millis")]
    pub reconcile_millis: u64,
    /// D-1097 D3: instance-mode placements and token files to import while the store is empty.
    #[serde(default)]
    pub legacy_import: Option<crate::legacy_import::LegacyImport>,
    /// D-1097: the VC settings file (see [`DaemonConfig::voice_settings_path`]).
    #[serde(default)]
    pub voice_settings: Option<PathBuf>,
    /// D-1097: core's public HTTP API, read by the settings page for the agent list
    /// (for example `http://127.0.0.1:8080`).
    #[serde(default)]
    pub core_api: Option<String>,
}

fn default_reconcile_millis() -> u64 {
    1_000
}

/// D-1097 D2: the one config core holds for every Discord instance.
pub const DEFAULT_CORE_CONFIG_B64: &str = "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==";

/// The config to register in core for an instance whose Discord config is `config_b64`. The
/// Discord config is only validated here; core never sees it.
pub fn core_config_b64_for_runtime_config(config_b64: &str) -> Result<String> {
    crate::config::parse_instance_config(&crate::config::decode_config_b64(config_b64)?)?;
    Ok(DEFAULT_CORE_CONFIG_B64.to_string())
}

fn retry_deadline(row: &InstanceRow) -> i64 {
    let shift = row.failure_count.min(6);
    let delay = 1_000_i64.saturating_mul(1_i64 << shift).min(60_000);
    chrono::Utc::now().timestamp_millis().saturating_add(delay)
}

impl DaemonConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        value.validate()?;
        Ok(value)
    }

    /// D-1097: the VC settings file the children and the settings page share. Optional in the
    /// daemon config; absent = `voice/settings.json` next to the store.
    pub fn voice_settings_path(&self) -> PathBuf {
        self.voice_settings.clone().unwrap_or_else(|| {
            self.database_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join("voice")
                .join("settings.json")
        })
    }

    pub fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("database_path", &self.database_path),
            ("admin_socket", &self.admin_socket),
            ("core_socket", &self.core_socket),
            ("child_binary", &self.child_binary),
            ("placement_dir", &self.placement_dir),
        ] {
            anyhow::ensure!(path.is_absolute(), "{name} must be absolute");
        }
        if let Some(path) = &self.attachment_outbox_root {
            anyhow::ensure!(
                path.is_absolute(),
                "attachment_outbox_root must be absolute"
            );
        }
        if let Some(import) = &self.legacy_import {
            import.validate()?;
        }
        if let Some(path) = &self.voice_settings {
            anyhow::ensure!(path.is_absolute(), "voice_settings must be absolute");
        }
        anyhow::ensure!(
            self.reconcile_millis > 0,
            "reconcile_millis must be positive"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedInstance {
    pub revision: u64,
    pub digest: String,
    pub bindings: Vec<String>,
    pub enabled: bool,
}

#[async_trait]
pub trait GateReconciler: Send + Sync {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation>;

    /// Makes core hold `desired` (instance and complete binding set) and returns what the
    /// child's `hello` must carry.
    async fn reconcile(&self, desired: &InstanceRow) -> Result<VerifiedInstance>;
}

pub use crate::core_provisioning::ProvisionReconciler;

pub trait SpawnerFactory: Send + Sync {
    fn observe_process(
        &self,
        _row: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        Ok(opencrab_process_supervisor::lifecycle::ProcessObservation::Missing)
    }
    fn reap_stale(&self, _row: &InstanceRow) -> Result<()> {
        Ok(())
    }
    fn cleanup_artifacts(&self, _row: &InstanceRow) -> Result<()> {
        Ok(())
    }
    fn adopted_child(
        &self,
        _row: &InstanceRow,
    ) -> Result<Option<Box<dyn opencrab_process_supervisor::SupervisedChild>>> {
        Ok(None)
    }
    fn control_socket(&self, _row: &InstanceRow, _start_nonce: &str) -> Option<PathBuf> {
        None
    }
    /// D-1097: where this instance's child asks for one more channel. `None` = not offered.
    fn channel_socket(&self, _row: &InstanceRow) -> Option<PathBuf> {
        None
    }
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        start_nonce: &str,
    ) -> Result<Arc<dyn ChildSpawner>>;
}

struct StoreObserver {
    store: Arc<Mutex<DiscordStore>>,
    generation: u64,
    nonce: String,
}

#[async_trait]
impl ProcessObserver for StoreObserver {
    async fn spawned(&self, target_id: &str, pid: Option<u32>) {
        if let Some(pid) = pid {
            let _ = self.store.lock().ok().and_then(|store| {
                store
                    .record_started(target_id, self.generation, pid, &self.nonce)
                    .ok()
            });
        }
    }

    async fn spawn_failed(&self, target_id: &str) {
        self.persist_error(target_id, "child_start_exit");
    }

    async fn exited(&self, target_id: &str, _: &str) {
        self.persist_error(target_id, "child_exit");
    }
}

impl StoreObserver {
    fn persist_error(&self, target_id: &str, code: &str) {
        let retry = chrono::Utc::now().timestamp_millis() + 1_000;
        if let Ok(store) = self.store.lock() {
            let _ = store.mark_error(target_id, self.generation, code, retry);
        }
    }
}

pub struct DiscordDaemon<R, F> {
    store: Arc<Mutex<DiscordStore>>,
    key: Arc<[u8; 32]>,
    reconciler: Arc<R>,
    factory: Arc<F>,
    supervisors: Arc<ProcessSupervisorSet>,
    startup_recovered: std::sync::atomic::AtomicBool,
    /// D-1097: one channel-request listener per started child, stopped with the child.
    channel_tasks: Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<Result<()>>>>,
    /// Set by [`DiscordDaemon::serve_channel_requests`]; `None` = no listeners are started.
    channel_binder: Mutex<Option<Arc<dyn crate::channel_request::ChannelBinder>>>,
}

impl<R, F> DiscordDaemon<R, F>
where
    R: GateReconciler + 'static,
    F: SpawnerFactory + 'static,
{
    pub fn new(
        store: DiscordStore,
        key: [u8; 32],
        reconciler: Arc<R>,
        factory: Arc<F>,
    ) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            key: Arc::new(key),
            reconciler,
            factory,
            supervisors: ProcessSupervisorSet::new(SupervisorConfig::daemon_owned()),
            startup_recovered: std::sync::atomic::AtomicBool::new(false),
            channel_tasks: Mutex::new(std::collections::HashMap::new()),
            channel_binder: Mutex::new(None),
        })
    }

    pub fn store(&self) -> Arc<Mutex<DiscordStore>> {
        self.store.clone()
    }

    async fn recover_startup(&self) -> Result<()> {
        use opencrab_process_supervisor::lifecycle::{
            CoreObservation, ProcessObservation, StartupDecision,
        };
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .list()?;
        let now = chrono::Utc::now().timestamp_millis();
        for row in rows {
            let process = self
                .factory
                .observe_process(&row)
                .unwrap_or(ProcessObservation::Unknown);
            let core = self
                .reconciler
                .observe(&row)
                .await
                .unwrap_or(CoreObservation::Unavailable);
            let decision = row.lifecycle().recover(now, process, core);
            if !matches!(decision, StartupDecision::AdoptRunning) {
                let _ = self.factory.reap_stale(&row);
                let _ = self.factory.cleanup_artifacts(&row);
            }
            match decision {
                StartupDecision::KeepDisabled => {}
                StartupDecision::EnqueuePending => {
                    self.store
                        .lock()
                        .unwrap()
                        .mark_pending(&row.instance_id, row.desired_generation)?;
                }
                StartupDecision::StartReady => {}
                StartupDecision::AdoptRunning => {
                    let child = self
                        .factory
                        .adopted_child(&row)?
                        .context("exact live process could not be adopted")?;
                    self.supervisors
                        .adopt_observed(
                            &row.instance_id,
                            child,
                            Arc::new(StoreObserver {
                                store: self.store.clone(),
                                generation: row.desired_generation,
                                nonce: row.process_nonce.clone().unwrap_or_default(),
                            }),
                        )
                        .await;
                }
                StartupDecision::PersistError(code) => {
                    self.store.lock().unwrap().mark_error(
                        &row.instance_id,
                        row.desired_generation,
                        code,
                        retry_deadline(&row),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub async fn reconcile_once(&self) -> Result<()> {
        if !self
            .startup_recovered
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.recover_startup().await?;
        }
        let rows = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .list()?;
        for row in rows {
            match row.lifecycle_state {
                opencrab_process_supervisor::lifecycle::LifecycleState::Pending => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.stop_channel_listener(&row.instance_id);
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                    if !self
                        .store
                        .lock()
                        .unwrap()
                        .mark_provisioning(&row.instance_id, row.desired_generation)?
                    {
                        continue;
                    }
                    let verified = match self.reconciler.reconcile(&row).await {
                        Ok(value) => value,
                        Err(error) => {
                            self.factory.cleanup_artifacts(&row)?;
                            self.store.lock().unwrap().mark_error(
                                &row.instance_id,
                                row.desired_generation,
                                "reconciliation_failed",
                                retry_deadline(&row),
                            )?;
                            tracing::warn!(instance_id = %row.instance_id, error = %error, "instance reconciliation failed");
                            continue;
                        }
                    };
                    anyhow::ensure!(
                        self.store.lock().unwrap().mark_verified(
                            &row.instance_id,
                            row.desired_generation,
                            verified.revision,
                            &verified.digest,
                            &verified.bindings,
                            verified.enabled,
                        )?,
                        "stale lifecycle verification commit"
                    );
                    if verified.enabled {
                        if let Err(error) = self.start_ready(&row.instance_id).await {
                            self.factory.cleanup_artifacts(&row)?;
                            self.store.lock().unwrap().mark_error(
                                &row.instance_id,
                                row.desired_generation,
                                "child_start_failed",
                                retry_deadline(&row),
                            )?;
                            tracing::warn!(instance_id = %row.instance_id, error = %error, "child start failed");
                        }
                    } else {
                        self.factory.cleanup_artifacts(&row)?;
                    }
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Ready => {
                    if let Err(error) = self.start_ready(&row.instance_id).await {
                        self.factory.cleanup_artifacts(&row)?;
                        self.store.lock().unwrap().mark_error(
                            &row.instance_id,
                            row.desired_generation,
                            "child_start_failed",
                            retry_deadline(&row),
                        )?;
                        tracing::warn!(instance_id = %row.instance_id, error = %error, "child start failed");
                    }
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
                | opencrab_process_supervisor::lifecycle::LifecycleState::Provisioning => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.stop_channel_listener(&row.instance_id);
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Error => {
                    self.supervisors.stop(&row.instance_id).await;
                    self.stop_channel_listener(&row.instance_id);
                    self.factory.reap_stale(&row)?;
                    self.factory.cleanup_artifacts(&row)?;
                    let _ = self.store.lock().unwrap().retry_due_error(
                        &row.instance_id,
                        row.desired_generation,
                        chrono::Utc::now().timestamp_millis(),
                    )?;
                }
                opencrab_process_supervisor::lifecycle::LifecycleState::Running => {}
            }
        }
        Ok(())
    }

    async fn start_ready(&self, instance_id: &str) -> Result<()> {
        let row = self
            .store
            .lock()
            .unwrap()
            .get(instance_id)?
            .context("instance missing")?;
        if !row.lifecycle().child_may_start() || row.process_nonce.is_some() {
            return Ok(());
        }
        let credential = self
            .store
            .lock()
            .unwrap()
            .decrypt_credential(instance_id, &self.key)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let readiness = match self.factory.control_socket(&row, &nonce) {
            Some(path) => Some(prepare_readiness_listener(&path)?),
            None => None,
        };
        self.start_channel_listener(&row);
        let spawner = self.factory.for_instance(&row, credential, &nonce)?;
        let observer = Arc::new(StoreObserver {
            store: self.store.clone(),
            generation: row.desired_generation,
            nonce: nonce.clone(),
        });
        self.supervisors
            .start_observed(instance_id, spawner, observer)
            .await;
        if let Some(listener) = readiness {
            let store = self.store.clone();
            let id = row.instance_id.clone();
            let generation = row.desired_generation;
            tokio::spawn(wait_for_child_readiness(
                listener,
                store,
                self.supervisors.clone(),
                self.factory.clone(),
                id,
                generation,
                nonce,
            ));
        }
        Ok(())
    }

    pub fn confirm_child_ready(
        &self,
        instance_id: &str,
        generation: u64,
        pid: u32,
        nonce: &str,
    ) -> Result<bool> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .mark_running(instance_id, generation, pid, nonce)
    }

    pub async fn shutdown(&self) {
        for (_, task) in self.channel_tasks.lock().unwrap().drain() {
            task.abort();
        }
        self.supervisors.shutdown_all().await;
    }
}

fn prepare_readiness_listener(path: &Path) -> Result<tokio::net::UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

async fn wait_for_child_readiness<F: SpawnerFactory + 'static>(
    listener: tokio::net::UnixListener,
    store: Arc<Mutex<DiscordStore>>,
    supervisors: Arc<ProcessSupervisorSet>,
    factory: Arc<F>,
    instance_id: String,
    generation: u64,
    expected_nonce: String,
) {
    use tokio::io::AsyncReadExt as _;
    let ready = async {
        let (_, mut stream) = {
            let (stream, address) = listener.accept().await?;
            (address, stream)
        };
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let nonce = value
            .get("nonce")
            .and_then(serde_json::Value::as_str)
            .context("readiness nonce missing")?;
        let pid = value
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .context("readiness pid missing")?;
        anyhow::ensure!(nonce == expected_nonce, "readiness nonce mismatch");
        Ok::<(u32, String), anyhow::Error>((pid, nonce.to_string()))
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), ready).await;
    let parsed = match result {
        Ok(Ok(value)) => value,
        _ => {
            supervisors.stop(&instance_id).await;
            if let Ok(guard) = store.lock() {
                if let Ok(Some(row)) = guard.get(&instance_id) {
                    let _ = factory.cleanup_artifacts(&row);
                    let _ = guard.mark_error(
                        &instance_id,
                        generation,
                        "child_readiness_failed",
                        retry_deadline(&row),
                    );
                }
            }
            return;
        }
    };
    if let Ok(store) = store.lock() {
        let _ = store.mark_running(&instance_id, generation, parsed.0, &parsed.1);
    }
}

#[path = "daemon_production.rs"]
mod production;
use production::ProductionFactory;

pub async fn run(config: DaemonConfig) -> Result<()> {
    config.validate()?;
    let lock_path = config.database_path.with_extension("lock");
    let _lock = opencrab_process_supervisor::lock::StoreLock::acquire(&lock_path)
        .context("another Discord daemon owns this store")?;
    let encoded_key = crate::secret::take_master_key().context("Discord master key is required")?;
    let key = crate::secret_store::parse_master_key(&encoded_key)?;
    let mut raw = [0_u8; 32];
    raw.copy_from_slice(&key[..]);
    let store = DiscordStore::open(&config.database_path)?;
    if let Some(import) = &config.legacy_import {
        let outcome = crate::legacy_import::import_legacy(&store, import, &raw)
            .context("discord legacy import failed")?;
        tracing::info!(?outcome, "discord legacy import");
    }
    let daemon = Arc::new(DiscordDaemon::new(
        store,
        raw,
        Arc::new(ProvisionReconciler::new(
            config.core_socket.clone(),
            "discord",
        )),
        Arc::new(ProductionFactory {
            child_binary: config.child_binary.clone(),
            core_socket: config.core_socket.clone(),
            placement_dir: config.placement_dir.clone(),
            attachment_spool_root: config.attachment_spool_root.clone(),
            attachment_outbox_root: config.attachment_outbox_root.clone(),
            database_path: config.database_path.clone(),
            voice_settings: config.voice_settings_path(),
        }),
    )?);
    daemon.serve_channel_requests(Arc::new(DaemonChannels(Arc::downgrade(&daemon))));
    let _admin = crate::admin::spawn(config.admin_socket.clone(), daemon.store(), Arc::new(raw));
    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(config.reconcile_millis));
    loop {
        interval.tick().await;
        daemon.reconcile_once().await?;
    }
}

#[path = "daemon_channels.rs"]
mod channels;
pub use channels::DaemonChannels;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "daemon_settings_tests.rs"]
mod settings_tests;

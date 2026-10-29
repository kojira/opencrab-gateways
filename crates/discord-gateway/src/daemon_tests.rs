use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

/// `{"agent_id":"agent","self_bot_id":"111111111111111111"}`: the smallest valid Discord config.
const VALID_CONFIG_B64: &str =
    "eyJhZ2VudF9pZCI6ImFnZW50Iiwic2VsZl9ib3RfaWQiOiIxMTExMTExMTExMTExMTExMTEifQ==";

struct FakeGate;
#[async_trait]
impl GateReconciler for FakeGate {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(if desired.enabled {
            opencrab_process_supervisor::lifecycle::CoreObservation::ExactEnabled
        } else {
            opencrab_process_supervisor::lifecycle::CoreObservation::ExactDisabled
        })
    }

    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance> {
        assert!(subject_grant.is_none() || subject_grant == Some("grant-secret"));
        Ok(VerifiedInstance {
            revision: 4,
            digest: "digest".into(),
            bindings: desired.addresses.clone(),
            enabled: desired.enabled,
        })
    }
}

struct OfflineGate;
#[async_trait]
impl GateReconciler for OfflineGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Unavailable)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("core server is stopped")
    }
}

struct FailingGate;
#[async_trait]
impl GateReconciler for FailingGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Mismatch)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("core unavailable")
    }
}

struct RecoveryGate(opencrab_process_supervisor::lifecycle::CoreObservation);
#[async_trait]
impl GateReconciler for RecoveryGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(self.0)
    }

    async fn reconcile(&self, _: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        anyhow::bail!("reconciliation not expected during startup matrix")
    }
}

struct RecoveryFactory {
    observation: opencrab_process_supervisor::lifecycle::ProcessObservation,
    reaped: Arc<AtomicUsize>,
    cleaned: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
impl SpawnerFactory for RecoveryFactory {
    fn observe_process(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
        Ok(self.observation)
    }
    fn reap_stale(&self, _: &InstanceRow) -> Result<()> {
        self.reaped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn cleanup_artifacts(&self, _: &InstanceRow) -> Result<()> {
        self.cleaned.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn adopted_child(&self, _: &InstanceRow) -> Result<Option<Box<dyn SupervisedChild>>> {
        Ok(Some(Box::new(FakeChild {
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        })))
    }
    fn for_instance(
        &self,
        _: &InstanceRow,
        _: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        anyhow::bail!("spawn not expected during startup matrix")
    }
}

struct FakeChild {
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
#[async_trait]
impl SupervisedChild for FakeChild {
    async fn wait_exit(&mut self) -> String {
        self.exit.notified().await;
        "crash".into()
    }
    async fn kill(&mut self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
    }
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct FakeSpawner {
    target: String,
    started: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
#[async_trait]
impl ChildSpawner for FakeSpawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeChild {
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        }))
    }
    fn target_id(&self) -> &str {
        &self.target
    }
    fn service_name(&self) -> &str {
        "discord-adapter"
    }
}

struct FakeFactory {
    started: Arc<AtomicUsize>,
    exit: Arc<Notify>,
    kills: Arc<AtomicUsize>,
}
impl SpawnerFactory for FakeFactory {
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        assert_eq!(&*credential, b"token-secret");
        Ok(Arc::new(FakeSpawner {
            target: row.instance_id.clone(),
            started: self.started.clone(),
            exit: self.exit.clone(),
            kills: self.kills.clone(),
        }))
    }
}

fn daemon(
    enabled: bool,
) -> (
    tempfile::TempDir,
    DiscordDaemon<FakeGate, FakeFactory>,
    Arc<AtomicUsize>,
    Arc<Notify>,
) {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &["opaque-address".into()],
            "token-secret",
            Some("grant-secret"),
            enabled,
            &[9; 32],
        )
        .unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let exit = Arc::new(Notify::new());
    let factory = Arc::new(FakeFactory {
        started: started.clone(),
        exit: exit.clone(),
        kills: Arc::new(AtomicUsize::new(0)),
    });
    (
        temp,
        DiscordDaemon::new(store, [9; 32], Arc::new(FakeGate), factory).unwrap(),
        started,
        exit,
    )
}

#[tokio::test]
async fn s5_discord_daemon_runs_and_crash_state_persists_while_server_is_stopped() {
    let (_temp, daemon, started, exit) = daemon(true);
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1);
    let store = daemon.store();
    let ready = store
        .lock()
        .unwrap()
        .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .unwrap()
        .unwrap();
    assert_eq!(
        ready.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Ready
    );
    let nonce = ready.process_nonce.clone().unwrap();
    assert!(daemon
        .confirm_child_ready(&ready.instance_id, ready.desired_generation, 4242, &nonce)
        .unwrap());
    exit.notify_one();
    for _ in 0..100 {
        if store
            .lock()
            .unwrap()
            .get(&ready.instance_id)
            .unwrap()
            .unwrap()
            .lifecycle_state
            == opencrab_process_supervisor::lifecycle::LifecycleState::Error
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let crashed = store
        .lock()
        .unwrap()
        .get(&ready.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        crashed.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "durable daemon, not utility, owns retry"
    );
    tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
    daemon.reconcile_once().await.unwrap();
    assert_eq!(
        daemon
            .store()
            .lock()
            .unwrap()
            .get(&ready.instance_id)
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Pending
    );
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_restart_starts_persisted_ready_child_while_server_is_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("owner.db");
    let store = DiscordStore::open(&path).unwrap();
    let generation = store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &["opaque-address".into()],
            "token-secret",
            Some("grant-secret"),
            true,
            &[9; 32],
        )
        .unwrap();
    assert!(store
        .mark_provisioning("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", generation)
        .unwrap());
    assert!(store
        .mark_verified(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            generation,
            4,
            "digest",
            &["binding".into()],
            true,
        )
        .unwrap());
    drop(store);

    let started = Arc::new(AtomicUsize::new(0));
    let daemon = DiscordDaemon::new(
        DiscordStore::open(&path).unwrap(),
        [9; 32],
        Arc::new(OfflineGate),
        Arc::new(FakeFactory {
            started: started.clone(),
            exit: Arc::new(Notify::new()),
            kills: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(started.load(Ordering::SeqCst), 1);
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_daemon_startup_recovery_matrix_uses_process_and_core_observations() {
    use opencrab_process_supervisor::lifecycle::{
        CoreObservation, LifecycleState, ProcessObservation,
    };
    for state in [
        LifecycleState::Disabled,
        LifecycleState::Pending,
        LifecycleState::Provisioning,
        LifecycleState::Ready,
        LifecycleState::Running,
        LifecycleState::Error,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
        let enabled = state != LifecycleState::Disabled;
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let generation = store
            .upsert_desired(
                id,
                "agent",
                7,
                "e30=",
                &[],
                "token",
                None,
                enabled,
                &[9; 32],
            )
            .unwrap();
        if !matches!(state, LifecycleState::Pending) {
            store.mark_provisioning(id, generation).unwrap();
        }
        if matches!(
            state,
            LifecycleState::Disabled
                | LifecycleState::Ready
                | LifecycleState::Running
                | LifecycleState::Error
        ) {
            store
                .mark_verified(id, generation, 1, "digest", &[], enabled)
                .unwrap();
        }
        if matches!(state, LifecycleState::Running) {
            store.record_started(id, generation, 4242, "nonce").unwrap();
            store.mark_running(id, generation, 4242, "nonce").unwrap();
        }
        if matches!(state, LifecycleState::Error) {
            store
                .mark_error(
                    id,
                    generation,
                    "failed",
                    chrono::Utc::now().timestamp_millis() + 60_000,
                )
                .unwrap();
        }
        let reaped = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let daemon = DiscordDaemon::new(
            store,
            [9; 32],
            Arc::new(RecoveryGate(if enabled {
                CoreObservation::ExactEnabled
            } else {
                CoreObservation::ExactDisabled
            })),
            Arc::new(RecoveryFactory {
                observation: if state == LifecycleState::Running {
                    ProcessObservation::ExactLive
                } else {
                    ProcessObservation::Missing
                },
                reaped: reaped.clone(),
                cleaned: cleaned.clone(),
                exit: Arc::new(Notify::new()),
                kills: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        daemon.recover_startup().await.unwrap();
        let recovered = daemon.store().lock().unwrap().get(id).unwrap().unwrap();
        let expected = match state {
            LifecycleState::Provisioning => LifecycleState::Pending,
            other => other,
        };
        assert_eq!(recovered.lifecycle_state, expected, "{state:?}");
        assert_eq!(
            reaped.load(Ordering::SeqCst),
            usize::from(state != LifecycleState::Running),
            "{state:?}"
        );
        assert_eq!(
            cleaned.load(Ordering::SeqCst),
            usize::from(state != LifecycleState::Running),
            "{state:?}"
        );
        daemon.shutdown().await;
    }
}

#[tokio::test]
async fn s5_discord_running_loss_and_stale_process_are_reaped_to_durable_error() {
    use opencrab_process_supervisor::lifecycle::{
        CoreObservation, LifecycleState, ProcessObservation,
    };
    for observation in [ProcessObservation::Missing, ProcessObservation::StaleLive] {
        let temp = tempfile::tempdir().unwrap();
        let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let generation = store
            .upsert_desired(id, "agent", 7, "e30=", &[], "token", None, true, &[9; 32])
            .unwrap();
        store.mark_provisioning(id, generation).unwrap();
        store
            .mark_verified(id, generation, 1, "digest", &[], true)
            .unwrap();
        store.record_started(id, generation, 4242, "nonce").unwrap();
        store.mark_running(id, generation, 4242, "nonce").unwrap();
        let reaped = Arc::new(AtomicUsize::new(0));
        let daemon = DiscordDaemon::new(
            store,
            [9; 32],
            Arc::new(RecoveryGate(CoreObservation::ExactEnabled)),
            Arc::new(RecoveryFactory {
                observation,
                reaped: reaped.clone(),
                cleaned: Arc::new(AtomicUsize::new(0)),
                exit: Arc::new(Notify::new()),
                kills: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();
        daemon.recover_startup().await.unwrap();
        assert_eq!(
            daemon
                .store()
                .lock()
                .unwrap()
                .get(id)
                .unwrap()
                .unwrap()
                .lifecycle_state,
            LifecycleState::Error
        );
        assert_eq!(reaped.load(Ordering::SeqCst), 1);
        daemon.shutdown().await;
    }
}

#[tokio::test]
async fn s5_discord_reconciliation_failure_is_durable_and_does_not_kill_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &[],
            "token-secret",
            Some("grant-secret"),
            true,
            &[9; 32],
        )
        .unwrap();
    let daemon = DiscordDaemon::new(
        store,
        [9; 32],
        Arc::new(FailingGate),
        Arc::new(FakeFactory {
            started: Arc::new(AtomicUsize::new(0)),
            exit: Arc::new(Notify::new()),
            kills: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    assert!(daemon.reconcile_once().await.is_ok());
    assert_eq!(
        daemon
            .store()
            .lock()
            .unwrap()
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_malformed_readiness_reaps_child_and_persists_retry() {
    use tokio::io::AsyncWriteExt as _;
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let generation = store
        .upsert_desired(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "agent",
            7,
            "e30=",
            &[],
            "token-secret",
            None,
            true,
            &[9; 32],
        )
        .unwrap();
    store
        .mark_provisioning("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", generation)
        .unwrap();
    store
        .mark_verified(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            generation,
            1,
            "digest",
            &[],
            true,
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let socket = temp.path().join("ready.sock");
    let listener = prepare_readiness_listener(&socket).unwrap();
    let supervisors = ProcessSupervisorSet::new(SupervisorConfig::daemon_owned());
    let factory = Arc::new(FakeFactory {
        started: Arc::new(AtomicUsize::new(0)),
        exit: Arc::new(Notify::new()),
        kills: Arc::new(AtomicUsize::new(0)),
    });
    let waiter = tokio::spawn(wait_for_child_readiness(
        listener,
        store.clone(),
        supervisors,
        factory,
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        generation,
        "expected".into(),
    ));
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream.write_all(b"{}").await.unwrap();
    stream.shutdown().await.unwrap();
    waiter.await.unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Error
    );
}

#[tokio::test]
async fn s5_discord_disabled_recovery_removes_daemon_owned_placement_and_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let placement_dir = temp.path().join("placements");
    std::fs::create_dir_all(&placement_dir).unwrap();
    let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let placement = placement_dir.join(format!("{instance_id}.json"));
    let control = placement_dir.join(format!("{instance_id}.control.sock"));
    std::fs::write(&placement, b"owned").unwrap();
    std::fs::write(&control, b"owned").unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let generation = store
        .upsert_desired(
            instance_id,
            "agent",
            7,
            "e30=",
            &[],
            "token",
            None,
            false,
            &[9; 32],
        )
        .unwrap();
    store.mark_provisioning(instance_id, generation).unwrap();
    store
        .mark_verified(instance_id, generation, 1, "digest", &[], false)
        .unwrap();
    let daemon = DiscordDaemon::new(
        store,
        [9; 32],
        Arc::new(OfflineGate),
        Arc::new(ProductionFactory {
            child_binary: "/bin/true".into(),
            core_socket: temp.path().join("core.sock"),
            placement_dir,
            attachment_spool_root: None,
            attachment_outbox_root: None,
            database_path: temp.path().join("owner.db"),
        }),
    )
    .unwrap();
    daemon.reconcile_once().await.unwrap();
    assert!(!placement.exists());
    assert!(!control.exists());
    daemon.shutdown().await;
}

#[tokio::test]
async fn s5_discord_disabled_and_nonready_instances_never_spawn() {
    let (_temp, daemon, started, _) = daemon(false);
    daemon.reconcile_once().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(started.load(Ordering::SeqCst), 0);
    let row = daemon
        .store()
        .lock()
        .unwrap()
        .get("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        .unwrap()
        .unwrap();
    assert_eq!(
        row.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
    );
    daemon.shutdown().await;
}

#[test]
fn s5_discord_runtime_config_rejects_core_and_legacy_database_paths() {
    let value = serde_json::json!({
        "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
        "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
        "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
        "core_database_path":"/tmp/core.db"
    });
    assert!(serde_json::from_value::<DaemonConfig>(value).is_err());
    let legacy = serde_json::json!({
        "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
        "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
        "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
        "legacy_database_path":"/tmp/core.db"
    });
    assert!(serde_json::from_value::<DaemonConfig>(legacy).is_err());
}

#[test]
fn d1079_production_placement_carries_attachment_outbox_root() {
    let temp = tempfile::tempdir().unwrap();
    let placement_dir = temp.path().join("placements");
    let factory = ProductionFactory {
        child_binary: "/bin/true".into(),
        core_socket: temp.path().join("core.sock"),
        placement_dir: placement_dir.clone(),
        attachment_spool_root: Some(temp.path().join("inbox")),
        attachment_outbox_root: Some(temp.path().join("outbox")),
        database_path: temp.path().join("owner.db"),
    };
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let generation = store
        .upsert_desired(
            instance_id,
            "agent",
            7,
            VALID_CONFIG_B64,
            &[],
            "token",
            None,
            true,
            &[9; 32],
        )
        .unwrap();
    store.mark_provisioning(instance_id, generation).unwrap();
    store
        .mark_verified(instance_id, generation, 1, "digest", &[], true)
        .unwrap();
    let row = store.get(instance_id).unwrap().unwrap();
    factory
        .for_instance(&row, zeroize::Zeroizing::new(b"token".to_vec()), "nonce")
        .unwrap();
    let written: serde_json::Value = serde_json::from_slice(
        &std::fs::read(placement_dir.join(format!("{instance_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        written["attachment_outbox_root"],
        serde_json::json!(temp.path().join("outbox"))
    );
}

#[test]
fn d1079_daemon_config_requires_absolute_attachment_outbox_root() {
    let config = |root: &str| {
        serde_json::from_value::<DaemonConfig>(serde_json::json!({
            "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
            "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
            "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
            "attachment_outbox_root": root
        }))
        .unwrap()
    };
    config("/tmp/outbox").validate().unwrap();
    assert!(config("outbox").validate().is_err());
}

/// D-1097 D2: core receives one fixed neutral config. Discord access (owners, trusted users,
/// co-agents), delivery mode and reactions stay in the gateway, so changing them never needs a
/// core revision or a core digest change.
#[test]
fn d1097_core_config_is_fixed_and_neutral_whatever_the_discord_config() {
    use base64::Engine as _;
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap())
    };
    let plain = encode(serde_json::json!({"agent_id":"a","self_bot_id":"111111111111111111"}));
    let with_access = encode(serde_json::json!({
        "agent_id":"a","self_bot_id":"111111111111111111","delivery_mode":"tool_driven",
        "access":{"owners":["222222222222222222"],"trusted_users":["333333333333333333"]}
    }));
    let first = core_config_b64_for_runtime_config(&plain).unwrap();
    let second = core_config_b64_for_runtime_config(&with_access).unwrap();
    assert_eq!(first, DEFAULT_CORE_CONFIG_B64);
    assert_eq!(second, DEFAULT_CORE_CONFIG_B64);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(DEFAULT_CORE_CONFIG_B64)
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        !text.contains("222222222222222222") && !text.contains("access"),
        "{text}"
    );
    assert!(core_config_b64_for_runtime_config("not base64").is_err());
}

/// D-1097 D2: the child's hello digest is the digest of the fixed core config, not of the
/// Discord config the child runs with, so the core instance matches.
#[test]
fn d1097_child_placement_carries_the_neutral_core_config() {
    use base64::Engine as _;
    let temp = tempfile::tempdir().unwrap();
    let placement_dir = temp.path().join("placements");
    let factory = ProductionFactory {
        child_binary: "/bin/true".into(),
        core_socket: temp.path().join("core.sock"),
        placement_dir: placement_dir.clone(),
        attachment_spool_root: None,
        attachment_outbox_root: None,
        database_path: temp.path().join("owner.db"),
    };
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let config = base64::engine::general_purpose::STANDARD.encode(
        br#"{"agent_id":"a","self_bot_id":"111111111111111111","access":{"owners":["222222222222222222"]}}"#,
    );
    let generation = store
        .upsert_desired(
            instance_id,
            "a",
            7,
            &config,
            &["discord-a-1-2".into()],
            "token",
            None,
            true,
            &[9; 32],
        )
        .unwrap();
    store.mark_provisioning(instance_id, generation).unwrap();
    store
        .mark_verified(instance_id, generation, 1, "digest", &[], true)
        .unwrap();
    let row = store.get(instance_id).unwrap().unwrap();
    factory
        .for_instance(&row, zeroize::Zeroizing::new(b"token".to_vec()), "nonce")
        .unwrap();
    let path = placement_dir.join(format!("{instance_id}.json"));
    let placement = crate::config::Placement::load(&path).unwrap();
    let instance = &placement.instances[0];
    assert_eq!(
        instance.config_b64, config,
        "the child runs with the Discord config"
    );
    assert_eq!(
        instance.core_config_b64.as_deref(),
        Some(DEFAULT_CORE_CONFIG_B64)
    );
    let core_bytes = crate::config::decode_config_b64(DEFAULT_CORE_CONFIG_B64).unwrap();
    assert_eq!(
        instance.hello_digest().unwrap(),
        crate::config::config_digest(&core_bytes)
    );
}

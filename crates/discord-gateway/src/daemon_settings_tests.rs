//! D-1097 D1: a settings change rebuilds only the changed instance's child. D-1099: the daemon
//! never talks to core.

use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [9; 32];

#[derive(Default)]
struct CountingFactory {
    starts: Mutex<BTreeMap<String, usize>>,
    kills: Arc<AtomicUsize>,
}

struct Child(Arc<AtomicUsize>);
#[async_trait]
impl SupervisedChild for Child {
    async fn wait_exit(&mut self) -> String {
        std::future::pending::<()>().await;
        String::new()
    }
    async fn kill(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct Spawner(String, Arc<AtomicUsize>);
#[async_trait]
impl ChildSpawner for Spawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        Ok(Box::new(Child(self.1.clone())))
    }
    fn target_id(&self) -> &str {
        &self.0
    }
    fn service_name(&self) -> &str {
        "discord-adapter"
    }
}

impl SpawnerFactory for CountingFactory {
    fn for_instance(
        &self,
        row: &InstanceRow,
        _: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        *self
            .starts
            .lock()
            .unwrap()
            .entry(row.instance_id.clone())
            .or_default() += 1;
        Ok(Arc::new(Spawner(
            row.instance_id.clone(),
            self.kills.clone(),
        )))
    }
}

fn config(owners: &[&str]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_vec(&serde_json::json!({
            "agent_id":"agent","self_bot_id":"111111111111111111","access":{"owners":owners}
        }))
        .unwrap(),
    )
}

#[tokio::test]
async fn d1097_a_settings_change_rebuilds_only_that_child() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    for (id, agent) in [(A, "agent"), (B, "other")] {
        let raw = config(&[]);
        let cfg = if agent == "agent" {
            raw
        } else {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .encode(br#"{"agent_id":"other","self_bot_id":"111111111111111111"}"#)
        };
        store
            .upsert_desired(
                id,
                agent,
                &cfg,
                &[format!("discord-{agent}-1-2")],
                "t",
                true,
                &KEY,
            )
            .unwrap();
    }
    let factory = Arc::new(CountingFactory::default());
    let daemon = DiscordDaemon::new(store, KEY, factory.clone()).unwrap();
    daemon.reconcile_once().await.unwrap();
    assert_eq!(factory.starts.lock().unwrap().get(A), Some(&1));
    assert_eq!(factory.starts.lock().unwrap().get(B), Some(&1));

    let response = crate::admin_settings::set_config(
        &daemon.store().lock().unwrap(),
        A,
        &serde_json::json!({"agent_id":"agent","self_bot_id":"111111111111111111",
            "access":{"owners":["222222222222222222"]}}),
        &KEY,
    )
    .unwrap();
    assert_eq!(response["state"], "pending");
    daemon.reconcile_once().await.unwrap();
    assert_eq!(
        factory.starts.lock().unwrap().get(A),
        Some(&2),
        "the changed instance is rebuilt"
    );
    assert_eq!(
        factory.starts.lock().unwrap().get(B),
        Some(&1),
        "the other instance is untouched"
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn d1097_disabling_stops_the_running_bot_and_enabling_restarts_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            A,
            "agent",
            &config(&[]),
            &["discord-agent-1-2".into()],
            "t",
            true,
            &KEY,
        )
        .unwrap();
    let factory = Arc::new(CountingFactory::default());
    let daemon = DiscordDaemon::new(store, KEY, factory.clone()).unwrap();
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(factory.starts.lock().unwrap().get(A), Some(&1));

    crate::admin_settings::set_enabled(&daemon.store().lock().unwrap(), A, false, &KEY).unwrap();
    daemon.reconcile_once().await.unwrap();
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        factory.kills.load(Ordering::SeqCst),
        1,
        "the running bot is stopped"
    );
    assert_eq!(
        daemon
            .store()
            .lock()
            .unwrap()
            .get(A)
            .unwrap()
            .unwrap()
            .lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Disabled
    );

    crate::admin_settings::set_enabled(&daemon.store().lock().unwrap(), A, true, &KEY).unwrap();
    daemon.reconcile_once().await.unwrap();
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        factory.starts.lock().unwrap().get(A),
        Some(&2),
        "enabling starts it again"
    );
    daemon.shutdown().await;
}

/// D-1097/D-1099 owner mention: the daemon records a channel the child started listening to,
/// without a rebuild, and it is still there after a restart. Core is not involved.
#[tokio::test]
async fn d1097_record_address_persists_without_a_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("owner.db");
    let store = DiscordStore::open(&path).unwrap();
    store
        .upsert_desired(
            A,
            "agent",
            &config(&[]),
            &["discord-agent-1-2".into()],
            "t",
            true,
            &KEY,
        )
        .unwrap();
    let daemon = DiscordDaemon::new(store, KEY, Arc::new(CountingFactory::default())).unwrap();
    daemon.reconcile_once().await.unwrap();
    let generation = daemon
        .store()
        .lock()
        .unwrap()
        .get(A)
        .unwrap()
        .unwrap()
        .desired_generation;
    daemon.record_address(A, "discord-agent-1-3").unwrap();
    assert!(daemon.record_address(A, "discord-other-1-3").is_err());
    let row = daemon.store().lock().unwrap().get(A).unwrap().unwrap();
    assert_eq!(
        row.addresses,
        vec!["discord-agent-1-2", "discord-agent-1-3"]
    );
    assert_eq!(row.desired_generation, generation, "no rebuild");
    daemon.shutdown().await;
    drop(daemon);
    assert_eq!(
        DiscordStore::open(&path)
            .unwrap()
            .get(A)
            .unwrap()
            .unwrap()
            .addresses,
        vec!["discord-agent-1-2", "discord-agent-1-3"],
        "still listened after a restart"
    );
}

/// D-1097: a bot created with no channel gets a placement its child accepts, so it starts and
/// an owner mention can enable its first channel.
#[test]
fn d1097_a_bot_without_a_channel_gets_a_valid_child_placement() {
    let temp = tempfile::tempdir().unwrap();
    let placement_dir = temp.path().join("placements");
    let factory = ProductionFactory {
        child_binary: "/bin/true".into(),
        core_socket: temp.path().join("core.sock"),
        placement_dir: placement_dir.clone(),
        attachment_spool_root: None,
        attachment_outbox_root: None,
        database_path: temp.path().join("owner.db"),
        voice_settings: temp.path().join("voice/settings.json"),
    };
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let plan = crate::admin_settings::plan_create(
        "agent",
        "555555555555555555",
        &["222222222222222222".into()],
        &[],
        "bot-token",
    )
    .unwrap();
    crate::admin_settings::store_create(&store, A, &plan, &KEY).unwrap();
    let generation = store.get(A).unwrap().unwrap().desired_generation;
    store.mark_provisioning(A, generation).unwrap();
    store.mark_verified(A, generation, true).unwrap();
    let row = store.get(A).unwrap().unwrap();
    factory
        .for_instance(
            &row,
            zeroize::Zeroizing::new(b"bot-token".to_vec()),
            "nonce",
        )
        .unwrap();
    let place = crate::config::Placement::load(&placement_dir.join(format!("{A}.json")))
        .expect("the child accepts a placement with no channel");
    assert!(place.instances[0].addresses.is_empty());
    assert!(place.channel_socket.is_some());
}

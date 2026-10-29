//! D-1097 D1/D2: a settings change rebuilds only the changed instance's child, and core keeps
//! seeing the same neutral config (no core revision for a Discord-only change).

use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::collections::BTreeMap;

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [9; 32];

#[derive(Default)]
struct RecordingGate {
    configs: Mutex<Vec<String>>,
}

#[async_trait]
impl GateReconciler for RecordingGate {
    async fn observe(
        &self,
        _: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Mismatch)
    }

    async fn reconcile(&self, desired: &InstanceRow, _: Option<&str>) -> Result<VerifiedInstance> {
        let core = core_config_b64_for_runtime_config(&desired.config_b64)?;
        self.configs.lock().unwrap().push(core);
        Ok(VerifiedInstance {
            revision: 1,
            digest: "neutral".into(),
            bindings: desired.addresses.clone(),
            enabled: desired.enabled,
        })
    }
}

#[derive(Default)]
struct CountingFactory {
    starts: Mutex<BTreeMap<String, usize>>,
}

struct Child;
#[async_trait]
impl SupervisedChild for Child {
    async fn wait_exit(&mut self) -> String {
        std::future::pending::<()>().await;
        String::new()
    }
    async fn kill(&mut self) {}
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct Spawner(String);
#[async_trait]
impl ChildSpawner for Spawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        Ok(Box::new(Child))
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
        Ok(Arc::new(Spawner(row.instance_id.clone())))
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
async fn d1097_a_settings_change_rebuilds_only_that_child_with_the_same_core_config() {
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
                7,
                &cfg,
                &[format!("discord-{agent}-1-2")],
                "t",
                None,
                true,
                &KEY,
            )
            .unwrap();
    }
    let gate = Arc::new(RecordingGate::default());
    let factory = Arc::new(CountingFactory::default());
    let daemon = DiscordDaemon::new(store, KEY, gate.clone(), factory.clone()).unwrap();
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
    let configs = gate.configs.lock().unwrap().clone();
    assert_eq!(configs.len(), 3);
    assert!(
        configs.iter().all(|c| c == DEFAULT_CORE_CONFIG_B64),
        "core only ever sees the neutral config: {configs:?}"
    );
    daemon.shutdown().await;
}

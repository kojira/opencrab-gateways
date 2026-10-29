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

/// A minimal gate-admin over a Unix socket: keeps one instance and answers the calls
/// `GateAdminClient::reconcile` makes. Records every `config_b64` it receives.
async fn fake_gate_admin(socket: PathBuf, received: Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let state: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0_u8; 4096];
        let (head, body) = loop {
            let n = stream.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..split]).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while raw.len() < split + 4 + length {
                    let n = stream.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..n]);
                }
                break (head, raw[split + 4..split + 4 + length].to_vec());
            }
        };
        let line = head.lines().next().unwrap_or_default().to_string();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let (status, reply) = {
            let mut current = state.lock().unwrap();
            if line.starts_with("GET ") {
                match current.as_ref() {
                    Some(value) => (200, value.clone()),
                    None => (404, serde_json::json!({"code":"instance_unknown"})),
                }
            } else if line.starts_with("PUT /api/gate-instances/") {
                let config = body["config_b64"].as_str().unwrap().to_string();
                received.lock().unwrap().push(config.clone());
                let value = serde_json::json!({"instance_id":A,"kind_id":"discord","subject_id":7,
                "revision":1,"enabled":true,"config_b64":config,"config_digest":"d","bindings":[]});
                *current = Some(value.clone());
                (201, value)
            } else if line.starts_with("PUT /api/gate-bindings/") {
                let binding = serde_json::json!({"binding_id": line.split('/').nth(3).unwrap()
                .split(' ').next().unwrap(), "address": body["address"], "session_id": null});
                current.as_mut().unwrap()["bindings"]
                    .as_array_mut()
                    .unwrap()
                    .push(binding.clone());
                (201, binding)
            } else if line.starts_with("POST ") {
                let config = body["config_b64"].as_str().unwrap().to_string();
                received.lock().unwrap().push(config.clone());
                let value = current.as_mut().unwrap();
                value["config_b64"] = serde_json::json!(config);
                value["revision"] = serde_json::json!(value["revision"].as_u64().unwrap() + 1);
                (201, value.clone())
            } else {
                (200, serde_json::json!({}))
            }
        };
        let text = reply.to_string();
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}

/// D-1097 D2 through the real reconciler: core receives the neutral config, and a Discord-only
/// change makes no core revision.
#[tokio::test]
async fn d1097_uds_reconciler_sends_only_the_neutral_config_to_core() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("gate-admin.sock");
    let received = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(fake_gate_admin(socket.clone(), received.clone()));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let reconciler = UdsGateReconciler::new(
        opencrab_gate_client::admin::GateAdminClient::new_for_test(socket, "t".into()),
        "discord",
    );
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            A,
            "agent",
            7,
            &config(&[]),
            &["discord-agent-1-2".into()],
            "t",
            None,
            true,
            &KEY,
        )
        .unwrap();
    let first = reconciler
        .reconcile(&store.get(A).unwrap().unwrap(), None)
        .await
        .unwrap();
    store
        .upsert_desired(
            A,
            "agent",
            7,
            &config(&["222222222222222222"]),
            &["discord-agent-1-2".into()],
            "",
            None,
            true,
            &KEY,
        )
        .unwrap();
    let second = reconciler
        .reconcile(&store.get(A).unwrap().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(
        received.lock().unwrap().clone(),
        vec![DEFAULT_CORE_CONFIG_B64.to_string()],
        "only the first call creates the instance; the owner change sends nothing new"
    );
    assert_eq!(
        first.revision, second.revision,
        "no core revision for a Discord-only change"
    );
    server.abort();
}

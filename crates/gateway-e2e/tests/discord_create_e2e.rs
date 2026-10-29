//! D-1097: a new Discord bot for an agent that has none, created against a real core.
//!
//! Real pieces: core's extgate state, its gate-admin router served on a mode-0600 Unix socket,
//! a creation-namespace principal over the agent's subject, the Discord daemon's admin socket
//! `create` op, the real subject-grant issuer and the real gate-admin reconciler. Only the child
//! process is a stand-in (no Discord connection in a test).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencrab_discord_gateway::admin::{spawn_with, AdminContext};
use opencrab_discord_gateway::daemon::{
    DiscordDaemon, SpawnerFactory, UdsGateReconciler, DEFAULT_CORE_CONFIG_B64,
};
use opencrab_discord_gateway::store::{DiscordStore, InstanceRow};
use opencrab_discord_gateway::subject_grant::{instance_id_for, GateAdminGrants};
use opencrab_extgate::gate_admin_security::{
    issue_principal, Operation, PrincipalRequest, PrincipalScope,
};
use opencrab_extgate::{admin_router, ExtgateState};
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

const AGENT: &str = "agent-new-bot";
const NAMESPACE: &str = "0c9a7e51-2b3d-4f6a-8e1c-5d7b9f2a4c63";
const KEY: [u8; 32] = [7; 32];

struct Child;
#[async_trait::async_trait]
impl SupervisedChild for Child {
    async fn wait_exit(&mut self) -> String {
        std::future::pending::<()>().await;
        String::new()
    }
    async fn kill(&mut self) {}
    fn pid(&self) -> Option<u32> {
        None
    }
}

struct Spawner(String);
#[async_trait::async_trait]
impl ChildSpawner for Spawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        Ok(Box::new(Child))
    }
    fn target_id(&self) -> &str {
        &self.0
    }
}

#[derive(Default)]
struct Started(Mutex<Vec<String>>);
impl SpawnerFactory for Started {
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> anyhow::Result<Arc<dyn ChildSpawner>> {
        assert_eq!(&*credential, b"bot-token-for-the-new-bot");
        self.0.lock().unwrap().push(row.instance_id.clone());
        Ok(Arc::new(Spawner(row.instance_id.clone())))
    }
}

fn agent(db: &opencrab_db::Db) -> i64 {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT.into(),
            name: "New".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    conn.query_row(
        "SELECT subject_id FROM agents WHERE agent_id=?1",
        [AGENT],
        |r| r.get(0),
    )
    .unwrap()
}

async fn call(socket: &std::path::Path, request: serde_json::Value) -> serde_json::Value {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn d1097_a_new_bot_is_created_in_real_core_and_starts() {
    let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
    let subject_id = agent(&db);
    let extgate = Arc::new(ExtgateState::new_protected(db.clone()));

    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = dir.path().canonicalize().unwrap();
    let gate_admin = root.join("gate-admin.sock");
    let prepared = opencrab_extgate::admin_socket::prepare_admin_socket(&gate_admin, unsafe {
        libc::geteuid()
    })
    .unwrap();
    let (listener, _cleanup) = prepared.into_parts();
    let router = admin_router(extgate.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    let namespace = uuid::Uuid::parse_str(NAMESPACE).unwrap();
    let bearer = {
        let now = opencrab_extgate::now_nanos();
        let mut conn = extgate.db.lock().unwrap();
        issue_principal(
            &mut conn,
            &PrincipalRequest {
                principal_id: "discord-daemon".into(),
                operations: [
                    Operation::InstanceRead,
                    Operation::InstancePut,
                    Operation::InstanceRevise,
                    Operation::BindingPut,
                    Operation::BindingDelete,
                ]
                .into_iter()
                .collect(),
                subject_ids: [subject_id].into_iter().collect(),
                scope: PrincipalScope::CreationNamespace(namespace),
                expires_at: now + 86_400 * 1_000_000_000,
            },
            now,
        )
        .unwrap()
    };
    let credential = root.join("gate-admin.token");
    std::fs::write(&credential, bearer.expose_secret()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let store = DiscordStore::open(&root.join("discord.db")).unwrap();
    let factory = Arc::new(Started::default());
    let daemon = Arc::new(
        DiscordDaemon::new(
            store,
            KEY,
            Arc::new(UdsGateReconciler::new(
                opencrab_gate_client::admin::GateAdminClient::from_credential_file(
                    gate_admin.clone(),
                    &credential,
                )
                .unwrap(),
                "discord",
            )),
            factory.clone(),
        )
        .unwrap(),
    );
    let admin_socket = root.join("discord-admin.sock");
    let admin = spawn_with(
        admin_socket.clone(),
        AdminContext {
            scope: Arc::new(Default::default()),
            creation_namespace: Some(namespace),
            grants: Some(Arc::new(
                GateAdminGrants::from_credential_file(gate_admin.clone(), &credential).unwrap(),
            )),
        },
        daemon.store(),
        Arc::new(KEY),
    );
    for _ in 0..200 {
        if admin_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let address = format!("discord-{AGENT}-500-600");
    let created = call(
        &admin_socket,
        serde_json::json!({"id":"c","op":"create","agent_id":AGENT,
            "credential":"bot-token-for-the-new-bot","self_bot_id":"555555555555555555",
            "owners":["222222222222222222"],"addresses":[address]}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let instance_id = instance_id_for(&namespace, AGENT).to_string();
    assert_eq!(created["result"]["instance_id"], instance_id);

    daemon.reconcile_once().await.unwrap();

    // Core now has the instance, with only the neutral config, and the channel's binding.
    let (kind, subject, config_b64, enabled): (String, i64, String, bool) = {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT kind_id, subject_id, config_b64, enabled FROM gate_instances WHERE instance_id=?1",
            [&instance_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(kind, "discord");
    assert_eq!(subject, subject_id);
    assert_eq!(config_b64, DEFAULT_CORE_CONFIG_B64);
    assert!(enabled);
    let bound: Vec<String> = {
        let conn = db.lock().unwrap();
        let mut statement = conn
            .prepare("SELECT address FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL")
            .unwrap();
        statement
            .query_map([&instance_id], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(bound, vec![address]);
    assert_eq!(
        factory.0.lock().unwrap().clone(),
        vec![instance_id.clone()],
        "the bot starts"
    );
    let row = daemon
        .store()
        .lock()
        .unwrap()
        .get(&instance_id)
        .unwrap()
        .unwrap();
    assert!(
        row.subject_grant_envelope.is_none(),
        "the used grant is not kept"
    );

    // The grant was single use: a second bot for the same agent is refused.
    let again = call(
        &admin_socket,
        serde_json::json!({"id":"c2","op":"create","agent_id":AGENT,
            "credential":"x","self_bot_id":"555555555555555555",
            "owners":["222222222222222222"],"addresses":[]}),
    )
    .await;
    assert_eq!(again["ok"], false, "{again}");
    admin.abort();
    daemon.shutdown().await;
}

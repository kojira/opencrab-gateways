//! D-1097: the Discord daemon against a real core with BOTH gate-admin principals, as in
//! production: an exact-scope principal over the instances that already exist (random ids,
//! imported from the instance-mode placements), and a creation-namespace principal for bots the
//! settings page creates.
//!
//! Real pieces: core's extgate state and gate-admin router on a mode-0600 Unix socket, both
//! principals, the one-time import, the routed gate-admin reconciler, the subject-grant issuer
//! and the daemon admin socket. Only the child process is a stand-in (no Discord in a test).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use opencrab_discord_gateway::admin::{spawn_with, AdminContext};
use opencrab_discord_gateway::daemon::{
    DiscordDaemon, SpawnerFactory, UdsGateReconciler, DEFAULT_CORE_CONFIG_B64,
};
use opencrab_discord_gateway::gate_routing::GateRouting;
use opencrab_discord_gateway::legacy_import::{import_legacy, ImportOutcome, LegacyImport};
use opencrab_discord_gateway::store::{DiscordStore, InstanceRow};
use opencrab_discord_gateway::subject_grant::{instance_id_for, GateAdminGrants};
use opencrab_extgate::gate_admin_security::{
    issue_principal, Operation, PrincipalRequest, PrincipalScope,
};
use opencrab_extgate::{admin_router, ExtgateState};
use opencrab_gate_client::admin::{DesiredInstance, GateAdminClient};
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

const NAMESPACE: &str = "0c9a7e51-2b3d-4f6a-8e1c-5d7b9f2a4c63";
const KEY: [u8; 32] = [7; 32];
const LEGACY: [&str; 3] = ["agent-kairo", "agent-nostaro", "agent-labomi"];
const NEW_AGENT: &str = "agent-new-bot";

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

/// Records which instance started with which token.
#[derive(Default)]
struct Started(Mutex<Vec<(String, String)>>);
impl SpawnerFactory for Started {
    fn for_instance(
        &self,
        row: &InstanceRow,
        credential: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> anyhow::Result<Arc<dyn ChildSpawner>> {
        self.0.lock().unwrap().push((
            row.instance_id.clone(),
            String::from_utf8(credential.to_vec()).unwrap(),
        ));
        Ok(Arc::new(Spawner(row.instance_id.clone())))
    }
}

fn add_agent(db: &opencrab_db::Db, agent_id: &str) -> i64 {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: agent_id.into(),
            name: agent_id.into(),
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
        [agent_id],
        |r| r.get(0),
    )
    .unwrap()
}

fn secret_file(path: &std::path::Path, value: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn principal(extgate: &ExtgateState, id: &str, subjects: &[i64], scope: PrincipalScope) -> String {
    let now = opencrab_extgate::now_nanos();
    let mut conn = extgate.db.lock().unwrap();
    issue_principal(
        &mut conn,
        &PrincipalRequest {
            principal_id: id.into(),
            operations: [
                Operation::InstanceRead,
                Operation::InstancePut,
                Operation::InstanceRevise,
                Operation::BindingPut,
                Operation::BindingDelete,
            ]
            .into_iter()
            .collect(),
            subject_ids: subjects.iter().copied().collect(),
            scope,
            expires_at: now + 86_400 * 1_000_000_000,
        },
        now,
    )
    .unwrap()
    .expose_secret()
    .to_string()
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

fn core_instance(db: &opencrab_db::Db, instance_id: &str) -> Option<(i64, String, i64)> {
    let conn = db.lock().unwrap();
    conn.query_row(
        "SELECT subject_id, config_b64, revision FROM gate_instances WHERE instance_id=?1",
        [instance_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

fn bound(db: &opencrab_db::Db, instance_id: &str) -> Vec<String> {
    let conn = db.lock().unwrap();
    let mut statement = conn
        .prepare("SELECT address FROM gate_bindings WHERE instance_id=?1 AND closed_at IS NULL ORDER BY address")
        .unwrap();
    statement
        .query_map([instance_id], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Reconciles until no instance is pending or in error.
///
/// Core's revision route answers `{instance_id, revision, enabled, config_digest}`, while
/// gate-client parses it as a full instance, so a revision reports an error after core applied
/// it. The next reconcile finds the config already in place and converges, which is how
/// production recovers too. Tracked separately as a core issue.
async fn converge<R, F>(daemon: &DiscordDaemon<R, F>)
where
    R: opencrab_discord_gateway::daemon::GateReconciler + 'static,
    F: SpawnerFactory + 'static,
{
    use opencrab_process_supervisor::lifecycle::LifecycleState;
    for _ in 0..40 {
        daemon.reconcile_once().await.unwrap();
        let settled = daemon
            .store()
            .lock()
            .unwrap()
            .list()
            .unwrap()
            .iter()
            .all(|row| {
                !matches!(
                    row.lifecycle_state,
                    LifecycleState::Pending | LifecycleState::Error | LifecycleState::Provisioning
                )
            });
        if settled {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the daemon did not converge");
}

#[tokio::test]
async fn d1097_imported_exact_instances_keep_working_and_a_new_bot_uses_the_namespace() {
    let db = opencrab_db::Db::from_connection(opencrab_db::init_memory().unwrap());
    let extgate = Arc::new(ExtgateState::new_protected(db.clone()));
    let dir = tempfile::Builder::new()
        .prefix("dc")
        .tempdir_in("/tmp")
        .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = dir.path().canonicalize().unwrap();
    let gate_admin = root.join("ga.sock");
    let (listener, _cleanup) =
        opencrab_extgate::admin_socket::prepare_admin_socket(&gate_admin, unsafe {
            libc::geteuid()
        })
        .unwrap()
        .into_parts();
    let router = admin_router(extgate.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    // Production today: 3 Discord instances with random ids and their instance-mode placements.
    let subjects: Vec<i64> = LEGACY.iter().map(|agent| add_agent(&db, agent)).collect();
    let new_subject = add_agent(&db, NEW_AGENT);
    let legacy_ids: Vec<String> = LEGACY
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let exact_bearer = principal(
        &extgate,
        "discord-exact",
        &subjects,
        PrincipalScope::Exact(
            legacy_ids
                .iter()
                .map(|id| uuid::Uuid::parse_str(id).unwrap())
                .collect(),
        ),
    );
    let namespace = uuid::Uuid::parse_str(NAMESPACE).unwrap();
    let creation_bearer = principal(
        &extgate,
        "discord-creation",
        &[subjects.clone(), vec![new_subject]].concat(),
        PrincipalScope::CreationNamespace(namespace),
    );
    let exact_cred = root.join("exact.token");
    let creation_cred = root.join("creation.token");
    secret_file(&exact_cred, &exact_bearer);
    secret_file(&creation_cred, &creation_bearer);
    let exact = GateAdminClient::from_credential_file(gate_admin.clone(), &exact_cred).unwrap();

    let tokens = root.join("tokens");
    std::fs::create_dir(&tokens).unwrap();
    let mut placements = Vec::new();
    for ((agent, id), subject) in LEGACY.iter().zip(&legacy_ids).zip(&subjects) {
        let config = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({"agent_id":agent,
                "self_bot_id":"111111111111111111","access":{"owners":["222222222222222222"]}}))
            .unwrap(),
        );
        // The instance-mode setup created these in core with the full Discord config.
        let grant = {
            let mut conn = db.lock().unwrap();
            opencrab_db::queries::issue_subject_association_grant(
                &mut conn,
                agent,
                *subject,
                i64::MAX,
                100,
            )
            .unwrap()
        };
        exact
            .put_instance(
                id,
                &DesiredInstance {
                    kind_id: "discord",
                    subject_id: *subject,
                    enabled: true,
                    config_b64: &config,
                    subject_grant: Some(&grant),
                },
            )
            .await
            .unwrap();
        let address = format!("discord-{agent}-500-600");
        let path = root.join(format!("{agent}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"core_socket":"/tmp/core.sock",
                "instances":[{"instance_id":id,"revision":1,"addresses":[address],"config_b64":config}]}))
            .unwrap(),
        )
        .unwrap();
        secret_file(
            &tokens.join(format!("{agent}.token")),
            &format!("token-{agent}"),
        );
        placements.push(path);
    }

    // The daemon: import with the exact credential, then reconcile with both.
    let store = DiscordStore::open(&root.join("discord.db")).unwrap();
    let outcome = import_legacy(
        &store,
        &LegacyImport {
            placements,
            token_dir: tokens,
        },
        &exact,
        &KEY,
    )
    .await
    .unwrap();
    assert_eq!(outcome, ImportOutcome::Imported(3));
    let routing = GateRouting {
        exact: exact.clone(),
        creation: Some((
            namespace,
            GateAdminClient::from_credential_file(gate_admin.clone(), &creation_cred).unwrap(),
        )),
    };
    let factory = Arc::new(Started::default());
    let daemon = Arc::new(
        DiscordDaemon::new(
            store,
            KEY,
            Arc::new(UdsGateReconciler::routed(routing, "discord")),
            factory.clone(),
        )
        .unwrap(),
    );
    let admin_socket = root.join("admin.sock");
    let admin = spawn_with(
        admin_socket.clone(),
        AdminContext {
            scope: Arc::new(legacy_ids.iter().cloned().collect()),
            creation_namespace: Some(namespace),
            grants: Some(Arc::new(
                GateAdminGrants::from_credential_file(gate_admin.clone(), &creation_cred).unwrap(),
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
    converge(&daemon).await;

    // The 3 imported bots: revised to the neutral config through the exact principal, bound, started.
    for ((agent, id), subject) in LEGACY.iter().zip(&legacy_ids).zip(&subjects) {
        let (core_subject, config, revision) = core_instance(&db, id).unwrap();
        assert_eq!(core_subject, *subject);
        assert_eq!(config, DEFAULT_CORE_CONFIG_B64, "{agent}");
        assert_eq!(revision, 2, "{agent}: one revision to the neutral config");
        assert_eq!(bound(&db, id), vec![format!("discord-{agent}-500-600")]);
    }
    let started = factory.0.lock().unwrap().clone();
    for (agent, id) in LEGACY.iter().zip(&legacy_ids) {
        assert!(
            started.contains(&(id.clone(), format!("token-{agent}"))),
            "{agent} started"
        );
    }
    let listed = call(&admin_socket, serde_json::json!({"id":"l","op":"list"})).await;
    assert_eq!(listed["result"]["instances"].as_array().unwrap().len(), 3);

    // A settings edit on an imported bot goes through the exact principal and rebuilds it.
    let response = call(
        &admin_socket,
        serde_json::json!({"id":"o","op":"set_enabled","scope_instance_id":legacy_ids[0],
            "instance_id":legacy_ids[0],"enabled":false}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    daemon.reconcile_once().await.unwrap();
    assert_eq!(
        core_instance(&db, &legacy_ids[0]).unwrap().2,
        3,
        "revised to disabled"
    );

    // A new agent's bot through the namespace principal, with no channel yet.
    let created = call(
        &admin_socket,
        serde_json::json!({"id":"c","op":"create","agent_id":NEW_AGENT,
            "credential":"token-new","self_bot_id":"555555555555555555",
            "owners":["222222222222222222"],"addresses":[]}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let new_id = instance_id_for(&namespace, NEW_AGENT).to_string();
    converge(&daemon).await;
    let (core_subject, config, _) = core_instance(&db, &new_id).expect("created in core");
    assert_eq!(core_subject, new_subject);
    assert_eq!(config, DEFAULT_CORE_CONFIG_B64);
    assert!(
        bound(&db, &new_id).is_empty(),
        "no channel until the owner enables one"
    );
    assert!(
        factory
            .0
            .lock()
            .unwrap()
            .contains(&(new_id.clone(), "token-new".into())),
        "the new bot starts"
    );
    let row = daemon
        .store()
        .lock()
        .unwrap()
        .get(&new_id)
        .unwrap()
        .unwrap();
    assert!(
        row.subject_grant_envelope.is_none(),
        "the used grant is not kept"
    );

    // A second bot for the same agent is refused.
    let again = call(
        &admin_socket,
        serde_json::json!({"id":"c2","op":"create","agent_id":NEW_AGENT,
            "credential":"x","self_bot_id":"555555555555555555",
            "owners":["222222222222222222"],"addresses":[]}),
    )
    .await;
    assert_eq!(again["ok"], false, "{again}");
    admin.abort();
    daemon.shutdown().await;
}

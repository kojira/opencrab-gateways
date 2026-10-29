//! D-1097: creating a bot for an agent that has none, through the admin socket.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const NS: &str = "12345678-1234-4234-8234-123456789abc";
const KEY: [u8; 32] = [6; 32];

struct FakeIssuer {
    calls: AtomicUsize,
    fail: bool,
}

#[async_trait::async_trait]
impl crate::subject_grant::SubjectGrantIssuer for FakeIssuer {
    async fn issue(&self, agent_id: &str) -> Result<crate::subject_grant::SubjectGrant> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail, "core refused: unauthorized");
        Ok(crate::subject_grant::SubjectGrant {
            subject_id: if agent_id == "agent-new" { 11 } else { 12 },
            grant: zeroize::Zeroizing::new(format!("grant-for-{agent_id}")),
        })
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Mutex<DiscordStore>>,
    issuer: Arc<FakeIssuer>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(fail: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = Arc::new(Mutex::new(
        DiscordStore::open(&temp.path().join("owner.db")).unwrap(),
    ));
    let issuer = Arc::new(FakeIssuer {
        calls: AtomicUsize::new(0),
        fail,
    });
    let context = AdminContext {
        scope: Arc::new(std::collections::BTreeSet::new()),
        creation_namespace: Some(uuid::Uuid::parse_str(NS).unwrap()),
        grants: Some(issuer.clone()),
    };
    let task = spawn_with(socket.clone(), context, store.clone(), Arc::new(KEY));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    Fixture {
        _temp: temp,
        socket,
        store,
        issuer,
        task,
    }
}

async fn call(socket: &Path, request: serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

fn create(agent: &str) -> serde_json::Value {
    json!({"id":"c","op":"create","agent_id":agent,"credential":"bot-token-secret",
        "self_bot_id":"555555555555555555","owners":["222222222222222222"],
        "addresses":[format!("discord-{agent}-100-200")]})
}

#[tokio::test]
async fn d1097_create_stores_a_pending_instance_with_a_core_grant() {
    let f = fixture(false).await;
    let response = call(&f.socket, create("agent-new")).await;
    assert_eq!(response["ok"], true, "{response}");
    let expected =
        crate::subject_grant::instance_id_for(&uuid::Uuid::parse_str(NS).unwrap(), "agent-new");
    assert_eq!(response["result"]["instance_id"], expected.to_string());
    assert!(!response.to_string().contains("bot-token-secret"));
    assert!(!response.to_string().contains("grant-for"));

    let id = expected.to_string();
    let row = f.store.lock().unwrap().get(&id).unwrap().unwrap();
    assert_eq!(row.agent_id, "agent-new");
    assert_eq!(row.subject_id, 11);
    assert!(row.enabled);
    assert_eq!(row.addresses, vec!["discord-agent-new-100-200"]);
    assert_eq!(
        row.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Pending
    );
    let cfg = crate::config::parse_instance_config(
        &crate::config::decode_config_b64(&row.config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg.agent_id, "agent-new");
    assert_eq!(cfg.self_bot_id, "555555555555555555");
    assert_eq!(cfg.access.owners, vec!["222222222222222222"]);
    let store = f.store.lock().unwrap();
    assert_eq!(
        &*store.decrypt_credential(&id, &KEY).unwrap(),
        b"bot-token-secret"
    );
    assert_eq!(
        &**store.decrypt_subject_grant(&id, &KEY).unwrap().unwrap(),
        b"grant-for-agent-new"
    );
    drop(store);

    // The new instance is now in scope for the settings ops.
    let response = call(&f.socket, json!({"id":"l","op":"list"})).await;
    let items = response["result"]["instances"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let response = call(
        &f.socket,
        json!({"id":"e","op":"set_enabled","scope_instance_id":id,"instance_id":id,"enabled":false}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
}

#[tokio::test]
async fn d1097_create_refuses_bad_input_a_second_bot_and_a_core_refusal() {
    let f = fixture(false).await;
    for (label, mut request) in [
        ("no owner", create("agent-new")),
        ("bad bot id", create("agent-new")),
        ("foreign address", create("agent-new")),
        ("no token", create("agent-new")),
    ] {
        match label {
            "no owner" => request["owners"] = json!([]),
            "bad bot id" => request["self_bot_id"] = json!("not-a-snowflake"),
            "foreign address" => request["addresses"] = json!(["discord-someone-else-1-2"]),
            _ => request["credential"] = json!(""),
        }
        let response = call(&f.socket, request).await;
        assert_eq!(response["ok"], false, "{label}: {response}");
    }
    assert_eq!(
        f.issuer.calls.load(Ordering::SeqCst),
        0,
        "input is checked before core"
    );

    assert_eq!(call(&f.socket, create("agent-new")).await["ok"], true);
    let again = call(&f.socket, create("agent-new")).await;
    assert_eq!(again["ok"], false, "one bot per agent: {again}");
    assert_eq!(f.store.lock().unwrap().list().unwrap().len(), 1);

    let refused = fixture(true).await;
    let response = call(&refused.socket, create("agent-new")).await;
    assert_eq!(response["ok"], false);
    assert!(refused.store.lock().unwrap().list().unwrap().is_empty());
}

#[tokio::test]
async fn d1097_create_is_unavailable_without_a_creation_namespace() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = Arc::new(Mutex::new(
        DiscordStore::open(&temp.path().join("owner.db")).unwrap(),
    ));
    let task = spawn(
        socket.clone(),
        Arc::new(std::collections::BTreeSet::new()),
        store.clone(),
        Arc::new(KEY),
    );
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let response = call(&socket, create("agent-new")).await;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error"], "create_unavailable");
    task.abort();
}

/// The instance id must equal core's rule for creation-namespace principals
/// (`namespace_instance_id`: uuid5(ns, "instance\0" + agent_id)), or core refuses it.
#[test]
fn d1097_instance_id_matches_core_namespace_rule() {
    let ns = uuid::Uuid::parse_str(NS).unwrap();
    assert_eq!(
        crate::subject_grant::instance_id_for(&ns, "agent-x"),
        uuid::Uuid::new_v5(&ns, b"instance\0agent-x")
    );
    assert_ne!(
        crate::subject_grant::instance_id_for(&ns, "agent-x"),
        crate::subject_grant::instance_id_for(&ns, "agent-y")
    );
}

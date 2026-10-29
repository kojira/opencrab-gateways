//! D-1098: creating a bot for an agent that has none, through the admin socket. No credential,
//! no grant: the daemon gives it a fresh instance id and provisions it on its next reconcile.

use super::*;

const KEY: [u8; 32] = [6; 32];

struct Fixture {
    _temp: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Mutex<DiscordStore>>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = Arc::new(Mutex::new(
        DiscordStore::open(&temp.path().join("owner.db")).unwrap(),
    ));
    let task = spawn(socket.clone(), store.clone(), Arc::new(KEY));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    Fixture {
        _temp: temp,
        socket,
        store,
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
async fn d1098_create_stores_a_pending_instance_under_a_fresh_id() {
    let f = fixture().await;
    let response = call(&f.socket, create("agent-new")).await;
    assert_eq!(response["ok"], true, "{response}");
    assert!(!response.to_string().contains("bot-token-secret"));
    let id = response["result"]["instance_id"].as_str().unwrap().to_string();
    let parsed = uuid::Uuid::parse_str(&id).unwrap();
    assert_eq!(parsed.get_version_num(), 4, "a fresh random id");

    let row = f.store.lock().unwrap().get(&id).unwrap().unwrap();
    assert_eq!(row.agent_id, "agent-new");
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
    assert_eq!(cfg.self_bot_id, "555555555555555555");
    assert_eq!(cfg.access.owners, vec!["222222222222222222"]);
    assert_eq!(
        &*f.store.lock().unwrap().decrypt_credential(&id, &KEY).unwrap(),
        b"bot-token-secret"
    );

    // The new instance is managed like any other.
    let listed = call(&f.socket, json!({"id":"l","op":"list"})).await;
    assert_eq!(listed["result"]["instances"].as_array().unwrap().len(), 1);
    let off = call(
        &f.socket,
        json!({"id":"e","op":"set_enabled","instance_id":id,"enabled":false}),
    )
    .await;
    assert_eq!(off["ok"], true, "{off}");
}

#[tokio::test]
async fn d1098_create_refuses_bad_input_and_a_second_bot_for_the_agent() {
    let f = fixture().await;
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
    assert!(f.store.lock().unwrap().list().unwrap().is_empty());
    assert_eq!(call(&f.socket, create("agent-new")).await["ok"], true);
    let again = call(&f.socket, create("agent-new")).await;
    assert_eq!(again["ok"], false, "one bot per agent: {again}");
    assert_eq!(f.store.lock().unwrap().list().unwrap().len(), 1);
}

#[tokio::test]
async fn d1098_an_unknown_instance_is_refused() {
    let f = fixture().await;
    let response = call(
        &f.socket,
        json!({"id":"x","op":"set_enabled","instance_id":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","enabled":false}),
    )
    .await;
    assert_eq!(response["ok"], false, "{response}");
}

//! D-1097 D1: the settings operations on the Discord admin socket.

use super::*;
use base64::Engine as _;
use std::os::unix::fs::PermissionsExt as _;

const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [4; 32];

fn config(value: serde_json::Value) -> String {
    base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap())
}

fn base_config() -> serde_json::Value {
    json!({"agent_id":"agent","self_bot_id":"111111111111111111","name":"crab"})
}

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
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    store
        .upsert_desired(
            ID,
            "agent",
            7,
            &config(base_config()),
            &["discord-agent-1-2".into()],
            "token-secret",
            None,
            true,
            &KEY,
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let scope = Arc::new(std::collections::BTreeSet::from([ID.to_string()]));
    let task = spawn(socket.clone(), scope, store.clone(), Arc::new(KEY));
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

fn generation(store: &Mutex<DiscordStore>) -> u64 {
    store
        .lock()
        .unwrap()
        .get(ID)
        .unwrap()
        .unwrap()
        .desired_generation
}

#[tokio::test]
async fn d1097_list_returns_scoped_instances_without_secrets() {
    let f = fixture().await;
    assert_eq!(
        std::fs::metadata(&f.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let response = call(&f.socket, json!({"id":"1","op":"list"})).await;
    assert_eq!(response["ok"], true, "{response}");
    let items = response["result"]["instances"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["instance_id"], ID);
    assert_eq!(item["agent_id"], "agent");
    assert_eq!(item["enabled"], true);
    assert_eq!(item["credential_configured"], true);
    assert_eq!(item["addresses"], json!(["discord-agent-1-2"]));
    assert_eq!(item["config"]["name"], "crab");
    assert_eq!(item["config"]["system_reactions"]["accepted"], "👀");
    let text = response.to_string();
    assert!(
        !text.contains("token-secret") && !text.contains("enc:v1:"),
        "{text}"
    );
}

#[tokio::test]
async fn d1097_set_config_validates_bumps_generation_and_keeps_the_credential() {
    let f = fixture().await;
    let before = generation(&f.store);
    let mut wanted = base_config();
    wanted["access"] = json!({"owners":["222222222222222222"],
        "trusted_users":["333333333333333333"],
        "co_agents":{"444444444444444444":"other-agent"}});
    wanted["delivery_mode"] = json!("say");
    wanted["system_reactions"] = json!({"accepted":"✅"});
    let response = call(
        &f.socket,
        json!({"id":"2","op":"set_config","scope_instance_id":ID,"instance_id":ID,"config":wanted}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(generation(&f.store), before + 1);
    let row = f.store.lock().unwrap().get(ID).unwrap().unwrap();
    assert_eq!(
        row.lifecycle_state,
        opencrab_process_supervisor::lifecycle::LifecycleState::Pending
    );
    let saved = crate::config::parse_instance_config(
        &crate::config::decode_config_b64(&row.config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.access.owners, vec!["222222222222222222"]);
    assert_eq!(saved.access.co_agents["444444444444444444"], "other-agent");
    assert_eq!(saved.delivery_mode.as_deref(), Some("say"));
    assert_eq!(saved.system_reactions.accepted, "✅");
    assert_eq!(saved.system_reactions.completed, "🏁");
    assert_eq!(
        &*f.store
            .lock()
            .unwrap()
            .decrypt_credential(ID, &KEY)
            .unwrap(),
        b"token-secret"
    );

    for invalid in [
        json!({"agent_id":"agent","self_bot_id":"111111111111111111","access":{"owners":["not-a-snowflake"]}}),
        json!({"agent_id":"agent","self_bot_id":"111111111111111111","token":"leak"}),
        json!({"agent_id":"renamed","self_bot_id":"111111111111111111"}),
    ] {
        let response = call(
            &f.socket,
            json!({"id":"3","op":"set_config","scope_instance_id":ID,"instance_id":ID,"config":invalid}),
        )
        .await;
        assert_eq!(response["ok"], false, "{response}");
        assert!(!response.to_string().contains("leak"));
    }
    assert_eq!(
        generation(&f.store),
        before + 1,
        "rejected changes do not apply"
    );
}

#[tokio::test]
async fn d1097_set_addresses_credential_and_enabled() {
    let f = fixture().await;
    let response = call(
        &f.socket,
        json!({"id":"4","op":"set_addresses","scope_instance_id":ID,"instance_id":ID,
            "addresses":["discord-agent-1-2","discord-agent-1-3"]}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(
        f.store.lock().unwrap().get(ID).unwrap().unwrap().addresses,
        vec!["discord-agent-1-2", "discord-agent-1-3"]
    );
    for bad in [
        json!([]),
        json!(["discord-other-1-2"]),
        json!(["discord-agent-x-2"]),
    ] {
        let response = call(
            &f.socket,
            json!({"id":"5","op":"set_addresses","scope_instance_id":ID,"instance_id":ID,"addresses":bad}),
        )
        .await;
        assert_eq!(response["ok"], false, "{bad}: {response}");
    }

    let response = call(
        &f.socket,
        json!({"id":"6","op":"set_credential","scope_instance_id":ID,"instance_id":ID,
            "credential":"replacement-secret"}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    assert!(!response.to_string().contains("replacement-secret"));
    assert_eq!(
        &*f.store
            .lock()
            .unwrap()
            .decrypt_credential(ID, &KEY)
            .unwrap(),
        b"replacement-secret"
    );
    let response = call(
        &f.socket,
        json!({"id":"7","op":"set_credential","scope_instance_id":ID,"instance_id":ID,"credential":""}),
    )
    .await;
    assert_eq!(response["ok"], false, "an empty token is not a token");

    let response = call(
        &f.socket,
        json!({"id":"8","op":"set_enabled","scope_instance_id":ID,"instance_id":ID,"enabled":false}),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    assert!(!f.store.lock().unwrap().get(ID).unwrap().unwrap().enabled);
}

#[tokio::test]
async fn d1097_settings_operations_are_scoped() {
    let f = fixture().await;
    for request in [
        json!({"id":"9","op":"set_enabled","scope_instance_id":OTHER,"instance_id":OTHER,"enabled":false}),
        json!({"id":"10","op":"set_enabled","scope_instance_id":ID,"instance_id":OTHER,"enabled":false}),
        json!({"id":"11","op":"set_credential","scope_instance_id":OTHER,"instance_id":OTHER,"credential":"x"}),
    ] {
        let response = call(&f.socket, request).await;
        assert_eq!(response["error"], "scope_denied", "{response}");
    }
}

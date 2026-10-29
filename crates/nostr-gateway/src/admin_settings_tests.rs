//! D-1097 N1/N3: settings ops on the protected admin socket, exercised over the real socket.

use super::*;
use base64::Engine as _;
use serde_json::json;

const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [4; 32];

struct Fixture {
    _temp: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Mutex<NostrStore>>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn config_b64(relays: &[&str]) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_vec(&json!({
            "relays": relays,
            "self_pubkey": "a".repeat(64),
            "name": "bot",
            "filter": {"keywords": ["crab"]},
            "watches": [{"id": 4, "interval_secs": 1800, "filter": {"authors": ["x"], "kinds": [1]}}],
        }))
        .unwrap(),
    )
}

async fn fixture(nostaro_bin: PathBuf) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
    for (id, agent) in [(ID, "agent-a"), (OTHER, "agent-b")] {
        store
            .upsert_desired(
                id,
                agent,
                &config_b64(&["wss://old.example"]),
                &["addr".into()],
                "nsec-secret-value",
                true,
                &KEY,
            )
            .unwrap();
    }
    store
        .set_access(
            ID,
            &crate::config::AccessConfig {
                owner: vec!["11".repeat(32)],
                ..Default::default()
            },
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let task = spawn(
        socket.clone(),
        AdminContext {
            store: store.clone(),
            master_key: Arc::new(KEY),
            nostaro_bin,
        },
    );
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

async fn call(socket: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    assert!(!line.contains("nsec-secret-value"), "secret leaked: {line}");
    serde_json::from_str(&line).unwrap()
}

fn op(name: &str, extra: Value) -> Value {
    let mut request = json!({"id": name, "op": name, "instance_id": ID});
    for (key, value) in extra.as_object().unwrap() {
        request[key] = value.clone();
    }
    request
}

#[tokio::test]
async fn list_returns_every_instance_without_secrets() {
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let reply = call(&f.socket, json!({"id": "l", "op": "list"})).await;
    assert_eq!(reply["ok"], json!(true), "{reply}");
    let items = reply["result"]["instances"].as_array().unwrap();
    let ids: Vec<_> = items.iter().map(|i| i["instance_id"].clone()).collect();
    assert!(
        ids.contains(&json!(ID)) && ids.contains(&json!(OTHER)),
        "{reply}"
    );
    let a = items
        .iter()
        .find(|i| i["instance_id"] == json!(ID))
        .unwrap();
    assert_eq!(a["agent_id"], json!("agent-a"));
    assert_eq!(a["credential_configured"], json!(true));
    assert!(!reply.to_string().contains("enc:v1:"));
}

#[tokio::test]
async fn get_shows_settings_and_access_but_not_the_key() {
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let reply = call(&f.socket, op("get", json!({}))).await;
    let result = &reply["result"];
    assert_eq!(result["settings"]["relays"], json!(["wss://old.example"]));
    assert_eq!(result["settings"]["name"], json!("bot"));
    assert_eq!(result["settings"]["filter"]["keywords"], json!(["crab"]));
    // The settings page seeds the watch store from these, exactly like the child does.
    assert_eq!(result["settings"]["watches"][0]["id"], json!(4));
    assert_eq!(
        result["settings"]["watches"][0]["interval_secs"],
        json!(1800)
    );
    assert_eq!(result["access"]["owner"], json!(["11".repeat(32)]));
    assert_eq!(result["credential_configured"], json!(true));
}

#[tokio::test]
async fn set_config_validates_and_bumps_generation() {
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let before = f.store.lock().unwrap().get(ID).unwrap().unwrap();
    let bad = call(
        &f.socket,
        op(
            "set_config",
            json!({"settings": {"relays": [], "name": "bot"}}),
        ),
    )
    .await;
    assert_eq!(bad["ok"], json!(false), "{bad}");
    assert_eq!(
        f.store.lock().unwrap().get(ID).unwrap().unwrap().config_b64,
        before.config_b64,
        "rejected settings must not be stored"
    );

    let good = call(
        &f.socket,
        op(
            "set_config",
            json!({"settings": {"relays": ["wss://new.example"], "name": "crab",
                "filter": {"authors": [], "keywords": ["x"], "kinds": [1]}}}),
        ),
    )
    .await;
    assert_eq!(good["ok"], json!(true), "{good}");
    let row = f.store.lock().unwrap().get(ID).unwrap().unwrap();
    assert_eq!(row.desired_generation, before.desired_generation + 1);
    // access lives in identity_projections; the stored runtime config carries none.
    let raw: Value =
        serde_json::from_slice(&crate::config::decode_config_b64(&row.config_b64).unwrap())
            .unwrap();
    assert_eq!(raw["relays"], json!(["wss://new.example"]));
    assert_eq!(raw["name"], json!("crab"));
    assert_eq!(
        raw["self_pubkey"],
        json!("a".repeat(64)),
        "self_pubkey is kept"
    );
    assert!(raw.get("access").is_none());
}

#[tokio::test]
async fn set_access_normalizes_npub_and_refuses_an_empty_set() {
    use nostr::nips::nip19::ToBech32 as _;
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let keys = nostr::Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let empty = call(&f.socket, op("set_access", json!({"settings": {}}))).await;
    assert_eq!(empty["ok"], json!(false), "{empty}");

    let bad = call(
        &f.socket,
        op("set_access", json!({"settings": {"owner": ["not-a-key"]}})),
    )
    .await;
    assert_eq!(bad["ok"], json!(false), "{bad}");

    let good = call(
        &f.socket,
        op(
            "set_access",
            json!({"settings": {"owner": [npub], "trusted_users": ["22".repeat(32)],
                "co_agents": {"33".repeat(32): "peer-agent"}}}),
        ),
    )
    .await;
    assert_eq!(good["ok"], json!(true), "{good}");
    let access = f.store.lock().unwrap().get(ID).unwrap().unwrap().access;
    assert_eq!(access.owner, vec![keys.public_key().to_hex()]);
    assert_eq!(access.trusted_users, vec!["22".repeat(32)]);
    assert_eq!(
        access.co_agents.get(&"33".repeat(32)).map(String::as_str),
        Some("peer-agent")
    );
}

#[tokio::test]
async fn set_key_is_write_only_and_updates_self_pubkey() {
    use nostr::nips::nip19::ToBech32 as _;
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let bad = call(
        &f.socket,
        op("set_key", json!({"credential": "nsec1bogus"})),
    )
    .await;
    assert_eq!(bad["ok"], json!(false), "{bad}");

    let reply = call(
        &f.socket,
        op("set_key", json!({"credential": nsec.clone()})),
    )
    .await;
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert!(!reply.to_string().contains(&nsec), "key must not be echoed");
    assert_eq!(
        reply["result"]["self_pubkey"],
        json!(keys.public_key().to_hex())
    );
    let store = f.store.lock().unwrap();
    assert_eq!(
        &*store.decrypt_credential(ID, &KEY).unwrap(),
        nsec.as_bytes()
    );
    let raw: Value = serde_json::from_slice(
        &crate::config::decode_config_b64(&store.get(ID).unwrap().unwrap().config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["self_pubkey"], json!(keys.public_key().to_hex()));
}

#[tokio::test]
async fn set_enabled_toggles() {
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let reply = call(&f.socket, op("set_enabled", json!({"enabled": false}))).await;
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert!(!f.store.lock().unwrap().get(ID).unwrap().unwrap().enabled);
}

#[tokio::test]
async fn edits_to_an_unknown_instance_are_refused() {
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let reply = call(
        &f.socket,
        json!({"id": "x", "op": "set_enabled", "instance_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc", "enabled": false}),
    )
    .await;
    assert_eq!(reply["ok"], json!(false), "{reply}");
}

fn fake_vanity(dir: &Path, nsec: &str, pubkey: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join("nostaro");
    let args = dir.join("args");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho 'Searching...' >&2\n\
             echo '{{\"nsec\":\"{nsec}\",\"npub\":\"npub1x\",\"pubkey\":\"{pubkey}\"}}'\n",
            args.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    script
}

#[tokio::test]
async fn generate_key_requires_overwrite_and_never_returns_the_key() {
    use nostr::nips::nip19::ToBech32 as _;
    let _env = crate::ENV_LOCK.lock().await;
    let temp = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let script = fake_vanity(temp.path(), &nsec, &keys.public_key().to_hex());
    let f = fixture(script).await;

    let bad_prefix = call(
        &f.socket,
        op("generate_key", json!({"prefix": "abO", "overwrite": true})),
    )
    .await;
    assert_eq!(bad_prefix["ok"], json!(false), "{bad_prefix}");
    assert!(
        !temp.path().join("args").exists(),
        "invalid prefix must not spawn nostaro"
    );

    let refused = call(&f.socket, op("generate_key", json!({"prefix": "qq"}))).await;
    assert_eq!(refused["ok"], json!(false), "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("overwrite"));

    let reply = call(
        &f.socket,
        op("generate_key", json!({"prefix": "qq", "overwrite": true})),
    )
    .await;
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert!(
        !reply.to_string().contains(&nsec),
        "generated key must not be returned"
    );
    assert_eq!(
        reply["result"]["self_pubkey"],
        json!(keys.public_key().to_hex())
    );
    let args = std::fs::read_to_string(temp.path().join("args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["vanity", "--json", "--prefix=qq"]
    );
    let store = f.store.lock().unwrap();
    assert_eq!(
        &*store.decrypt_credential(ID, &KEY).unwrap(),
        nsec.as_bytes()
    );
    let row = store.get(ID).unwrap().unwrap();
    assert!(
        !row.enabled,
        "a new identity starts disabled until the operator enables it"
    );
}

#[tokio::test]
async fn a_key_pasted_into_access_is_rejected_without_echoing_it() {
    use nostr::nips::nip19::ToBech32 as _;
    let f = fixture(PathBuf::from("/nonexistent")).await;
    let nsec = nostr::Keys::generate().secret_key().to_bech32().unwrap();
    let reply = call(
        &f.socket,
        op("set_access", json!({"settings": {"owner": [nsec.clone()]}})),
    )
    .await;
    assert_eq!(reply["ok"], json!(false), "{reply}");
    assert!(!reply.to_string().contains(&nsec), "input echoed: {reply}");
    let reply = call(
        &f.socket,
        op(
            "set_access",
            json!({"settings": {"co_agents": {nsec.clone(): "peer"}}}),
        ),
    )
    .await;
    assert!(!reply.to_string().contains(&nsec), "input echoed: {reply}");
}

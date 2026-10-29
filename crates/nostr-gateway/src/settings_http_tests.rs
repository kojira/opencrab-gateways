//! D-1097 N2: the Nostr gateway settings page over real HTTP, against a real admin socket.

use super::*;
use crate::admin::{AdminContext, AdminScope};
use crate::store::NostrStore;
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Mutex;

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [6; 32];

struct Fixture {
    _temp: tempfile::TempDir,
    base: String,
    run_dir: std::path::PathBuf,
    store: Arc<Mutex<NostrStore>>,
    http: reqwest::Client,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let run_dir = temp.path().join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
    for (id, agent, name) in [(A, "agent-a", "のすたろう"), (B, "agent-b", "らぼみ")] {
        let config = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_vec(&json!({
                "relays": ["wss://r.example"],
                "self_pubkey": "a".repeat(64),
                "name": name,
                "watches": [{"id": 9, "interval_secs": 3600, "filter": {"kinds": [1]}}],
            }))
            .unwrap(),
        );
        store
            .upsert_desired(
                id,
                agent,
                7,
                &config,
                &["addr".into()],
                "nsec-hidden",
                None,
                true,
                &KEY,
            )
            .unwrap();
        store
            .set_access(
                id,
                &crate::config::AccessConfig {
                    owner: vec!["11".repeat(32)],
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let store = Arc::new(Mutex::new(store));
    let admin_socket = temp.path().join("admin.sock");
    let scope: AdminScope = Arc::new([A.to_string(), B.to_string()].into_iter().collect());
    crate::admin::spawn(
        admin_socket.clone(),
        AdminContext {
            scope,
            store: store.clone(),
            master_key: Arc::new(KEY),
            nostaro_bin: "/nonexistent".into(),
        },
    );
    while !admin_socket.exists() {
        tokio::task::yield_now().await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        Settings {
            admin_socket,
            watch_dir: run_dir.clone(),
        },
    ));
    Fixture {
        _temp: temp,
        base: format!("http://{addr}"),
        run_dir,
        store,
        http: reqwest::Client::new(),
    }
}

impl Fixture {
    async fn get(&self, path: &str) -> (reqwest::StatusCode, Value) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(!text.contains("nsec-hidden"), "secret leaked: {text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(!text.contains("nsec-hidden"), "secret leaked: {text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn page_renders_and_lists_every_instance() {
    let f = fixture().await;
    let html = f
        .http
        .get(&f.base)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "relays",
        "owner",
        "trusted_users",
        "co_agents",
        "watch",
        "generate",
    ] {
        assert!(html.contains(needle), "page must expose {needle}");
    }
    assert!(!html.contains("nsec-hidden"));
    let (status, body) = f.get("/api/instances").await;
    assert!(status.is_success());
    let names: Vec<_> = body["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["settings"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["のすたろう", "らぼみ"]);
}

#[tokio::test]
async fn config_and_access_edits_reach_the_store() {
    let f = fixture().await;
    let (status, body) = f
        .post(
            &format!("/api/instances/{B}/config"),
            json!({"relays": ["wss://n.example"], "name": "らぼみ", "filter": {}}),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = f
        .post(
            &format!("/api/instances/{B}/access"),
            json!({"owner": ["22".repeat(32)], "trusted_users": [], "followees": [], "co_agents": {}}),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let row = f.store.lock().unwrap().get(B).unwrap().unwrap();
    assert_eq!(row.access.owner, vec!["22".repeat(32)]);
    let raw: Value =
        serde_json::from_slice(&crate::config::decode_config_b64(&row.config_b64).unwrap())
            .unwrap();
    assert_eq!(raw["relays"], json!(["wss://n.example"]));
    // the other instance is untouched
    assert_eq!(
        f.store
            .lock()
            .unwrap()
            .get(A)
            .unwrap()
            .unwrap()
            .access
            .owner,
        vec!["11".repeat(32)]
    );
}

#[tokio::test]
async fn bad_input_is_400_and_unknown_instance_is_404() {
    let f = fixture().await;
    let (status, _) = f
        .post(
            &format!("/api/instances/{A}/config"),
            json!({"relays": [], "name": "x"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    let unknown = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let (status, _) = f.get(&format!("/api/instances/{unknown}/watches")).await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn key_is_write_only_and_enabled_toggles() {
    use nostr::nips::nip19::ToBech32 as _;
    let f = fixture().await;
    let keys = nostr::Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();
    let (status, body) = f
        .post(&format!("/api/instances/{A}/key"), json!({"nsec": nsec}))
        .await;
    assert!(status.is_success(), "{body}");
    assert!(!body.to_string().contains(&nsec));
    let (_, listed) = f.get("/api/instances").await;
    assert!(!listed.to_string().contains(&nsec));
    let (status, _) = f
        .post(
            &format!("/api/instances/{A}/enabled"),
            json!({"enabled": false}),
        )
        .await;
    assert!(status.is_success());
    assert!(!f.store.lock().unwrap().get(A).unwrap().unwrap().enabled);
}

#[tokio::test]
async fn watches_are_per_instance_and_seeded_like_the_child() {
    let f = fixture().await;
    // First read seeds each instance's store from its own config, as the child does.
    let (_, a) = f.get(&format!("/api/instances/{A}/watches")).await;
    assert_eq!(a["watches"][0]["id"], json!(9));
    let (status, saved) = f
        .post(
            &format!("/api/instances/{B}/watches"),
            json!({"interval_secs": 1800, "authors": ["x"], "kinds": [1, 6]}),
        )
        .await;
    assert!(status.is_success(), "{saved}");
    let path_b = f.run_dir.join(format!("nostaro-watches-{B}.json"));
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&path_b).unwrap()).unwrap();
    assert_eq!(on_disk["watches"].as_array().unwrap().len(), 2);
    let (_, a_again) = f.get(&format!("/api/instances/{A}/watches")).await;
    assert_eq!(
        a_again["watches"].as_array().unwrap().len(),
        1,
        "A must not see B's edit"
    );

    let (status, _) = f
        .post(
            &format!("/api/instances/{B}/watches"),
            json!({"interval_secs": 5}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    let (status, _) = f
        .post(
            &format!("/api/instances/{B}/watches/delete"),
            json!({"id": 9}),
        )
        .await;
    assert!(status.is_success());
    let on_disk: Value = serde_json::from_slice(&std::fs::read(&path_b).unwrap()).unwrap();
    assert_eq!(on_disk["watches"].as_array().unwrap().len(), 1);
}

/// The agent (instance-side `watch_set` / `watch_delete`) writes the same store file; the page
/// shows that change instead of its own stale copy.
#[tokio::test]
async fn watch_list_shows_changes_written_by_the_instance() {
    let f = fixture().await;
    let (_, first) = f.get(&format!("/api/instances/{A}/watches")).await;
    assert_eq!(first["watches"].as_array().unwrap().len(), 1);
    // Leave a gap over the mtime granularity before the other writer touches the file.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let path = f.run_dir.join(format!("nostaro-watches-{A}.json"));
    let instance_side = WatchRegistry::load(path, &[]).unwrap();
    let created = instance_side
        .set(parse_set_request(&json!({"interval_secs": 600, "kinds": [1]})).unwrap())
        .unwrap();
    let (_, after) = f.get(&format!("/api/instances/{A}/watches")).await;
    let ids: Vec<_> = after["watches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&created.id), "{after}");
}

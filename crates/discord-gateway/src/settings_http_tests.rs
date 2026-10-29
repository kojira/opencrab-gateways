//! D-1097 D4: the Discord settings page talks to the daemon only through its admin socket and
//! never returns a secret.

use super::*;
use crate::store::DiscordStore;
use base64::Engine as _;
use std::sync::{Arc, Mutex};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [5; 32];

struct Fixture {
    temp: tempfile::TempDir,
    base: String,
    store: Arc<Mutex<DiscordStore>>,
    tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn config(agent: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_vec(
            &serde_json::json!({"agent_id":agent,"self_bot_id":"111111111111111111"}),
        )
        .unwrap(),
    )
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    for (id, agent) in [(A, "agent-a"), (B, "agent-b")] {
        store
            .upsert_desired(
                id,
                agent,
                7,
                &config(agent),
                &[format!("discord-{agent}-1-2")],
                "token-secret",
                None,
                true,
                &KEY,
            )
            .unwrap();
    }
    let store = Arc::new(Mutex::new(store));
    let scope = Arc::new(std::collections::BTreeSet::from([
        A.to_string(),
        B.to_string(),
    ]));
    let admin = crate::admin::spawn(socket.clone(), scope, store.clone(), Arc::new(KEY));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let voice = temp.path().join("voice").join("settings.json");
    let server = tokio::spawn(serve_on(listener, socket, voice));
    Fixture {
        temp,
        base,
        store,
        tasks: vec![admin, server],
    }
}

#[tokio::test]
async fn d1097_page_lists_every_instance_without_secrets() {
    let f = fixture().await;
    let http = reqwest::Client::new();
    let page = http.get(format!("{}/", f.base)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("api/instances") && html.contains(r#"src="voice/""#));

    let response = http
        .get(format!("{}/api/instances", f.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains("token-secret") && !text.contains("enc:v1:"),
        "{text}"
    );
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    let agents: Vec<_> = body["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["agent_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(agents, vec!["agent-a", "agent-b"]);
    assert_eq!(body["instances"][0]["credential_configured"], true);

    let voice = http
        .get(format!("{}/api/voice-settings", f.base))
        .send()
        .await
        .unwrap();
    assert_eq!(voice.status(), 404, "the voice routes are served here");
    let _ = &f.temp;
}

#[tokio::test]
async fn d1097_page_writes_go_through_the_admin_socket() {
    let f = fixture().await;
    let http = reqwest::Client::new();
    let url = |path: &str| format!("{}/api/instances/{A}/{path}", f.base);

    let put = http
        .put(url("config"))
        .json(
            &serde_json::json!({"agent_id":"agent-a","self_bot_id":"111111111111111111",
            "access":{"owners":["222222222222222222"]}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200, "{}", put.text().await.unwrap());
    let row = f.store.lock().unwrap().get(A).unwrap().unwrap();
    let saved = crate::config::parse_instance_config(
        &crate::config::decode_config_b64(&row.config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.access.owners, vec!["222222222222222222"]);

    let bad = http
        .put(url("config"))
        .json(&serde_json::json!({"agent_id":"agent-a","self_bot_id":"x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    let put = http
        .put(url("addresses"))
        .json(&serde_json::json!({"addresses":["discord-agent-a-1-2","discord-agent-a-1-3"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    assert_eq!(
        f.store
            .lock()
            .unwrap()
            .get(A)
            .unwrap()
            .unwrap()
            .addresses
            .len(),
        2
    );

    let put = http
        .put(url("credential"))
        .json(&serde_json::json!({"credential":"new-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    assert!(!put.text().await.unwrap().contains("new-token"));
    assert_eq!(
        &*f.store.lock().unwrap().decrypt_credential(A, &KEY).unwrap(),
        b"new-token"
    );

    let put = http
        .put(url("enabled"))
        .json(&serde_json::json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    assert!(!f.store.lock().unwrap().get(A).unwrap().unwrap().enabled);

    let unknown = http
        .put(format!(
            "{}/api/instances/cccccccc-cccc-4ccc-8ccc-cccccccccccc/enabled",
            f.base
        ))
        .json(&serde_json::json!({"enabled":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), 400);
}

#[test]
fn d1097_bind_is_an_explicit_socket_address() {
    assert_eq!(
        parse_bind("0.0.0.0:8793").unwrap().to_string(),
        "0.0.0.0:8793"
    );
    assert!(parse_bind("8793").is_err());
}

#[tokio::test]
async fn d1097_the_embedded_voice_page_and_its_api_resolve_under_voice() {
    let f = fixture().await;
    let http = reqwest::Client::new();
    let page = http.get(format!("{}/voice/", f.base)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page
        .text()
        .await
        .unwrap()
        .contains(r#""api/voice-settings""#));
    let api = http
        .get(format!("{}/voice/api/voice-settings", f.base))
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 404, "unconfigured, served by the VC router");
}

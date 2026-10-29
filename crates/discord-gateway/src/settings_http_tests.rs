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
                &config(agent),
                &[format!("discord-{agent}-1-2")],
                "token-secret",
                true,
                &KEY,
            )
            .unwrap();
    }
    let store = Arc::new(Mutex::new(store));
    let admin = crate::admin::spawn(socket.clone(), store.clone(), Arc::new(KEY));
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

/// A fake Discord `GET /users/@me`: returns the bot id for the one token it knows.
async fn fake_discord(known: &'static str, id: &'static str) -> String {
    use axum::http::HeaderMap;
    let app = axum::Router::new().route(
        "/users/@me",
        axum::routing::get(move |headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if auth == format!("Bot {known}") {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({"id": id, "bot": true})),
                )
            } else {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"message":"401: Unauthorized"})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

async fn fixture_with_discord(discord: String) -> Fixture {
    fixture_with(discord, None).await
}

async fn fixture_with(discord: String, core: Option<String>) -> Fixture {
    let mut f = fixture().await;
    for task in f.tasks.drain(1..) {
        task.abort();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    f.base = format!("http://{}", listener.local_addr().unwrap());
    let socket = f.temp.path().join("admin.sock");
    let voice = f.temp.path().join("voice").join("settings.json");
    f.tasks.push(tokio::spawn(serve_with(
        listener,
        socket,
        voice,
        DiscordApi::new(discord),
        core,
    )));
    f
}

/// Slice 1: replacing the token checks it with Discord and stores the bot id it belongs to, so
/// a token for another bot can never run under the old id. The token never comes back.
#[tokio::test]
async fn d1097_token_is_checked_with_discord_and_sets_the_bot_id() {
    let discord = fake_discord("good-token", "999999999999999999").await;
    let f = fixture_with_discord(discord).await;
    let http = reqwest::Client::new();
    let url = format!("{}/api/instances/{A}/credential", f.base);

    let bad = http
        .put(&url)
        .json(&serde_json::json!({"credential":"wrong-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let text = bad.text().await.unwrap();
    assert!(!text.contains("wrong-token"), "{text}");
    assert_eq!(
        &*f.store.lock().unwrap().decrypt_credential(A, &KEY).unwrap(),
        b"token-secret",
        "a rejected token is not stored"
    );

    let good = http
        .put(&url)
        .json(&serde_json::json!({"credential":"good-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), 200);
    let body = good.text().await.unwrap();
    assert!(!body.contains("good-token"), "{body}");
    let row = f.store.lock().unwrap().get(A).unwrap().unwrap();
    assert_eq!(
        &*f.store.lock().unwrap().decrypt_credential(A, &KEY).unwrap(),
        b"good-token"
    );
    let cfg = crate::config::parse_instance_config(
        &crate::config::decode_config_b64(&row.config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg.self_bot_id, "999999999999999999");
}

/// Slice 1: the channel field takes a server id and a channel id and adds that channel.
#[tokio::test]
async fn d1097_channel_field_adds_the_channel_address() {
    let f = fixture().await;
    let http = reqwest::Client::new();
    let add = http
        .post(format!("{}/api/instances/{A}/channels", f.base))
        .json(&serde_json::json!({"guild_id":"100","channel_id":"200"}))
        .send()
        .await
        .unwrap();
    assert_eq!(add.status(), 200, "{}", add.text().await.unwrap());
    assert_eq!(
        f.store.lock().unwrap().get(A).unwrap().unwrap().addresses,
        vec!["discord-agent-a-1-2", "discord-agent-a-100-200"]
    );
    let dm = http
        .post(format!("{}/api/instances/{A}/channels", f.base))
        .json(&serde_json::json!({"guild_id":"","channel_id":"300"}))
        .send()
        .await
        .unwrap();
    assert_eq!(dm.status(), 200);
    for bad in [
        serde_json::json!({"guild_id":"x","channel_id":"200"}),
        serde_json::json!({"guild_id":"100","channel_id":""}),
    ] {
        let response = http
            .post(format!("{}/api/instances/{A}/channels", f.base))
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{bad}");
    }
    assert_eq!(
        f.store
            .lock()
            .unwrap()
            .get(A)
            .unwrap()
            .unwrap()
            .addresses
            .len(),
        3
    );
}

/// A fake core `GET /api/agents`.
async fn fake_core_agents() -> String {
    let app = axum::Router::new().route(
        "/api/agents",
        axum::routing::get(|| async {
            axum::Json(serde_json::json!([
                {"id":"agent-a","name":"A"},{"id":"agent-b","name":"B"},{"id":"agent-new","name":"New"}
            ]))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    base
}

/// The create form lists core's agents and marks those that already have a bot.
#[tokio::test]
async fn d1097_page_lists_core_agents_and_which_have_a_bot() {
    let discord = fake_discord("good-token", "999999999999999999").await;
    let f = fixture_with(discord, Some(fake_core_agents().await)).await;
    let html = reqwest::get(format!("{}/", f.base))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("新しいエージェントの bot"));
    assert!(!html.contains("まだこの画面からは作れません"));
    let agents: serde_json::Value = reqwest::get(format!("{}/api/agents", f.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = agents["agents"].as_array().unwrap();
    let has = |id: &str| rows.iter().find(|a| a["id"] == id).unwrap()["has_bot"].clone();
    assert_eq!(has("agent-a"), true);
    assert_eq!(has("agent-new"), false);
}

/// The create route checks the token with Discord, then asks the daemon to create the bot with
/// the bot id the token belongs to.
#[tokio::test]
async fn d1097_create_route_checks_the_token_and_forwards_to_the_daemon() {
    let discord = fake_discord("good-token", "999999999999999999").await;
    let f = fixture_with(discord, None).await;
    let http = reqwest::Client::new();
    let body = |token: &str| {
        serde_json::json!({"agent_id":"agent-new","credential":token,
            "owners":["222222222222222222"],"guild_id":"100","channel_id":"200"})
    };
    let bad = http
        .post(format!("{}/api/instances", f.base))
        .json(&body("wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(f
        .store
        .lock()
        .unwrap()
        .get_by_agent("agent-new")
        .unwrap()
        .is_none());
    let good = http
        .post(format!("{}/api/instances", f.base))
        .json(&body("good-token"))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), 200);
    let text = good.text().await.unwrap();
    assert!(!text.contains("good-token"));
    let created: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(created["self_bot_id"], "999999999999999999");
    let row = f
        .store
        .lock()
        .unwrap()
        .get(created["instance_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.agent_id, "agent-new");
    assert_eq!(row.addresses, vec!["discord-agent-new-100-200"]);
}

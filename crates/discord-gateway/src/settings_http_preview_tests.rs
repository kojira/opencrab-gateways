//! D-1101b: the settings page plays a bot's aizuchi in the browser.

use super::*;
use crate::store::DiscordStore;
use crate::voice::tests::mock_http;
use base64::Engine as _;
use std::sync::{Arc, Mutex};

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const KEY: [u8; 32] = [5; 32];

struct Fixture {
    _temp: tempfile::TempDir,
    base: String,
    recorded: Arc<mock_http::Recorded>,
    tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn fixture() -> Fixture {
    let (voicevox, recorded) = mock_http::spawn("x").await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.0".into());
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("admin.sock");
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    // mock_http::settings_json gives agent "agent" style 8 and the default style 3.
    let config = base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_vec(&json!({"agent_id":"agent","self_bot_id":"111111111111111111"}))
            .unwrap(),
    );
    store
        .upsert_desired(A, "agent", &config, &[], "token-secret", true, &KEY)
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let admin = crate::admin::spawn(socket.clone(), store, Arc::new(KEY));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let voice = temp.path().join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let settings = voice.join("settings.json");
    std::fs::write(&settings, mock_http::settings_json(&voicevox)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(serve_on(listener, socket, settings));
    Fixture {
        _temp: temp,
        base,
        recorded,
        tasks: vec![admin, server],
    }
}

async fn preview(f: &Fixture, instance: &str, text: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/api/instances/{instance}/aizuchi/preview",
            f.base
        ))
        .json(&json!({ "text": text }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn d1101b_preview_route_returns_the_bots_wav() {
    let f = fixture().await;
    let response = preview(&f, A, "ほうほう、").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "audio/wav");
    assert_eq!(response.bytes().await.unwrap(), mock_http::WAV_FROM_TTS);
    let queries = f.recorded.tts_queries.lock().unwrap().clone();
    assert_eq!(queries, [("ほうほう、".to_string(), "8".to_string())]);
}

#[tokio::test]
async fn d1101b_preview_route_refuses_an_invalid_word_without_synthesis() {
    let f = fixture().await;
    let too_long = "あ".repeat(crate::voice::aizuchi::MAX_WORD_CHARS + 1);
    for word in ["", too_long.as_str()] {
        let response = preview(&f, A, word).await;
        assert_eq!(response.status(), 400, "{word:?}");
        let body: Value = response.json().await.unwrap();
        assert!(body["error"]
            .as_str()
            .unwrap()
            .starts_with("この言葉は使えません"));
    }
    assert!(f.recorded.tts_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn d1101b_preview_route_404s_an_unknown_bot() {
    let f = fixture().await;
    let response = preview(&f, "cccccccc-cccc-4ccc-8ccc-cccccccccccc", "うん、").await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn d1101b_page_has_a_play_button_per_aizuchi_word() {
    let f = fixture().await;
    let page = reqwest::get(&f.base).await.unwrap().text().await.unwrap();
    assert!(
        page.contains("aizuchi/preview"),
        "the page calls the preview route"
    );
    assert!(page.contains("▶"), "each word gets a play button");
    assert!(
        page.contains("aizuchi-words"),
        "words are listed one per row"
    );
}

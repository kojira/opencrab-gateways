//! D-1103: a bot on ZEN-TTS reads, prewarms and previews through the ZEN-TTS server, while a
//! VOICEVOX bot on the same settings stays on VOICEVOX.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;

use super::mock_http;
use crate::voice::aizuchi::AizuchiConfig;
use crate::voice::preview::aizuchi_preview;
use crate::voice::settings::VoiceSettings;
use crate::voice::synthesizer::{SynthesisPriority, Synthesizer};
use crate::voice::tts_cache::{TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

pub const WAV_FROM_ZEN: &[u8] = b"RIFF-fake-wav-from-zen-tts";

#[derive(Default)]
pub struct ZenRecorded {
    pub requests: Mutex<Vec<serde_json::Value>>,
}

/// A stand-in for kojira/zen-tts-server (`/health`, `/voices`, `/synthesize`).
pub async fn spawn_zen() -> (String, Arc<ZenRecorded>) {
    let recorded = Arc::new(ZenRecorded::default());
    let app = Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(serde_json::json!({
                    "ok": true, "version": "1.0.0", "models": {"marine": "marine-1.0", "kikka": "kikka-1.0"}
                }))
            }),
        )
        .route(
            "/voices",
            get(|| async {
                axum::Json(serde_json::json!({
                    "default": "marine",
                    "voices": [{"id": "marine", "name": {"ja": "全力 マリン", "en": "Zenryoku Marine"}}],
                    "emotions": ["neutral", "happy", "sad", "angry", "apologetic"]
                }))
            }),
        )
        .route(
            "/synthesize",
            post(
                |State(r): State<Arc<ZenRecorded>>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                    r.requests.lock().unwrap().push(body);
                    ([("content-type", "audio/wav")], WAV_FROM_ZEN.to_vec())
                },
            ),
        )
        .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), recorded)
}

/// VOICEVOX for every bot except `agent-zen`, which reads with ZEN-TTS `kikka`, happy 70.
fn settings_json(voicevox: &str, zen: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&mock_http::settings_json(voicevox)).unwrap();
    v["tts"]["zen"] = serde_json::json!({"base_url": zen, "default_voice": "marine"});
    v["tts"]["agents"] = serde_json::json!({
        "agent-zen": {"provider": "zen-tts", "voice": "kikka", "emotion": {"happy": 70}}
    });
    v.to_string()
}

#[tokio::test]
async fn d1103_a_zen_bot_reads_through_the_zen_server_and_a_voicevox_bot_does_not() {
    let (voicevox, vv) = mock_http::spawn("x").await;
    *vv.engine_version.lock().unwrap() = Some("0.25.2".into());
    let (zen, zen_recorded) = spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(dir.path().join("c"), MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES),
        Vec::new(),
    );
    let settings = VoiceSettings::from_json(&settings_json(&voicevox, &zen)).unwrap();

    let choice = settings.tts.choice_for_agent("agent-zen");
    let wav = synthesizer
        .synthesize(&settings.tts, &choice, "こんにちは。", SynthesisPriority::Live)
        .await
        .unwrap();
    assert_eq!(wav, WAV_FROM_ZEN);
    let sent = zen_recorded.requests.lock().unwrap().clone();
    assert_eq!(
        sent,
        [serde_json::json!({"text": "こんにちは。", "voice": "kikka", "emotion": {"happy": 70}})]
    );

    let vv_choice = settings.tts.choice_for_agent("agent");
    let wav = synthesizer
        .synthesize(&settings.tts, &vv_choice, "こんにちは。", SynthesisPriority::Live)
        .await
        .unwrap();
    assert_eq!(wav, mock_http::WAV_FROM_TTS);
    // settings_json: agent_voices {"agent": "8"}.
    assert_eq!(
        vv.tts_queries.lock().unwrap().clone(),
        [("こんにちは。".to_string(), "8".to_string())]
    );
    assert_eq!(zen_recorded.requests.lock().unwrap().len(), 1, "VOICEVOX bot never hit ZEN");
}

#[tokio::test]
async fn d1103_prewarm_covers_the_zen_bot_and_its_aizuchi_play_from_cache() {
    let (voicevox, vv) = mock_http::spawn("x").await;
    *vv.engine_version.lock().unwrap() = Some("0.25.2".into());
    let (zen, zen_recorded) = spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let voice = dir.path().join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let path = voice.join("settings.json");
    std::fs::write(&path, settings_json(&voicevox, &zen)).unwrap();
    let phrases = AizuchiConfig::default().prewarm_phrases();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(voice.join("tts-cache"), MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES),
        phrases.clone(),
    );
    let settings = VoiceSettings::from_json(&settings_json(&voicevox, &zen)).unwrap();
    synthesizer.refresh(&settings.tts).await;
    for _ in 0..300 {
        if zen_recorded.requests.lock().unwrap().len() >= phrases.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let zen_count = zen_recorded.requests.lock().unwrap().len();
    assert_eq!(zen_count, phrases.len(), "every phrase prewarmed in the ZEN voice");
    let choice = settings.tts.choice_for_agent("agent-zen");
    assert_eq!(synthesizer.cached(&choice, &phrases[0]).as_deref(), Some(WAV_FROM_ZEN));

    // The settings page preview plays the same cached ZEN audio, without a new synthesis.
    let preview = aizuchi_preview(&path, "agent-zen", &phrases[0]).await.unwrap();
    assert!(preview.from_cache);
    assert_eq!(preview.wav, WAV_FROM_ZEN);
    assert_eq!(zen_recorded.requests.lock().unwrap().len(), zen_count);
}

#[tokio::test]
async fn d1103_preview_names_zen_tts_when_its_server_is_down() {
    let (voicevox, vv) = mock_http::spawn("x").await;
    *vv.engine_version.lock().unwrap() = Some("0.25.2".into());
    let dir = tempfile::tempdir().unwrap();
    let voice = dir.path().join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let path = voice.join("settings.json");
    std::fs::write(&path, settings_json(&voicevox, "http://127.0.0.1:9")).unwrap();
    let error = aizuchi_preview(&path, "agent-zen", "うん、").await.unwrap_err();
    assert!(error.to_string().starts_with("ZEN-TTS で音声を作れませんでした"), "{error}");
}

#[tokio::test]
async fn d1103_pages_show_the_zen_tts_credit_with_a_link() {
    let credit = r#"<a href="https://zenryokukikai.com" target="_blank" rel="noopener">ZEN-TTS（https://zenryokukikai.com ）</a>"#;
    for (name, page) in [
        ("VC settings page", crate::voice::settings_http::PAGE),
        ("Discord settings page", crate::settings_http::PAGE),
    ] {
        assert!(page.contains("音声合成："), "{name} names the credit");
        assert!(page.contains(credit), "{name} links the credit");
    }
}

#[tokio::test]
async fn d1103_test_play_for_a_bot_uses_that_bots_zen_voice() {
    let (voicevox, vv) = mock_http::spawn("x").await;
    *vv.engine_version.lock().unwrap() = Some("0.25.2".into());
    let (zen, zen_recorded) = spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("voice").join("settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, settings_json(&voicevox, &zen)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(crate::voice::settings_http::serve_on(listener, path));
    let http = reqwest::Client::new();

    let voices: serde_json::Value = http
        .get(format!("http://{addr}/api/voice-settings/zen-voices"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(voices["voices"][0]["id"], "marine");

    let wav = http
        .post(format!("http://{addr}/api/voice-settings/test-tts"))
        .json(&serde_json::json!({"text": "試し。", "agent": "agent-zen"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wav.status(), 200);
    assert_eq!(wav.bytes().await.unwrap().as_ref(), WAV_FROM_ZEN);
    assert_eq!(
        zen_recorded.requests.lock().unwrap().last().unwrap(),
        &serde_json::json!({"text": "試し。", "voice": "kikka", "emotion": {"happy": 70}})
    );
}

//! STT / VOICEVOX の mock HTTP サーバ（127.0.0.1 の空きポート）。受けた要求を記録する。
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::Router;

#[derive(Default)]
pub struct Recorded {
    pub stt_bodies: Mutex<Vec<Vec<u8>>>,
    pub tts_queries: Mutex<Vec<(String, String)>>,
}

pub const WAV_FROM_TTS: &[u8] = b"RIFF-fake-wav-from-voicevox";

pub async fn spawn(stt_text: &'static str) -> (String, Arc<Recorded>) {
    let recorded = Arc::new(Recorded::default());
    let app = Router::new()
        .route(
            "/v1/audio/transcriptions",
            post(
                move |State(r): State<Arc<Recorded>>, body: Bytes| async move {
                    r.stt_bodies.lock().unwrap().push(body.to_vec());
                    axum::Json(serde_json::json!({ "text": stt_text }))
                },
            ),
        )
        .route(
            "/audio_query",
            post(
                |State(r): State<Arc<Recorded>>,
                 Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    r.tts_queries
                        .lock()
                        .unwrap()
                        .push((q["text"].clone(), q["speaker"].clone()));
                    axum::Json(serde_json::json!({ "accent_phrases": [] }))
                },
            ),
        )
        .route("/synthesis", post(|| async { WAV_FROM_TTS.to_vec() }))
        .route(
            "/speakers",
            get(|| async {
                axum::Json(serde_json::json!([
                    {"name": "ずんだもん", "styles": [{"name": "ノーマル", "id": 3}]}
                ]))
            }),
        )
        .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), recorded)
}

pub fn settings_json(base: &str) -> String {
    serde_json::json!({
        "stt": {"base_url": format!("{base}/v1"), "model": "reazonspeech-k2-v2", "language": "ja"},
        "tts": {"provider": "voicevox", "base_url": base, "default_voice": "3",
                "agent_voices": {"agent": "8"}}
    })
    .to_string()
}

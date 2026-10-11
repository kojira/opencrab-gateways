//! VC 設定画面（`discord-gateway voice-settings <settings.json> <port>`）。
//! 127.0.0.1 だけに bind する別プロセスで、gateway 本体は HTTP を listen しない。
//!
//! - `GET /` 素の HTML + fetch の 1 ページ
//! - `GET/PUT /api/voice-settings` 設定の読み書き（PUT は検証してから 0600 で原子的に置換）
//! - `GET /api/voice-settings/speakers` VOICEVOX 話者一覧の代理取得
//! - `POST /api/voice-settings/test-tts` 合成した WAV を返す
//! - `POST /api/voice-settings/test-stt` 合成音声を STT にかけて結果を返す（疎通確認）

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::clients;
use super::settings::{load_settings, save_settings, VoiceSettings};

const PAGE: &str = include_str!("settings_page.html");
const STT_TEST_TEXT: &str = "音声認識のテストです";

struct AppState {
    path: PathBuf,
    http: reqwest::Client,
}

type Shared = State<Arc<AppState>>;

/// loopback の `port` で設定画面を動かす（戻るのは失敗時だけ）。
pub async fn run(path: PathBuf, port: u16) -> anyhow::Result<()> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
    tracing::info!(addr = %listener.local_addr()?, path = %path.display(), "voice settings UI listening");
    serve_on(listener, path).await
}

pub async fn serve_on(listener: tokio::net::TcpListener, path: PathBuf) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        path,
        http: clients::http_client(),
    });
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/voice-settings", get(get_settings).put(put_settings))
        .route("/api/voice-settings/speakers", get(get_speakers))
        .route("/api/voice-settings/test-tts", post(test_tts))
        .route("/api/voice-settings/test-stt", post(test_stt))
        .with_state(state);
    axum::serve(listener, app).await?;
    Ok(())
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

/// 保存済み設定。無い・読めないときは返す HTTP 応答（状態と理由）。
fn saved(state: &AppState) -> Result<VoiceSettings, (StatusCode, String)> {
    if !state.path.exists() {
        return Err((StatusCode::CONFLICT, "voice is not configured".into()));
    }
    load_settings(&state.path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

async fn get_settings(State(state): Shared) -> Response {
    if !state.path.exists() {
        return error(StatusCode::NOT_FOUND, "voice is not configured");
    }
    match load_settings(&state.path) {
        Ok(settings) => Json(settings).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn put_settings(State(state): Shared, body: String) -> Response {
    let settings = match VoiceSettings::from_json(&body) {
        Ok(settings) => settings,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    match save_settings(&state.path, &settings) {
        Ok(()) => Json(settings).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn get_speakers(State(state): Shared) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    match clients::speakers(&state.http, &settings.tts).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct TtsTest {
    text: String,
    #[serde(default)]
    voice: Option<String>,
}

async fn test_tts(State(state): Shared, Json(request): Json<TtsTest>) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    let voice = request
        .voice
        .unwrap_or_else(|| settings.tts.default_voice.clone());
    match clients::synthesize(&state.http, &settings.tts, &request.text, &voice).await {
        Ok(wav) => ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn test_stt(State(state): Shared) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    let voice = settings.tts.default_voice.clone();
    let wav = match clients::synthesize(&state.http, &settings.tts, STT_TEST_TEXT, &voice).await {
        Ok(wav) => wav,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("TTS: {e:#}")),
    };
    match clients::transcribe(&state.http, &settings.stt, wav).await {
        Ok(text) => Json(json!({"expected": STT_TEST_TEXT, "text": text})).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("STT: {e:#}")),
    }
}

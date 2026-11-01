//! VC 設定画面（`discord-gateway voice-settings <settings.json> <port>`）。
//! 127.0.0.1 だけに bind する別プロセスで、gateway 本体は HTTP を listen しない。
//!
//! - `GET /` 素の HTML + fetch の 1 ページ（`GET /dict-editor.js` は辞書の編集部品）
//! - `GET/PUT /api/voice-settings` 設定の読み書き（PUT は検証してから 0600 で原子的に置換）
//! - `GET /api/voice-settings/speakers` VOICEVOX 話者一覧の代理取得
//! - `GET /api/voice-settings/zen-voices` ZEN-TTS の声の一覧の代理取得（D-1103）
//! - `POST /api/voice-settings/test-tts` 合成した WAV を返す（`dict` は試聴だけの辞書。保存しない）
//! - `POST /api/voice-settings/analyze` 言葉の読み・拍・アクセント型（ZEN-TTS の代理。D-1106）
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
use super::dictionary::DictionaryEntry;
use super::settings::{load_settings, save_settings, TtsEngine, VoiceChoice, VoiceSettings};

pub const PAGE: &str = include_str!("settings_page.html");
/// 読み上げ辞書の編集部品（VC 設定画面と bot の画面が `dict-editor.js` で読む。D-1106）。
pub const DICT_EDITOR_JS: &str = include_str!("dict_editor.js");
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
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .merge(api_router(path));
    axum::serve(listener, app).await?;
    Ok(())
}

/// The `/api/voice-settings*` routes for the settings file at `path`. The Discord settings page
/// (D-1097) serves them too, so both pages share one implementation.
pub fn api_router(path: PathBuf) -> Router {
    let state = Arc::new(AppState {
        path,
        http: clients::http_client(),
    });
    Router::new()
        .route(
            "/dict-editor.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    DICT_EDITOR_JS,
                )
            }),
        )
        .route("/api/voice-settings", get(get_settings).put(put_settings))
        .route("/api/voice-settings/speakers", get(get_speakers))
        .route("/api/voice-settings/zen-voices", get(get_zen_voices))
        .route("/api/voice-settings/test-tts", post(test_tts))
        .route("/api/voice-settings/test-stt", post(test_stt))
        .route("/api/voice-settings/analyze", post(analyze))
        .with_state(state)
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

async fn get_zen_voices(State(state): Shared) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    match clients::zen_voices(&state.http, &settings.tts).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct TtsTest {
    text: String,
    /// VOICEVOX のスタイル ID（`agent` が無いとき。省けば既定）。
    #[serde(default)]
    voice: Option<String>,
    /// この bot（agent）の読み上げで試す（D-1103。エンジン・声・感情はその bot の設定）。
    #[serde(default)]
    agent: Option<String>,
    /// 試聴だけに使う辞書の項目（D-1106）。保存済みの辞書に重ねて、この 1 回だけ使う。
    /// `agent` が無ければ ZEN-TTS の既定の声で読む（共通の辞書の編集画面から）。
    #[serde(default)]
    dict: Option<Vec<DictionaryEntry>>,
}

async fn test_tts(State(state): Shared, Json(request): Json<TtsTest>) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    let shared = || settings.tts.dictionary.for_agent("");
    let mut choice = match (
        &request.agent,
        &request.voice,
        &request.dict,
        &settings.tts.zen,
    ) {
        (Some(agent), _, _, _) => settings.tts.choice_for_agent(agent),
        (None, None, Some(_), Some(zen)) => {
            VoiceChoice::zen(&zen.default_voice, None, None).with_dictionary(shared())
        }
        (None, Some(voice), _, _) => VoiceChoice::voicevox(voice).with_dictionary(shared()),
        (None, None, _, _) => {
            VoiceChoice::voicevox(&settings.tts.default_voice).with_dictionary(shared())
        }
    };
    if let Some(overrides) = &request.dict {
        choice.dictionary = match choice.dictionary.with_overrides(overrides) {
            Ok(dictionary) => dictionary,
            Err(e) => return error(StatusCode::BAD_REQUEST, format!("{e:#}")),
        };
    }
    match clients::synthesize_choice(&state.http, &settings.tts, &choice, &request.text).await {
        Ok(wav) => ([(header::CONTENT_TYPE, "audio/wav")], wav).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct AnalyzeRequest {
    text: String,
    /// この bot（agent）が ZEN-TTS なら、その声で調べる。ほかは ZEN-TTS の既定の声。
    #[serde(default)]
    agent: Option<String>,
    /// 調べるときに使う辞書の項目（読みを直したとき、その読みで拍を出す）。
    #[serde(default)]
    dict: Option<Vec<DictionaryEntry>>,
}

/// 言葉の読み・拍・アクセント型（ZEN-TTS `POST /analyze` の代理）。辞書の編集画面が、読みと
/// アクセントを自動で入れ、拍の箱を出すのに使う。ZEN-TTS が無ければ 404。
async fn analyze(State(state): Shared, Json(request): Json<AnalyzeRequest>) -> Response {
    let settings = match saved(&state) {
        Ok(settings) => settings,
        Err((status, message)) => return error(status, message),
    };
    let Some(zen) = &settings.tts.zen else {
        return error(StatusCode::NOT_FOUND, "ZEN-TTS is not configured");
    };
    let voice = match request
        .agent
        .as_deref()
        .map(|agent| settings.tts.choice_for_agent(agent))
    {
        Some(choice) if choice.engine == TtsEngine::ZenTts => choice.voice,
        _ => zen.default_voice.clone(),
    };
    let mut body = json!({"text": request.text, "voice": voice});
    if let Some(dict) = &request.dict {
        body["dict"] = json!(dict);
    }
    match clients::zen_analyze(&state.http, &settings.tts, &body).await {
        Ok((status, value)) => {
            let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let status = if status.is_success() || status == StatusCode::BAD_REQUEST {
                status
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(value)).into_response()
        }
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

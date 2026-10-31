//! STT（OpenAI 互換 `/audio/transcriptions`）と、読み上げ（VOICEVOX・ZEN-TTS）の HTTP クライアント。
//! 接続先は毎回 [`super::settings`] から渡す（再起動なしで切り替わる）。

use std::time::Duration;

use anyhow::{bail, Context};

use super::settings::{SttSettings, TtsEngine, TtsSettings, VoiceChoice};

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// 合成の固定値（版 [`SYNTHESIS_CONSTANTS_VERSION`]）。値を変えたら版を上げ、TTS キャッシュを捨てさせる。
/// 話速は TTS キャッシュのキーに入る。
pub const TTS_SPEED_SCALE: f64 = 1.0;
/// 先頭の無音（秒）。VOICEVOX 既定の 0.1 秒を 0 にして、読み上げの出だしを早める。
/// 末尾の無音（postPhonemeLength）は VOICEVOX 既定のまま上書きしない。
pub const PRE_PHONEME_LENGTH: f64 = 0.0;
pub const SYNTHESIS_CONSTANTS_VERSION: u32 = 1;

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("reqwest client")
}

pub async fn transcribe(
    http: &reqwest::Client,
    stt: &SttSettings,
    wav: Vec<u8>,
) -> anyhow::Result<String> {
    let file = reqwest::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", file)
        .text("model", stt.model.clone())
        .text("response_format", "json");
    if let Some(language) = &stt.language {
        form = form.text("language", language.clone());
    }
    let url = format!(
        "{}/audio/transcriptions",
        stt.base_url.trim_end_matches('/')
    );
    let response = http
        .post(url)
        .multipart(form)
        .send()
        .await
        .context("STT request failed")?;
    let status = response.status();
    let body = response.text().await.context("STT response body")?;
    if !status.is_success() {
        bail!("STT failed ({status}): {}", truncate(&body));
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).context("STT response JSON")?;
    Ok(parsed["text"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// `choice` のエンジンと声で WAV を作る（D-1103）。
pub async fn synthesize_choice(
    http: &reqwest::Client,
    tts: &TtsSettings,
    choice: &VoiceChoice,
    text: &str,
) -> anyhow::Result<Vec<u8>> {
    match choice.engine {
        TtsEngine::Voicevox => synthesize(http, tts, text, &choice.voice).await,
        TtsEngine::ZenTts => zen_synthesize(http, tts, choice, text).await,
    }
}

/// ZEN-TTS サーバー（kojira/zen-tts-server）の `POST /synthesize`。
async fn zen_synthesize(
    http: &reqwest::Client,
    tts: &TtsSettings,
    choice: &VoiceChoice,
    text: &str,
) -> anyhow::Result<Vec<u8>> {
    let zen = tts.zen.as_ref().context("ZEN-TTS is not configured")?;
    let mut body = serde_json::json!({"text": text, "voice": choice.voice});
    if let Some(emotion) = &choice.emotion {
        body["emotion"] = emotion.clone();
    }
    if let Some(strength) = choice.strength {
        body["strength"] = strength.into();
    }
    let response = http
        .post(format!("{}/synthesize", zen.base_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .context("ZEN-TTS request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("ZEN-TTS failed ({status}): {}", truncate(&body));
    }
    Ok(response.bytes().await.context("ZEN-TTS audio")?.to_vec())
}

/// VOICEVOX の 2 段階 API（audio_query → synthesis）で WAV を作る。
pub async fn synthesize(
    http: &reqwest::Client,
    tts: &TtsSettings,
    text: &str,
    voice: &str,
) -> anyhow::Result<Vec<u8>> {
    let base = tts.base_url.trim_end_matches('/');
    let query = http
        .post(format!("{base}/audio_query"))
        .query(&[("text", text), ("speaker", voice)])
        .send()
        .await
        .context("VOICEVOX audio_query request failed")?;
    let status = query.status();
    let query_body = query.text().await.context("VOICEVOX audio_query body")?;
    if !status.is_success() {
        bail!(
            "VOICEVOX audio_query failed ({status}): {}",
            truncate(&query_body)
        );
    }
    let mut query: serde_json::Value =
        serde_json::from_str(&query_body).context("VOICEVOX audio_query JSON")?;
    let Some(fields) = query.as_object_mut() else {
        bail!("VOICEVOX audio_query is not an object");
    };
    fields.insert("speedScale".into(), TTS_SPEED_SCALE.into());
    fields.insert("prePhonemeLength".into(), PRE_PHONEME_LENGTH.into());
    let synthesis = http
        .post(format!("{base}/synthesis"))
        .query(&[("speaker", voice)])
        .header("content-type", "application/json")
        .body(query.to_string())
        .send()
        .await
        .context("VOICEVOX synthesis request failed")?;
    let status = synthesis.status();
    if !status.is_success() {
        let body = synthesis.text().await.unwrap_or_default();
        bail!("VOICEVOX synthesis failed ({status}): {}", truncate(&body));
    }
    Ok(synthesis.bytes().await.context("VOICEVOX audio")?.to_vec())
}

/// TTS キャッシュの有効性を決める版。設定にある全エンジンの版をつなげたもの（D-1103）。
/// ZEN-TTS が設定されていなければ VOICEVOX の版だけ（D-1103 以前のキャッシュはそのまま使える）。
/// どれかの版が読めなければ、今まで通りキャッシュは使わない（合成はする）。
pub async fn engine_version(http: &reqwest::Client, tts: &TtsSettings) -> anyhow::Result<String> {
    let voicevox = voicevox_version(http, tts).await?;
    match &tts.zen {
        None => Ok(voicevox),
        Some(zen) => Ok(format!("{voicevox}+zen:{}", zen_version(http, &zen.base_url).await?)),
    }
}

/// ZEN-TTS の版とモデル（`GET /health` の `version` と `models`）。
async fn zen_version(http: &reqwest::Client, base_url: &str) -> anyhow::Result<String> {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let response = http.get(url).send().await.context("ZEN-TTS health")?;
    let status = response.status();
    if !status.is_success() {
        bail!("ZEN-TTS health failed ({status})");
    }
    let health: serde_json::Value = response.json().await.context("ZEN-TTS health JSON")?;
    let version = health["version"].as_str().unwrap_or_default();
    if version.is_empty() {
        bail!("ZEN-TTS health has no version");
    }
    Ok(format!("{version}/{}", health["models"]))
}

/// ZEN-TTS の声の一覧（`GET /voices`。設定画面のプルダウン用）。
pub async fn zen_voices(
    http: &reqwest::Client,
    tts: &TtsSettings,
) -> anyhow::Result<serde_json::Value> {
    let zen = tts.zen.as_ref().context("ZEN-TTS is not configured")?;
    let url = format!("{}/voices", zen.base_url.trim_end_matches('/'));
    let response = http.get(url).send().await.context("ZEN-TTS voices")?;
    let status = response.status();
    if !status.is_success() {
        bail!("ZEN-TTS voices failed ({status})");
    }
    response.json().await.context("ZEN-TTS voices JSON")
}

/// VOICEVOX エンジンの版（`GET /version`、JSON 文字列）。
async fn voicevox_version(http: &reqwest::Client, tts: &TtsSettings) -> anyhow::Result<String> {
    let url = format!("{}/version", tts.base_url.trim_end_matches('/'));
    let response = http.get(url).send().await.context("VOICEVOX version")?;
    let status = response.status();
    if !status.is_success() {
        bail!("VOICEVOX version failed ({status})");
    }
    let version: String = response.json().await.context("VOICEVOX version JSON")?;
    if version.trim().is_empty() {
        bail!("VOICEVOX version is empty");
    }
    Ok(version)
}

/// VOICEVOX の話者一覧（設定画面のプルダウン用）。
pub async fn speakers(
    http: &reqwest::Client,
    tts: &TtsSettings,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}/speakers", tts.base_url.trim_end_matches('/'));
    let response = http.get(url).send().await.context("VOICEVOX speakers")?;
    let status = response.status();
    if !status.is_success() {
        bail!("VOICEVOX speakers failed ({status})");
    }
    response.json().await.context("VOICEVOX speakers JSON")
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

//! STT（OpenAI 互換 `/audio/transcriptions`）と VOICEVOX の HTTP クライアント。
//! 接続先は毎回 [`super::settings`] から渡す（再起動なしで切り替わる）。

use std::time::Duration;

use anyhow::{bail, Context};

use super::settings::{SttSettings, TtsSettings};

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

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
    let synthesis = http
        .post(format!("{base}/synthesis"))
        .query(&[("speaker", voice)])
        .header("content-type", "application/json")
        .body(query_body)
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

//! D-1101b: 設定画面で相づちを試聴する。VC で流すのと同じ音声を返す。
//!
//! 同じ音声にするため、bot と同じ道を通る。
//! - 声: [`TtsSettings::choice_for_agent`]（bot ごとのエンジンと声、無ければ既定。D-1103）
//! - 合成: [`clients::synthesize_choice`]（VOICEVOX の話速などは VC と同じ）
//! - キャッシュ: bot と共有の `voice/tts-cache`（[`TtsCache`]）。あれば VC と同じ音声をそのまま
//!   返し、無ければ合成してキャッシュへ入れる（次に VC で流すときもそれを使う）

use std::path::Path;

use super::aizuchi::AizuchiConfig;
use super::clients;
use super::settings::{load_settings, TtsEngine};
use super::tts_cache::{choice_cache_key, TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

/// 試聴の結果。
#[derive(Debug)]
pub struct Preview {
    pub wav: Vec<u8>,
    /// bot と共有の TTS キャッシュから返したか（false なら合成した）。
    pub from_cache: bool,
}

/// 試聴できなかった理由。画面にそのまま出す日本語を持つ。
#[derive(Debug)]
pub enum PreviewError {
    /// 相づちとして保存できない言葉（空、長すぎる、制御文字）。
    InvalidWord(String),
    /// VC の接続先がまだ設定されていない。
    NotConfigured,
    /// 読み上げエンジンにつながらない、または合成に失敗した（どのエンジンか、理由）。
    Engine(&'static str, String),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWord(reason) => write!(f, "この言葉は使えません: {reason}"),
            Self::NotConfigured => write!(f, "VC の音声設定がまだありません"),
            Self::Engine(engine, reason) => write!(f, "{engine} で音声を作れませんでした: {reason}"),
        }
    }
}

/// `agent_id` の bot が相づち `text` を言うときの WAV。保存前の言葉でもよい。
pub async fn aizuchi_preview(
    settings_path: &Path,
    agent_id: &str,
    text: &str,
) -> Result<Preview, PreviewError> {
    let word = text.trim().to_string();
    AizuchiConfig {
        enabled: true,
        question: vec![word.clone()],
        statement: Vec::new(),
    }
    .validate()
    .map_err(|e| PreviewError::InvalidWord(word_rule(&format!("{e:#}"))))?;
    if !settings_path.exists() {
        return Err(PreviewError::NotConfigured);
    }
    let settings = load_settings(settings_path)
        .map_err(|e| PreviewError::Engine("読み上げ", format!("{e:#}")))?;
    let tts = settings.tts;
    let choice = tts.choice_for_agent(agent_id);
    let engine = match choice.engine {
        TtsEngine::Voicevox => "VOICEVOX",
        TtsEngine::ZenTts => "ZEN-TTS",
    };
    let http = clients::http_client();
    let cache_dir = settings_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tts-cache");
    let cache = TtsCache::open(cache_dir, MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES);
    // bot と同じ規則: エンジンの版が確かめられたときだけキャッシュを使う。
    if let Ok(version) = clients::engine_version(&http, &tts).await {
        if cache.verify(&version).is_err() {
            cache.disable();
        }
    }
    let key = choice_cache_key(&choice, &word);
    if let Some(wav) = cache.get(&key) {
        tracing::info!(
            text_chars = word.chars().count(),
            from_cache = true,
            "aizuchi preview"
        );
        return Ok(Preview {
            wav,
            from_cache: true,
        });
    }
    let wav = clients::synthesize_choice(&http, &tts, &choice, &word)
        .await
        .map_err(|e| PreviewError::Engine(engine, format!("{e:#}")))?;
    cache.insert(&key, &wav);
    tracing::info!(
        text_chars = word.chars().count(),
        from_cache = false,
        "aizuchi preview"
    );
    Ok(Preview {
        wav,
        from_cache: false,
    })
}

/// 検証の英語の理由を、画面に出す日本語にする。
fn word_rule(reason: &str) -> String {
    if reason.contains("nonempty") {
        "空です".into()
    } else if reason.contains("chars") {
        format!("{}文字までです", super::aizuchi::MAX_WORD_CHARS)
    } else if reason.contains("control") {
        "改行や制御文字は使えません".into()
    } else {
        reason.into()
    }
}

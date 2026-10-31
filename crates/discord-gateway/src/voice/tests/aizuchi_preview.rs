//! D-1101b: 設定画面の相づち試聴は、VC で流すのと同じ声・同じキャッシュを使う。
use std::sync::Arc;
use std::time::Duration;

use super::mock_http::{self, Recorded};
use crate::voice::aizuchi::AizuchiConfig;
use crate::voice::preview::{aizuchi_preview, PreviewError};
use crate::voice::settings::VoiceSettings;
use crate::voice::synthesizer::Synthesizer;
use crate::voice::tts_cache::{TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

async fn mock() -> (String, Arc<Recorded>) {
    let (base, recorded) = mock_http::spawn("x").await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.0".into());
    (base, recorded)
}

/// `voice/settings.json` と、その隣の `voice/tts-cache`（bot と同じ配置）。
fn settings_file(dir: &std::path::Path, base: &str) -> std::path::PathBuf {
    let voice = dir.join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let path = voice.join("settings.json");
    std::fs::write(&path, mock_http::settings_json(base)).unwrap();
    path
}

fn synth_count(recorded: &Recorded) -> usize {
    recorded.synthesis_bodies.lock().unwrap().len()
}

#[tokio::test]
async fn d1101b_preview_uses_the_bots_own_voice_over_the_default() {
    let (base, recorded) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = settings_file(dir.path(), &base);

    let preview = aizuchi_preview(&path, "agent", "ほうほう、").await.unwrap();
    assert_eq!(preview.wav, mock_http::WAV_FROM_TTS);
    aizuchi_preview(&path, "other-agent", "ほうほう、")
        .await
        .unwrap();

    let speakers: Vec<String> = recorded
        .tts_queries
        .lock()
        .unwrap()
        .iter()
        .map(|(_, speaker)| speaker.clone())
        .collect();
    // settings_json: agent_voices {"agent": "8"}, default_voice "3".
    assert_eq!(speakers, ["8", "3"]);
}

#[tokio::test]
async fn d1101b_preview_of_a_prewarmed_word_comes_from_the_bots_cache() {
    let (base, recorded) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = settings_file(dir.path(), &base);
    // The bot prewarms its aizuchi into voice/tts-cache, exactly as VoiceManager does.
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(
            path.parent().unwrap().join("tts-cache"),
            MEMORY_LIMIT_BYTES,
            DISK_LIMIT_BYTES,
        ),
        AizuchiConfig::default().prewarm_phrases(),
    );
    let settings = VoiceSettings::from_json(&mock_http::settings_json(&base)).unwrap();
    synthesizer.refresh(&settings.tts).await;
    let expected = 2 * AizuchiConfig::default().prewarm_phrases().len();
    for _ in 0..200 {
        if synth_count(&recorded) >= expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(synth_count(&recorded), expected);

    let preview = aizuchi_preview(&path, "agent", "なるほど、").await.unwrap();
    assert!(preview.from_cache);
    assert_eq!(preview.wav, mock_http::WAV_FROM_TTS);
    assert_eq!(synth_count(&recorded), expected, "no new synthesis");
}

#[tokio::test]
async fn d1101b_preview_synthesizes_an_unsaved_word_and_caches_it() {
    let (base, recorded) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = settings_file(dir.path(), &base);

    let first = aizuchi_preview(&path, "agent", "まだ保存していない、")
        .await
        .unwrap();
    assert!(!first.from_cache);
    assert_eq!(first.wav, mock_http::WAV_FROM_TTS);
    assert_eq!(synth_count(&recorded), 1);
    let second = aizuchi_preview(&path, "agent", "まだ保存していない、")
        .await
        .unwrap();
    assert!(second.from_cache);
    assert_eq!(synth_count(&recorded), 1);
}

#[tokio::test]
async fn d1101b_preview_refuses_a_word_the_bot_could_not_save() {
    let (base, recorded) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = settings_file(dir.path(), &base);
    let too_long = "あ".repeat(crate::voice::aizuchi::MAX_WORD_CHARS + 1);
    for word in ["", "  ", too_long.as_str(), "改\n行"] {
        let result = aizuchi_preview(&path, "agent", word).await;
        assert!(
            matches!(result, Err(PreviewError::InvalidWord(_))),
            "{word:?}"
        );
    }
    assert_eq!(recorded.tts_queries.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn d1101b_preview_says_plainly_when_voicevox_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let path = settings_file(dir.path(), "http://127.0.0.1:9");
    let error = aizuchi_preview(&path, "agent", "うん、").await.unwrap_err();
    assert!(matches!(error, PreviewError::Engine(..)));
    assert!(error
        .to_string()
        .starts_with("VOICEVOX で音声を作れませんでした"));
}

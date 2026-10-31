//! D-1106: 読み上げ辞書（共通と bot ごと）。VOICEVOX でも ZEN-TTS でも、合成の直前に本文を置き換える。
use serde_json::json;

use super::mock_http;
use crate::voice::aizuchi::AizuchiConfig;
use crate::voice::preview::aizuchi_preview;
use crate::voice::settings::{load_settings, save_settings, VoiceSettings};
use crate::voice::synthesizer::{SynthesisPriority, Synthesizer};
use crate::voice::tts_cache::{TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

/// mock の VOICEVOX 設定に辞書を足す。`agent` は自分の辞書で「API」を「アピ」と読む。
fn with_dictionary(base: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(&mock_http::settings_json(base)).unwrap();
    v["tts"]["dictionary"] = json!({
        "shared": [
            {"word": "AI", "reading": "エーアイ"},
            {"word": "OpenAI", "reading": "オープンエーアイ"},
            {"word": "エー", "reading": "びー"},
            {"word": "API", "reading": "エーピーアイ"},
            {"word": "(笑)", "reading": ""}
        ],
        "agents": {"agent": [{"word": "API", "reading": "アピ"}]}
    });
    v
}

fn settings(base: &str) -> VoiceSettings {
    VoiceSettings::from_json(&with_dictionary(base).to_string()).unwrap()
}

#[test]
fn d1106_longest_word_first_and_replaced_text_is_not_replaced_again() {
    let tts = settings("http://127.0.0.1:9").tts;
    let other = tts.choice_for_agent("other");
    // 「OpenAI」は「AI」より先に当たる。置き換えた「エーアイ」の「エー」はもう置き換えない。
    assert_eq!(
        other.reading("OpenAIとAIの話(笑)"),
        "オープンエーアイとエーアイの話"
    );
    // 元の本文の「エー」は置き換える。
    assert_eq!(other.reading("エーと"), "びーと");
}

#[test]
fn d1106_a_bots_own_entry_wins_over_the_shared_one() {
    let tts = settings("http://127.0.0.1:9").tts;
    assert_eq!(tts.choice_for_agent("agent").reading("APIです"), "アピです");
    assert_eq!(
        tts.choice_for_agent("other").reading("APIです"),
        "エーピーアイです"
    );
    // 共通の言葉は bot の辞書に無くても効く。
    assert_eq!(tts.choice_for_agent("agent").reading("AI"), "エーアイ");
}

#[test]
fn d1106_dictionary_round_trips_and_absent_dictionary_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let with = settings("http://127.0.0.1:50021");
    save_settings(&path, &with).unwrap();
    assert_eq!(load_settings(&path).unwrap(), with);

    let without =
        VoiceSettings::from_json(&mock_http::settings_json("http://127.0.0.1:50021")).unwrap();
    save_settings(&path, &without).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("dictionary"), "{raw}");
    assert_eq!(load_settings(&path).unwrap(), without);
}

#[test]
fn d1106_invalid_dictionaries_are_rejected() {
    let long_word = "あ".repeat(65);
    let long_reading = "あ".repeat(129);
    let cases = [
        json!({"shared": [{"word": "", "reading": "x"}]}),
        json!({"shared": [{"word": long_word, "reading": "x"}]}),
        json!({"shared": [{"word": "x", "reading": long_reading}]}),
        json!({"shared": [{"word": "AI", "reading": "a"}, {"word": "AI", "reading": "b"}]}),
        json!({"agents": {"agent": [{"word": "AI", "reading": "a"}, {"word": "AI", "reading": "b"}]}}),
        json!({"agents": {"": [{"word": "AI", "reading": "a"}]}}),
    ];
    for dictionary in cases {
        let mut v = with_dictionary("http://127.0.0.1:50021");
        v["tts"]["dictionary"] = dictionary.clone();
        assert!(
            VoiceSettings::from_json(&v.to_string()).is_err(),
            "{dictionary} must be rejected"
        );
    }
    // 64 / 128 文字ちょうどと、空の読み（読まない）は使える。
    let mut v = with_dictionary("http://127.0.0.1:50021");
    v["tts"]["dictionary"] = json!({"shared": [
        {"word": "あ".repeat(64), "reading": "い".repeat(128)},
        {"word": "w", "reading": ""}
    ]});
    VoiceSettings::from_json(&v.to_string()).unwrap();
}

fn tts_texts(recorded: &mock_http::Recorded) -> Vec<String> {
    recorded
        .tts_queries
        .lock()
        .unwrap()
        .iter()
        .map(|(text, _)| text.clone())
        .collect()
}

#[tokio::test]
async fn d1106_vc_speech_reads_through_the_dictionary_and_a_change_takes_effect() {
    let (base, recorded) = mock_http::spawn("x").await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.0".into());
    let dir = tempfile::tempdir().unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(
            dir.path().join("tts-cache"),
            MEMORY_LIMIT_BYTES,
            DISK_LIMIT_BYTES,
        ),
        Vec::new(),
    );
    let tts = settings(&base).tts;
    synthesizer.refresh(&tts).await;
    let choice = tts.choice_for_agent("agent");
    synthesizer
        .synthesize(&tts, &choice, "APIの話。", SynthesisPriority::Live)
        .await
        .unwrap();
    assert_eq!(tts_texts(&recorded), ["アピの話。"]);

    // 辞書を変えると、同じ本文でも新しい読みで合成する（古い音声をキャッシュから使わない）。
    let mut changed = with_dictionary(&base);
    changed["tts"]["dictionary"]["agents"]["agent"] =
        json!([{"word": "API", "reading": "えいぴーあい"}]);
    let tts = VoiceSettings::from_json(&changed.to_string()).unwrap().tts;
    let choice = tts.choice_for_agent("agent");
    synthesizer
        .synthesize(&tts, &choice, "APIの話。", SynthesisPriority::Live)
        .await
        .unwrap();
    assert_eq!(tts_texts(&recorded), ["アピの話。", "えいぴーあいの話。"]);
}

#[tokio::test]
async fn d1106_aizuchi_prewarm_and_preview_use_the_bots_dictionary() {
    let (base, recorded) = mock_http::spawn("x").await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.0".into());
    let dir = tempfile::tempdir().unwrap();
    let voice = dir.path().join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let path = voice.join("settings.json");
    let mut v = with_dictionary(&base);
    v["tts"]["dictionary"]["agents"]["agent"] = json!([{"word": "うん、", "reading": "うむ、"}]);
    std::fs::write(&path, v.to_string()).unwrap();

    let preview = aizuchi_preview(&path, "agent", "うん、").await.unwrap();
    assert_eq!(preview.wav, mock_http::WAV_FROM_TTS);
    assert_eq!(tts_texts(&recorded), ["うむ、"]);

    // The bot's prewarm puts the same reading in the cache, so the VC aizuchi finds it.
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(
            voice.join("tts-cache"),
            MEMORY_LIMIT_BYTES,
            DISK_LIMIT_BYTES,
        ),
        AizuchiConfig::default().prewarm_phrases(),
    );
    let tts = load_settings(&path).unwrap().tts;
    synthesizer.refresh(&tts).await;
    assert!(synthesizer
        .cached(&tts.choice_for_agent("agent"), "うん、")
        .is_some());
}

#[tokio::test]
async fn d1106_test_play_reads_through_the_dictionary() {
    let (base, recorded) = mock_http::spawn("x").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, with_dictionary(&base).to_string()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(crate::voice::settings_http::serve_on(listener, path));
    let http = reqwest::Client::new();
    for body in [
        json!({"text": "APIです", "agent": "agent"}),
        json!({"text": "APIです"}),
    ] {
        let res = http
            .post(format!("http://{addr}/api/voice-settings/test-tts"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.headers()["content-type"], "audio/wav");
    }
    assert_eq!(tts_texts(&recorded), ["アピです", "エーピーアイです"]);
}

#[test]
fn d1106_pages_have_the_shared_and_per_bot_dictionaries() {
    assert!(crate::voice::settings_http::PAGE.contains("読み上げ辞書（共通）"));
    assert!(crate::settings_http::PAGE.contains("この bot の辞書"));
}

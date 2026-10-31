//! D-1106: 読み上げ辞書（共通と bot ごと）。VOICEVOX は本文の言葉を読みに置き換え、ZEN-TTS は
//! 重ねた辞書を `dict` で渡す（読みと抑揚）。
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
            {"surface": "AI", "reading": "エーアイ"},
            {"surface": "OpenAI", "reading": "オープンエーアイ"},
            {"surface": "エー", "reading": "びー"},
            {"surface": "API", "reading": "エーピーアイ"},
            {"surface": "(笑)", "reading": ""}
        ],
        "agents": {"agent": [{"surface": "API", "reading": "アピ"}]}
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
        other.engine_text("OpenAIとAIの話(笑)"),
        "オープンエーアイとエーアイの話"
    );
    // 元の本文の「エー」は置き換える。
    assert_eq!(other.engine_text("エーと"), "びーと");
}

#[test]
fn d1106_a_bots_own_entry_wins_over_the_shared_one() {
    let tts = settings("http://127.0.0.1:9").tts;
    assert_eq!(
        tts.choice_for_agent("agent").engine_text("APIです"),
        "アピです"
    );
    assert_eq!(
        tts.choice_for_agent("other").engine_text("APIです"),
        "エーピーアイです"
    );
    // 共通の言葉は bot の辞書に無くても効く。
    assert_eq!(tts.choice_for_agent("agent").engine_text("AI"), "エーアイ");
}

#[test]
fn d1106_dictionary_round_trips_and_absent_dictionary_is_not_written() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    // Intonation fields from the zen-tts demo export are kept as they are.
    let mut v = with_dictionary("http://127.0.0.1:50021");
    v["tts"]["dictionary"]["shared"][0] = json!({"surface": "AI", "reading": "エーアイ",
        "accent": 1, "phrase": "join", "strength": 0.3, "contour": [1.5, -0.5, 0], "pitch": [0.2], "length": [1.1, 1]});
    let with = VoiceSettings::from_json(&v.to_string()).unwrap();
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
        json!({"shared": [{"surface": "", "reading": "x"}]}),
        json!({"shared": [{"surface": long_word, "reading": "x"}]}),
        json!({"shared": [{"surface": "x", "reading": long_reading}]}),
        json!({"shared": [{"surface": "AI", "reading": "a"}, {"surface": "AI", "reading": "b"}]}),
        json!({"agents": {"agent": [{"surface": "AI", "reading": "a"}, {"surface": "AI", "reading": "b"}]}}),
        json!({"agents": {"": [{"surface": "AI", "reading": "a"}]}}),
        json!({"shared": [{"surface": "AI", "reading": "a", "phrase": "cut"}]}),
        json!({"shared": [{"surface": "AI", "reading": "a", "contour": [0, 4]}]}),
        json!({"shared": [{"word": "AI", "reading": "a"}]}),
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
        {"surface": "あ".repeat(64), "reading": "い".repeat(128)},
        {"surface": "w", "reading": ""},
        {"surface": "明日", "reading": "アス", "accent": 2, "phrase": "split", "strength": 0.5,
         "contour": [1, -1], "pitch": [0.5, 0], "length": [1, 1.2]}
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
        json!([{"surface": "API", "reading": "えいぴーあい"}]);
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
    v["tts"]["dictionary"]["agents"]["agent"] = json!([{"surface": "うん、", "reading": "うむ、"}]);
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

/// VOICEVOX for every bot except `agent-zen` (ZEN-TTS). Shared: 明日→アス (accent 2) and AI;
/// agent-zen overrides 明日 with its own reading and contour.
fn zen_settings(voicevox: &str, zen: &str) -> String {
    let mut v = with_dictionary(voicevox);
    v["tts"]["zen"] = json!({"base_url": zen, "default_voice": "marine"});
    v["tts"]["agents"] = json!({"agent-zen": {"provider": "zen-tts"}});
    v["tts"]["dictionary"] = json!({
        "shared": [
            {"surface": "明日", "reading": "アス", "accent": 2},
            {"surface": "AI", "reading": "エーアイ"}
        ],
        "agents": {"agent-zen": [{"surface": "明日", "reading": "アシタ", "accent": 0, "contour": [1, 0, -1]}]}
    });
    v.to_string()
}

#[tokio::test]
async fn d1106_a_zen_bot_sends_the_merged_dictionary_and_keeps_the_text() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (zen, zen_recorded) = super::zen_tts::spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, zen_settings(&voicevox, &zen)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(crate::voice::settings_http::serve_on(listener, path));
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/voice-settings/test-tts"))
        .json(&json!({"text": "明日のAI", "agent": "agent-zen"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        zen_recorded.requests.lock().unwrap().last().unwrap(),
        &json!({"text": "明日のAI", "voice": "marine", "dict": [
            {"surface": "明日", "reading": "アシタ", "accent": 0, "contour": [1.0, 0.0, -1.0]},
            {"surface": "AI", "reading": "エーアイ"}
        ]})
    );
}

#[test]
fn d1106_the_cache_key_changes_with_the_dictionary() {
    use crate::voice::tts_cache::choice_cache_key;
    let (a, b) = (
        VoiceSettings::from_json(&zen_settings("http://127.0.0.1:1", "http://127.0.0.1:2"))
            .unwrap(),
        {
            let mut v: serde_json::Value =
                serde_json::from_str(&zen_settings("http://127.0.0.1:1", "http://127.0.0.1:2"))
                    .unwrap();
            v["tts"]["dictionary"]["agents"]["agent-zen"][0]["contour"] = json!([0, 1, 0]);
            VoiceSettings::from_json(&v.to_string()).unwrap()
        },
    );
    assert_ne!(
        choice_cache_key(&a.tts.choice_for_agent("agent-zen"), "明日"),
        choice_cache_key(&b.tts.choice_for_agent("agent-zen"), "明日")
    );
}

#[test]
fn d1106_pages_have_the_shared_and_per_bot_dictionaries() {
    assert!(crate::voice::settings_http::PAGE.contains("読み上げ辞書（共通）"));
    assert!(crate::settings_http::PAGE.contains("この bot の辞書"));
    for page in [
        crate::voice::settings_http::PAGE,
        crate::settings_http::PAGE,
    ] {
        // Import / export in the zen-tts demo format.
        assert!(page.contains("zen-tts-dictionary"));
        assert!(page.contains("書き出す") && page.contains("読み込む"));
    }
}

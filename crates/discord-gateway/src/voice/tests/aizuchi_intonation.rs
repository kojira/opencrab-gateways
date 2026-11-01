//! D-1107: an aizuchi item may carry its own way of saying it (ZEN-TTS intonation). The same word
//! may be listed again with another intonation; each item is chosen, prewarmed, cached and
//! previewed on its own. The intonation reaches ZEN-TTS only for that aizuchi, never for speech.
use std::time::Duration;

use serde_json::{json, Value};

use super::mock_http;
use super::zen_tts::{spawn_zen, ZenRecorded};
use crate::config::parse_instance_config;
use crate::voice::aizuchi::{choose_aizuchi, AizuchiConfig, AizuchiWord};
use crate::voice::preview::aizuchi_preview;
use crate::voice::settings::VoiceSettings;
use crate::voice::synthesizer::{SynthesisPriority, Synthesizer};
use crate::voice::tts_cache::{choice_cache_key, TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

/// 「うん、」 rising (raised on the last sound, stretched) and 「うん、」 falling (drops after ウ).
fn rising() -> Value {
    json!({"text": "うん、", "reading": "ウン", "pitch": [0.0, 3.0], "length": [1.0, 1.5]})
}
fn falling() -> Value {
    json!({"text": "うん、", "reading": "ウン", "accent": 1})
}

fn instance(aizuchi: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"agent_id":"a","self_bot_id":"111","aizuchi":aizuchi})).unwrap()
}

/// `agent-zen` reads with ZEN-TTS `kikka`; everything else is VOICEVOX.
fn settings_json(voicevox: &str, zen: &str) -> String {
    let mut v: Value = serde_json::from_str(&mock_http::settings_json(voicevox)).unwrap();
    v["tts"]["zen"] = json!({"base_url": zen, "default_voice": "marine"});
    v["tts"]["agents"] = json!({"agent-zen": {"provider": "zen-tts", "voice": "kikka"}});
    v.to_string()
}

fn zen_requests(recorded: &ZenRecorded) -> Vec<Value> {
    recorded.requests.lock().unwrap().clone()
}

#[test]
fn d1107_an_old_plain_config_round_trips_unchanged() {
    let aizuchi =
        json!({"enabled": true, "question": ["えっと、"], "statement": ["うん、", "へえ、"]});
    let cfg = parse_instance_config(&instance(aizuchi.clone())).unwrap();
    assert_eq!(cfg.aizuchi.statement, ["うん、", "へえ、"]);
    assert_eq!(serde_json::to_value(&cfg.aizuchi).unwrap(), aizuchi);
}

#[test]
fn d1107_two_intonations_of_the_same_word_both_round_trip() {
    let aizuchi =
        json!({"enabled": true, "question": [], "statement": [rising(), falling(), "へえ、"]});
    let cfg = parse_instance_config(&instance(aizuchi.clone())).unwrap();
    assert_eq!(
        cfg.aizuchi.statement.len(),
        3,
        "the same word twice is allowed"
    );
    assert_ne!(cfg.aizuchi.statement[0], cfg.aizuchi.statement[1]);
    assert_eq!(serde_json::to_value(&cfg.aizuchi).unwrap(), aizuchi);
}

#[test]
fn d1107_bad_intonations_are_refused() {
    for bad in [
        json!({"text": "うん、", "accent": 1}),
        json!({"text": "うん、", "reading": "ウン", "pitch": [0.0, 7.0]}),
        json!({"text": "うん、", "reading": "ウン", "length": [1.0, 3.0]}),
        json!({"text": "、", "reading": "ウン", "accent": 1}),
        json!({"text": "うん、", "reading": "ウン", "volume": 1}),
    ] {
        assert!(
            parse_instance_config(&instance(json!({"statement": [bad.clone()]}))).is_err(),
            "accepted {bad}"
        );
    }
}

#[test]
fn d1107_each_intonation_is_its_own_choice() {
    let cfg: AizuchiConfig =
        serde_json::from_value(json!({"question": [], "statement": [rising(), falling()]}))
            .unwrap();
    let picks: Vec<AizuchiWord> = (0..4)
        .map(|pick| choose_aizuchi(&cfg, "今日は晴れ", None, pick).unwrap())
        .collect();
    assert!(picks.contains(&cfg.statement[0]) && picks.contains(&cfg.statement[1]));
    // The previous item is avoided, not every item with the same word.
    for pick in 0..4 {
        let next = choose_aizuchi(&cfg, "今日は晴れ", Some(&cfg.statement[0]), pick).unwrap();
        assert_eq!(next, cfg.statement[1]);
    }
    // Each item is prewarmed (both 「うん、」s).
    let prewarm = cfg.prewarm_phrases();
    assert!(prewarm.contains(&cfg.statement[0]) && prewarm.contains(&cfg.statement[1]));
}

#[tokio::test]
async fn d1107_the_aizuchi_carries_its_intonation_and_speech_does_not() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (zen, recorded) = spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(dir.path().join("c"), MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES),
        Vec::new(),
    );
    let settings = VoiceSettings::from_json(&settings_json(&voicevox, &zen)).unwrap();
    let base = settings.tts.choice_for_agent("agent-zen");
    let up: AizuchiWord = serde_json::from_value(rising()).unwrap();
    let down: AizuchiWord = serde_json::from_value(falling()).unwrap();

    for word in [&up, &down] {
        synthesizer
            .synthesize(
                &settings.tts,
                &word.voice_choice(&base),
                &word.text,
                SynthesisPriority::Live,
            )
            .await
            .unwrap();
    }
    synthesizer
        .synthesize(
            &settings.tts,
            &base,
            "うん、そうだね。",
            SynthesisPriority::Live,
        )
        .await
        .unwrap();

    let sent = zen_requests(&recorded);
    assert_eq!(
        sent[0],
        json!({"text": "うん、", "voice": "kikka",
            "dict": [{"surface": "うん", "reading": "ウン", "pitch": [0.0, 3.0], "length": [1.0, 1.5]}]})
    );
    assert_eq!(
        sent[1],
        json!({"text": "うん、", "voice": "kikka",
            "dict": [{"surface": "うん", "reading": "ウン", "accent": 1}]})
    );
    assert_eq!(
        sent[2],
        json!({"text": "うん、そうだね。", "voice": "kikka"}),
        "normal speech gets no aizuchi intonation"
    );

    // Each intonation is its own audio in the cache; a plain 「うん、」 is a third.
    let keys = [
        choice_cache_key(&up.voice_choice(&base), "うん、"),
        choice_cache_key(&down.voice_choice(&base), "うん、"),
        choice_cache_key(&base, "うん、"),
    ];
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert_ne!(keys[1], keys[2]);
}

#[tokio::test]
async fn d1107_prewarm_and_preview_use_each_items_intonation() {
    let (voicevox, vv) = mock_http::spawn("x").await;
    *vv.engine_version.lock().unwrap() = Some("0.25.2".into());
    let (zen, recorded) = spawn_zen().await;
    let dir = tempfile::tempdir().unwrap();
    let voice = dir.path().join("voice");
    std::fs::create_dir_all(&voice).unwrap();
    let path = voice.join("settings.json");
    std::fs::write(&path, settings_json(&voicevox, &zen)).unwrap();
    let up: AizuchiWord = serde_json::from_value(rising()).unwrap();
    let down: AizuchiWord = serde_json::from_value(falling()).unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(
            voice.join("tts-cache"),
            MEMORY_LIMIT_BYTES,
            DISK_LIMIT_BYTES,
        ),
        vec![up.clone(), down.clone()],
    );
    let settings = VoiceSettings::from_json(&settings_json(&voicevox, &zen)).unwrap();
    synthesizer.refresh(&settings.tts).await;
    let base = settings.tts.choice_for_agent("agent-zen");
    for _ in 0..300 {
        if synthesizer
            .cached(&up.voice_choice(&base), "うん、")
            .is_some()
            && synthesizer
                .cached(&down.voice_choice(&base), "うん、")
                .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(synthesizer
        .cached(&up.voice_choice(&base), "うん、")
        .is_some());
    assert!(synthesizer
        .cached(&down.voice_choice(&base), "うん、")
        .is_some());
    let prewarmed = zen_requests(&recorded).len();

    // The preview of each item plays its prewarmed audio.
    assert!(
        aizuchi_preview(&path, "agent-zen", up.clone())
            .await
            .unwrap()
            .from_cache
    );
    assert!(
        aizuchi_preview(&path, "agent-zen", down.clone())
            .await
            .unwrap()
            .from_cache
    );
    assert_eq!(zen_requests(&recorded).len(), prewarmed);
    // A changed intonation is new audio.
    let mut higher = up.clone();
    higher.intonation.as_mut().unwrap().pitch = Some(vec![0.0, 4.0]);
    let preview = aizuchi_preview(&path, "agent-zen", higher).await.unwrap();
    assert!(!preview.from_cache);
    assert_eq!(
        zen_requests(&recorded).last().unwrap()["dict"][0]["pitch"],
        json!([0.0, 4.0])
    );
}

#[test]
fn d1107_page_has_the_aizuchi_intonation_editor_without_numbers() {
    let page = crate::settings_http::PAGE;
    let js = crate::settings_http::AIZUCHI_EDITOR_JS;
    assert!(page.contains(r#"<script src="aizuchi-editor.js"></script>"#));
    assert!(page.contains("aizuchiList("));
    for needed in [
        "言い方",
        "同じ言葉で別の言い方",
        "平板",
        "おまかせ",
        "▲",
        "▼",
        "のばす",
        "ちぢめる",
        "/analyze",
        "aizuchi/preview",
    ] {
        assert!(
            js.contains(needed) || page.contains(needed),
            "lacks {needed}"
        );
    }
    assert!(
        !js.contains("type=\"number\"") && !js.contains("inputMode"),
        "no number to type"
    );
    assert!(
        !js.contains("<input type=\"range\""),
        "no slider with numbers"
    );
    // The page CSS shows buttons and flex rows; a closed item hides its intonation controls.
    assert!(
        js.contains(".az-actions[hidden]"),
        "closed rows hide their controls"
    );
}

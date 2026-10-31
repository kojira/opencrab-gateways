//! D-1085: TTS キャッシュ（T1–T6）と合成の固定値 v1。
use std::sync::Arc;
use std::time::Duration;

use super::e2e::{core, manager_with, wait_played, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::transport::DryRunTransport;
use crate::voice::aizuchi::AizuchiConfig;
use crate::voice::clients::{SYNTHESIS_CONSTANTS_VERSION, TTS_SPEED_SCALE};
use crate::voice::settings::VoiceSettings;
use crate::voice::synthesizer::{SynthesisPriority, Synthesizer};
use crate::voice::tts_cache::{cache_key, TtsCache, DISK_LIMIT_BYTES, MEMORY_LIMIT_BYTES};

/// 既定の相づち設定での先読み本文。
fn prewarm_phrases() -> Vec<String> {
    AizuchiConfig::default().prewarm_phrases()
}

/// 設定にあるスタイル（既定 3 と agent の 8）すべての先読み件数。
fn prewarm_jobs() -> usize {
    2 * prewarm_phrases().len()
}

async fn mock_with_version(delay_ms: u64) -> (String, Arc<Recorded>) {
    let (mock, recorded) =
        mock_http::spawn_with_synthesis_delay(vec![(0, "unused")], delay_ms).await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.2".into());
    (mock, recorded)
}

fn synth_count(recorded: &Recorded) -> usize {
    recorded.synthesis_started.lock().unwrap().len()
}

async fn wait_synth(recorded: &Recorded, n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while synth_count(recorded) < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("synthesis did not happen");
}

/// 先読みの合成が止まる（200ms 増えない）まで待ち、それまでの合成数を返す。
async fn settle(recorded: &Recorded) -> usize {
    wait_synth(recorded, 1).await;
    let mut seen = synth_count(recorded);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = synth_count(recorded);
        if now == seen {
            return now;
        }
        seen = now;
    }
}

#[test]
fn cache_key_is_sha256_over_engine_style_speed_and_text_only() {
    let key = cache_key("3", 1.0, "うん、");
    assert_eq!(key.len(), 64);
    assert_eq!(key, cache_key("3", 1.0, "うん、"));
    assert_ne!(key, cache_key("8", 1.0, "うん、"));
    assert_ne!(key, cache_key("3", 1.2, "うん、"));
    assert_ne!(key, cache_key("3", 1.0, "うん。"));
    assert_eq!(TTS_SPEED_SCALE, 1.0);
}

#[tokio::test]
async fn t1_same_style_speed_and_text_is_synthesized_once() {
    let (mock, recorded) = mock_with_version(0).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager_with(&core, path, player.clone(), Arc::new(DryRunTransport));
    m.join(BINDING, Some("555"), None).await.unwrap();
    let base = settle(&recorded).await;

    m.speak_posted("10", "今日はいい天気だね。");
    wait_played(&player, 1).await;
    m.speak_posted("10", "今日はいい天気だね。");
    wait_played(&player, 2).await;

    assert_eq!(synth_count(&recorded) - base, 1, "the second one must hit");
    let played = player.played.lock().unwrap();
    assert_eq!(played[0].1, played[1].1);
    assert_eq!(played[0].1, mock_http::WAV_FROM_TTS);
}

#[tokio::test]
async fn t2_a_different_style_misses() {
    let (mock, recorded) = mock_with_version(0).await;
    let dir = tempfile::tempdir().unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(dir.path().join("c"), MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES),
        prewarm_phrases(),
    );
    let settings = VoiceSettings::from_json(&mock_http::settings_json(&mock)).unwrap();
    synthesizer.refresh(&settings.tts).await;
    let base = settle(&recorded).await;
    for style in ["3", "8", "3"] {
        synthesizer
            .synthesize(
                &settings.tts,
                &crate::voice::settings::VoiceChoice::voicevox(style),
                "別の文。",
                SynthesisPriority::Live,
            )
            .await
            .unwrap();
    }
    assert_eq!(synth_count(&recorded) - base, 2);
}

#[test]
fn t3_memory_and_disk_stay_within_their_byte_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let cache = TtsCache::open(dir.path().to_path_buf(), 25, 35);
    cache.verify("v").unwrap();
    for i in 0..5 {
        cache.insert(&cache_key("3", 1.0, &i.to_string()), &[i as u8; 10]);
    }
    let (memory, disk) = cache.usage();
    assert!(memory <= 25, "memory {memory}");
    assert!(disk <= 35, "disk {disk}");
    let files = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "wav")
        })
        .count();
    assert_eq!(files, 3, "3 WAVs of 10 bytes fit in 35");
    // 最近のものが残り、古いものから消える。
    assert!(cache.get(&cache_key("3", 1.0, "4")).is_some());
    assert!(cache.get(&cache_key("3", 1.0, "0")).is_none());
}

#[test]
fn t4_a_disk_entry_survives_a_restart_and_is_0600() {
    let dir = tempfile::tempdir().unwrap();
    let key = cache_key("3", 1.0, "残る。");
    {
        let cache = TtsCache::open(dir.path().to_path_buf(), 1000, 1000);
        cache.verify("v").unwrap();
        cache.insert(&key, b"wav");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join(format!("{key}.wav")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let cache = TtsCache::open(dir.path().to_path_buf(), 1000, 1000);
    // 版を確かめるまでは使わない。
    assert!(cache.get(&key).is_none());
    cache.verify("v").unwrap();
    assert_eq!(cache.get(&key).as_deref(), Some(&b"wav"[..]));
}

#[test]
fn engine_or_constants_version_change_wipes_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let key = cache_key("3", 1.0, "消える。");
    {
        let cache = TtsCache::open(dir.path().to_path_buf(), 1000, 1000);
        cache.verify("0.25.2").unwrap();
        cache.insert(&key, b"wav");
    }
    let cache = TtsCache::open(dir.path().to_path_buf(), 1000, 1000);
    cache.verify("0.25.3").unwrap();
    assert!(cache.get(&key).is_none());
    assert!(!dir.path().join(format!("{key}.wav")).exists());

    // 合成の固定値の版が記録と違う場合も捨てる。
    cache.insert(&key, b"wav");
    std::fs::write(
        dir.path().join("cache-version.json"),
        serde_json::json!({
            "engine_version": "0.25.3",
            "synthesis_constants_version": SYNTHESIS_CONSTANTS_VERSION + 1
        })
        .to_string(),
    )
    .unwrap();
    let cache = TtsCache::open(dir.path().to_path_buf(), 1000, 1000);
    cache.verify("0.25.3").unwrap();
    assert!(cache.get(&key).is_none());
}

#[tokio::test]
async fn unreadable_engine_version_disables_the_cache() {
    // engine_version None = /version が 404。
    let (mock, recorded) = mock_http::spawn_with_synthesis_delay(vec![(0, "unused")], 0).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager_with(&core, path, player.clone(), Arc::new(DryRunTransport));
    m.join(BINDING, Some("555"), None).await.unwrap();
    m.speak_posted("10", "同じ文。");
    wait_played(&player, 1).await;
    m.speak_posted("10", "同じ文。");
    wait_played(&player, 2).await;
    assert_eq!(synth_count(&recorded), 2, "no prewarm and no cache hit");
    assert!(!dir
        .path()
        .join("tts-cache")
        .join("cache-version.json")
        .exists());
}

#[tokio::test]
async fn t5_prewarm_covers_every_phrase_for_every_configured_style() {
    let (mock, recorded) = mock_with_version(0).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let _m = manager_with(
        &core,
        path,
        Arc::new(FakePlayer::default()),
        Arc::new(DryRunTransport),
    );
    assert_eq!(settle(&recorded).await, prewarm_jobs());
    let mut queries = recorded.tts_queries.lock().unwrap().clone();
    queries.sort();
    let mut expected: Vec<(String, String)> = ["3", "8"]
        .iter()
        .flat_map(|style| {
            prewarm_phrases()
                .into_iter()
                .map(|phrase| (phrase, style.to_string()))
        })
        .collect();
    expected.sort();
    assert_eq!(queries, expected);
}

#[tokio::test]
async fn t6_a_live_job_is_synthesized_before_queued_prewarm_jobs() {
    let (mock, recorded) = mock_with_version(80).await;
    let dir = tempfile::tempdir().unwrap();
    let synthesizer = Synthesizer::new(
        crate::voice::clients::http_client(),
        TtsCache::open(dir.path().join("c"), MEMORY_LIMIT_BYTES, DISK_LIMIT_BYTES),
        prewarm_phrases(),
    );
    let settings = VoiceSettings::from_json(&mock_http::settings_json(&mock)).unwrap();
    synthesizer.refresh(&settings.tts).await;
    wait_synth(&recorded, 1).await;
    synthesizer
        .synthesize(
            &settings.tts,
            &crate::voice::settings::VoiceChoice::voicevox("3"),
            "いま読む文。",
            SynthesisPriority::Live,
        )
        .await
        .unwrap();
    let queries = recorded.tts_queries.lock().unwrap().clone();
    let at = queries
        .iter()
        .position(|(text, _)| text == "いま読む文。")
        .unwrap();
    assert!(at <= 1, "live job was synthesized at {at}: {queries:?}");
}

#[tokio::test]
async fn synthesis_constants_v1_set_speed_and_zero_leading_silence() {
    let (mock, recorded) = mock_http::spawn_with_synthesis_delay(vec![(0, "unused")], 0).await;
    let settings = VoiceSettings::from_json(&mock_http::settings_json(&mock)).unwrap();
    crate::voice::clients::synthesize(
        &crate::voice::clients::http_client(),
        &settings.tts,
        "文。",
        "3",
    )
    .await
    .unwrap();
    let body: serde_json::Value =
        serde_json::from_str(&recorded.synthesis_bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body["speedScale"], 1.0, "{body}");
    assert_eq!(body["prePhonemeLength"], 0.0, "{body}");
    assert!(body.get("postPhonemeLength").is_none(), "{body}");
    assert_eq!(body["accent_phrases"], serde_json::json!([]), "{body}");
}

#[test]
fn d1103_cache_keys_differ_by_engine_voice_emotion_and_strength() {
    use crate::voice::settings::VoiceChoice;
    use crate::voice::tts_cache::choice_cache_key;
    // VOICEVOX keys are exactly the pre-D-1103 keys, so the existing cache stays valid.
    assert_eq!(
        choice_cache_key(&VoiceChoice::voicevox("3"), "うん、"),
        cache_key("3", 1.0, "うん、")
    );
    let zen = |voice: &str, emotion: Option<serde_json::Value>, strength: Option<f64>| {
        VoiceChoice::zen(voice, emotion, strength)
    };
    let base = choice_cache_key(&zen("marine", None, None), "うん、");
    assert_ne!(
        base,
        cache_key("marine", 1.0, "うん、"),
        "engine is in the key"
    );
    assert_ne!(base, choice_cache_key(&zen("kikka", None, None), "うん、"));
    assert_ne!(
        base,
        choice_cache_key(
            &zen("marine", Some(serde_json::json!({"happy": 70})), None),
            "うん、"
        )
    );
    assert_ne!(
        base,
        choice_cache_key(&zen("marine", None, Some(1.2)), "うん、")
    );
    // The same emotion written in another key order is the same audio.
    assert_eq!(
        choice_cache_key(
            &zen(
                "marine",
                Some(serde_json::json!({"happy": 70, "sad": 30})),
                None
            ),
            "うん、"
        ),
        choice_cache_key(
            &zen(
                "marine",
                Some(serde_json::json!({"sad": 30, "happy": 70})),
                None
            ),
            "うん、"
        )
    );
}

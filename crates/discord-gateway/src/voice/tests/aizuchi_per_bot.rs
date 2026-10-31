//! D-1101: 相づちの言葉と on/off は bot ごとの設定（instance config の `aizuchi`）。
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with_aizuchi, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::config::parse_instance_config;
use crate::transport::DryRunTransport;
use crate::voice::aizuchi::{choose_aizuchi, AizuchiConfig, MAX_WORDS, MAX_WORD_CHARS};
use crate::voice::session::VoiceManager;

fn custom(question: &[&str], statement: &[&str]) -> AizuchiConfig {
    AizuchiConfig {
        enabled: true,
        question: question.iter().map(|s| s.to_string()).collect(),
        statement: statement.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn d1101_missing_config_keeps_todays_defaults() {
    let cfg = parse_instance_config(br#"{"agent_id":"a","self_bot_id":"111"}"#).unwrap();
    assert_eq!(cfg.aizuchi, AizuchiConfig::default());
    assert!(cfg.aizuchi.enabled);
    assert_eq!(cfg.aizuchi.question, ["えっと、", "うーん、"]);
    assert_eq!(
        cfg.aizuchi.statement,
        ["うん、", "へえ、", "なるほど、", "そっか、", "ああ、"]
    );
    // 一部だけ書いた設定は、書かなかった項目が既定値になる。
    let cfg =
        parse_instance_config(br#"{"agent_id":"a","self_bot_id":"111","aizuchi":{"enabled":false}}"#)
            .unwrap();
    assert!(!cfg.aizuchi.enabled);
    assert_eq!(cfg.aizuchi.question, ["えっと、", "うーん、"]);
}

#[test]
fn d1101_a_custom_list_is_chosen_from() {
    let cfg = custom(&["それで？"], &["ほうほう、", "ふむ、"]);
    for pick in 0..10 {
        assert_eq!(
            choose_aizuchi(&cfg, "元気ですか", None, pick).as_deref(),
            Some("それで？")
        );
        let s = choose_aizuchi(&cfg, "今日は晴れ", None, pick).unwrap();
        assert!(["ほうほう、", "ふむ、"].contains(&s.as_str()), "{s}");
    }
    // 1 語だけの組は、直前と同じでもその語を使う。
    assert_eq!(
        choose_aizuchi(&cfg, "元気ですか", Some("それで？"), 0).as_deref(),
        Some("それで？")
    );
}

#[test]
fn d1101_disabled_never_chooses() {
    let cfg = AizuchiConfig {
        enabled: false,
        ..Default::default()
    };
    for text in ["元気ですか", "今日は晴れ"] {
        assert_eq!(choose_aizuchi(&cfg, text, None, 0), None);
    }
    assert!(!cfg.prewarm_phrases().iter().any(|p| p == "うん、"));
}

#[test]
fn d1101_an_empty_list_plays_nothing_of_that_kind() {
    let cfg = custom(&[], &["うん、"]);
    assert_eq!(choose_aizuchi(&cfg, "元気ですか", None, 0), None);
    assert_eq!(
        choose_aizuchi(&cfg, "今日は晴れ", None, 0).as_deref(),
        Some("うん、")
    );
    let cfg = custom(&["えっと、"], &[]);
    assert_eq!(choose_aizuchi(&cfg, "今日は晴れ", None, 0), None);
}

#[test]
fn d1101_validation_rejects_bad_words() {
    let base = |aizuchi: Value| {
        serde_json::to_vec(&json!({"agent_id":"a","self_bot_id":"111","aizuchi":aizuchi})).unwrap()
    };
    let long: String = "あ".repeat(MAX_WORD_CHARS + 1);
    let many: Vec<String> = (0..=MAX_WORDS).map(|i| format!("語{i}")).collect();
    for bad in [
        json!({"question":[""]}),
        json!({"statement":["   "]}),
        json!({"statement":[long]}),
        json!({"question":many}),
        json!({"statement":["改\n行"]}),
        json!({"unknown":true}),
    ] {
        assert!(
            parse_instance_config(&base(bad.clone())).is_err(),
            "accepted {bad}"
        );
    }
    let ok = "あ".repeat(MAX_WORD_CHARS);
    assert!(parse_instance_config(&base(json!({"question":[],"statement":[ok]}))).is_ok());
}

#[test]
fn d1101_prewarm_includes_the_custom_words() {
    let phrases = custom(&["それで？"], &["ほうほう、"]).prewarm_phrases();
    assert!(phrases.iter().any(|p| p == "それで？"), "{phrases:?}");
    assert!(phrases.iter().any(|p| p == "ほうほう、"), "{phrases:?}");
    assert!(
        !phrases.iter().any(|p| p == "えっと、"),
        "the defaults are not prewarmed when replaced: {phrases:?}"
    );
}

struct Bot {
    core: Core,
    player: Arc<FakePlayer>,
    recorded: Arc<Recorded>,
    manager: Arc<VoiceManager>,
    _dir: tempfile::TempDir,
}

/// 1 体の bot（自分の相づち設定）を VC に入れ、先読みが終わるまで待つ。
async fn bot(stt: &'static str, aizuchi: AizuchiConfig) -> Bot {
    let (mock, recorded) = mock_http::spawn(stt).await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.2".into());
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let jobs = 2 * aizuchi.prewarm_phrases().len();
    let manager = manager_with_aizuchi(
        &core,
        path,
        player.clone(),
        Arc::new(DryRunTransport),
        aizuchi,
    );
    manager.join(BINDING, Some("555"), None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while recorded.synthesis_started.lock().unwrap().len() < jobs {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prewarm did not finish");
    tokio::time::sleep(Duration::from_millis(100)).await;
    Bot {
        core,
        player,
        recorded,
        manager,
        _dir: dir,
    }
}

impl Bot {
    async fn speak(&mut self) -> Value {
        let task = {
            let m = self.manager.clone();
            tokio::spawn(async move { m.process_segment(20, 30, loud_pcm()).await })
        };
        let said: Value =
            serde_json::from_slice(&read_frame(&mut self.core.reader).await.unwrap()).unwrap();
        assert_eq!(said["m"], "said", "{said}");
        write_json(
            &mut self.core.writer,
            &json!({"id":said["id"],"m":"ok","seq":1}),
        )
        .await
        .unwrap();
        task.await.unwrap();
        said
    }

    fn prewarmed(&self) -> Vec<String> {
        self.recorded
            .tts_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }
}

#[tokio::test]
async fn d1101_each_bot_uses_its_own_list_and_prewarms_it() {
    let mut a = bot("今日は晴れ", custom(&["それで？"], &["ほうほう、"])).await;
    let mut b = bot("今日は晴れ", AizuchiConfig::default()).await;
    assert!(a.prewarmed().iter().any(|t| t == "ほうほう、"));
    assert!(!b.prewarmed().iter().any(|t| t == "ほうほう、"));

    let said = a.speak().await;
    assert_eq!(said["agent_prelude"], "ほうほう、", "{said}");
    assert_eq!(a.player.played.lock().unwrap().len(), 1);

    let said = b.speak().await;
    let prelude = said["agent_prelude"].as_str().unwrap();
    assert!(
        ["うん、", "へえ、", "なるほど、", "そっか、", "ああ、"].contains(&prelude),
        "{said}"
    );
}

#[tokio::test]
async fn d1101_a_disabled_bot_plays_no_aizuchi() {
    let mut a = bot(
        "今日は晴れ",
        AizuchiConfig {
            enabled: false,
            ..Default::default()
        },
    )
    .await;
    let said = a.speak().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(said.get("agent_prelude").is_none(), "{said}");
    assert_eq!(a.player.played.lock().unwrap().len(), 0);
}

//! D-1085 P4/§9: 確定した発話の後に、TTS キャッシュ済みの相づちを流す（B1–B3）と P2a の指示。
//! 流す相づちは said の agent_prelude で送り、system_context には書かない。
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::transport::DryRunTransport;
use crate::voice::aizuchi::{choose_aizuchi, AIZUCHI_QUESTION, AIZUCHI_STATEMENT, PREWARM_PHRASES};
use crate::voice::session::{VoiceManager, VOICE_SYSTEM_CONTEXT};

struct Setup {
    core: Core,
    player: Arc<FakePlayer>,
    recorded: Arc<Recorded>,
    manager: Arc<VoiceManager>,
    _dir: tempfile::TempDir,
}

async fn setup(stt: &'static str, version: bool) -> Setup {
    let (mock, recorded) = mock_http::spawn(stt).await;
    if version {
        *recorded.engine_version.lock().unwrap() = Some("0.25.2".into());
    }
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let manager = manager_with(&core, path, player.clone(), Arc::new(DryRunTransport));
    manager.join(BINDING, Some("555"), None).await.unwrap();
    if version {
        tokio::time::timeout(Duration::from_secs(5), async {
            while recorded.synthesis_started.lock().unwrap().len() < 2 * PREWARM_PHRASES.len() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prewarm did not finish");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Setup {
        core,
        player,
        recorded,
        manager,
        _dir: dir,
    }
}

impl Setup {
    /// 話者 30 の発話を said にし、core 側で ok を返す。said を返す。
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

    fn synth_count(&self) -> usize {
        self.recorded.synthesis_started.lock().unwrap().len()
    }

    fn played_count(&self) -> usize {
        self.player.played.lock().unwrap().len()
    }
}

/// 相づちの指示は system_context に入れない（会話履歴の agent_prelude で伝える）。
fn assert_plain_voice_context(said: &Value) {
    assert_eq!(said["system_context"], VOICE_SYSTEM_CONTEXT, "{said}");
}

#[test]
fn question_and_statement_endings_choose_from_their_own_sets() {
    for pick in 0..10 {
        let q = choose_aizuchi("元気ですか", None, pick).unwrap();
        assert!(AIZUCHI_QUESTION.contains(&q), "{q}");
        let q = choose_aizuchi("何を食べたの？", None, pick).unwrap();
        assert!(AIZUCHI_QUESTION.contains(&q), "{q}");
        let s = choose_aizuchi("今日は晴れ。", None, pick).unwrap();
        assert!(AIZUCHI_STATEMENT.contains(&s), "{s}");
    }
    assert_eq!(choose_aizuchi("あ", None, 0), None);
    assert_eq!(choose_aizuchi(" あ。", None, 3), None);
}

#[test]
fn the_previous_aizuchi_is_never_chosen_again() {
    for set in [AIZUCHI_QUESTION, AIZUCHI_STATEMENT] {
        let text = if set == AIZUCHI_QUESTION {
            "元気ですか"
        } else {
            "今日は晴れ"
        };
        for previous in set {
            for pick in 0..10 {
                let next = choose_aizuchi(text, Some(previous), pick).unwrap();
                assert_ne!(next, *previous, "pick {pick}");
            }
        }
    }
}

#[test]
fn every_aizuchi_candidate_is_prewarmed() {
    for candidate in AIZUCHI_QUESTION.iter().chain(AIZUCHI_STATEMENT) {
        assert!(PREWARM_PHRASES.contains(candidate), "{candidate}");
    }
}

#[test]
fn p2a_voice_context_asks_for_a_very_short_first_sentence() {
    assert!(
        VOICE_SYSTEM_CONTEXT.contains("最初の1文はとても短く（数文字〜10文字）"),
        "{VOICE_SYSTEM_CONTEXT}"
    );
}

#[tokio::test]
async fn b1_played_aizuchi_is_sent_as_the_agent_prelude_from_the_cache() {
    let mut s = setup("こんにちは", true).await;
    let before = s.synth_count();
    let said = s.speak().await;
    assert_plain_voice_context(&said);
    let prelude = said["agent_prelude"]
        .as_str()
        .unwrap_or_else(|| panic!("{said}"));
    assert!(AIZUCHI_STATEMENT.contains(&prelude), "{said}");
    assert_eq!(s.played_count(), 1);
    assert_eq!(
        s.synth_count(),
        before,
        "the aizuchi must come from the cache"
    );
}

#[tokio::test]
async fn b2_no_aizuchi_and_no_prelude_while_the_player_is_busy() {
    let mut s = setup("こんにちは", true).await;
    s.player.busy.store(true, Ordering::SeqCst);
    let said = s.speak().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.played_count(), 0);
    assert!(said.get("agent_prelude").is_none(), "{said}");
    assert_plain_voice_context(&said);
}

#[tokio::test]
async fn b3_question_ending_sends_a_question_aizuchi() {
    let mut s = setup("元気ですか", true).await;
    let said = s.speak().await;
    let prelude = said["agent_prelude"]
        .as_str()
        .unwrap_or_else(|| panic!("{said}"));
    assert!(AIZUCHI_QUESTION.contains(&prelude), "{said}");
    assert_plain_voice_context(&said);
    assert_eq!(s.played_count(), 1);
}

#[tokio::test]
async fn consecutive_turns_in_a_channel_never_repeat_the_aizuchi() {
    let mut s = setup("こんにちは", true).await;
    let mut previous: Option<String> = None;
    for turn in 0..6 {
        let said = s.speak().await;
        let prelude = said["agent_prelude"].as_str().unwrap().to_string();
        assert_ne!(Some(&prelude), previous.as_ref(), "turn {turn}");
        previous = Some(prelude);
    }
    assert_eq!(s.played_count(), 6);
}

#[tokio::test]
async fn uncached_aizuchi_is_skipped_without_a_prelude_and_never_synthesized_live() {
    let mut s = setup("こんにちは", false).await;
    let said = s.speak().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.played_count(), 0);
    assert!(said.get("agent_prelude").is_none(), "{said}");
    assert_plain_voice_context(&said);
    assert_eq!(s.synth_count(), 0);
}

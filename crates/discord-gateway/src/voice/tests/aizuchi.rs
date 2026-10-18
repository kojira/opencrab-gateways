//! D-1085 P4: 確定した発話の後に、TTS キャッシュ済みの相づちを流す（B1–B3）と P2a の指示。
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::transport::DryRunTransport;
use crate::voice::aizuchi::{choose_aizuchi, PREWARM_PHRASES};
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
    /// 話者 30 の発話を said にし、core 側で ok を返す。said の system_context を返す。
    async fn speak(&mut self) -> String {
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
        said["system_context"].as_str().unwrap().to_string()
    }

    fn synth_count(&self) -> usize {
        self.recorded.synthesis_started.lock().unwrap().len()
    }
}

#[test]
fn aizuchi_is_chosen_from_the_utterance_ending() {
    assert_eq!(choose_aizuchi("元気ですか"), Some("えっと、"));
    assert_eq!(choose_aizuchi("何を食べたの？"), Some("えっと、"));
    assert_eq!(choose_aizuchi("今日は晴れ。"), Some("うん、"));
    assert_eq!(choose_aizuchi("あ"), None);
    assert_eq!(choose_aizuchi(" あ。"), None);
}

#[test]
fn p2a_voice_context_asks_for_a_very_short_first_sentence() {
    assert!(
        VOICE_SYSTEM_CONTEXT.contains("最初の1文はとても短く（数文字〜10文字）"),
        "{VOICE_SYSTEM_CONTEXT}"
    );
}

#[tokio::test]
async fn b1_cached_aizuchi_is_played_after_the_said_without_synthesis() {
    let mut s = setup("こんにちは", true).await;
    let before = s.synth_count();
    let context = s.speak().await;
    assert!(context.starts_with(VOICE_SYSTEM_CONTEXT), "{context}");
    assert!(context.contains("すでに『うん、』と言った"), "{context}");
    assert_eq!(s.player.played.lock().unwrap().len(), 1);
    assert_eq!(
        s.synth_count(),
        before,
        "the aizuchi must come from the cache"
    );
}

#[tokio::test]
async fn b2_no_aizuchi_while_the_player_is_busy() {
    let mut s = setup("こんにちは", true).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.speak().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(s.player.played.lock().unwrap().is_empty());
}

#[tokio::test]
async fn b3_question_ending_chooses_etto_in_the_system_context() {
    let mut s = setup("元気ですか", true).await;
    let context = s.speak().await;
    assert!(context.contains("すでに『えっと、』と言った"), "{context}");
    assert_eq!(s.player.played.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn uncached_aizuchi_is_skipped_and_never_synthesized_live() {
    let mut s = setup("こんにちは", false).await;
    s.speak().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(s.player.played.lock().unwrap().is_empty());
    assert_eq!(s.synth_count(), 0);
}

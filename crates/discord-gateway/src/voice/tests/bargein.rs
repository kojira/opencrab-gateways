//! D-1096: VC バージイン。ボットが話している間に、誰かの部分認識が 5 文字に達したら、再生を止め、
//! 積んだ文とこの後に届く同じ stream の文を読まず、core へ `interrupt`（相手に届いた分）を送る。
//! mock core（UDS）・実際の say consumer・mock STT/VOICEVOX を通す。
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::config::SystemReactions;
use crate::ops::SessionDeliveryTargets;
use crate::transport::testfake::RecordingTransport;
use crate::voice::audio::{EndVerdict, SegmenterEvent};
use crate::voice::session::VoiceManager;

const STREAM: &str = "5f0d2a3e-3333-4c1c-9c1c-000000000003";

struct Setup {
    core: Core,
    player: Arc<FakePlayer>,
    recorded: Arc<Recorded>,
    transport: Arc<RecordingTransport>,
    manager: Arc<VoiceManager>,
    consumer: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.consumer.abort();
    }
}

/// `synthesis_delay_ms` で合成を遅らせると、読み上げ待ちの文が溜まった状態を作れる。
async fn setup(stt: Vec<(u64, &'static str)>, synthesis_delay_ms: u64) -> Setup {
    let (mock, recorded) = mock_http::spawn_with_synthesis_delay(stt, synthesis_delay_ms).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let transport = Arc::new(RecordingTransport::default());
    let manager = manager_with(&core, path, player.clone(), transport.clone());
    manager.join(BINDING, Some("555"), None).await.unwrap();
    let targets: SessionDeliveryTargets =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let consumer = crate::run::spawn_say_consumer(
        core.client.clone(),
        core.address.clone(),
        transport.clone(),
        "agent".into(),
        SystemReactions::default(),
        targets,
        Some(manager.clone()),
        None,
    );
    Setup {
        core,
        player,
        recorded,
        transport,
        manager,
        consumer,
        _dir: dir,
    }
}

fn snapshot(seq: u64) -> SegmenterEvent {
    SegmenterEvent::Snapshot {
        utt: 0,
        seq,
        pcm: loud_pcm(),
        onset: false,
        verdict: EndVerdict::default(),
    }
}

impl Setup {
    async fn send(&mut self, frame: Value) {
        write_json(&mut self.core.writer, &frame).await.unwrap();
    }

    async fn partial(&mut self, text: &str) {
        self.send(json!({"m":"partial","binding_id":BINDING,"stream_id":STREAM,"text":text}))
            .await;
    }

    async fn say(&mut self, id: &str, text: &str) {
        self.say_on(id, text, STREAM).await;
    }

    /// core の say。gateway の ok を待つ。間に来る provisional には ok を返して読み飛ばす。
    async fn say_on(&mut self, id: &str, text: &str, stream_id: &str) {
        self.send(json!({"id":id,"m":"say","binding_id":BINDING,
                         "payload":{"text":text,"stream_id":stream_id}}))
            .await;
        loop {
            let frame = self.next_frame().await;
            if frame["m"] == "ok" {
                assert_eq!(frame["id"], id, "{frame}");
                return;
            }
            assert!(
                matches!(
                    frame["m"].as_str(),
                    Some("provisional" | "provisional_cancel")
                ),
                "unexpected request: {frame}"
            );
            write_json(
                &mut self.core.writer,
                &json!({"id": frame["id"], "m": "ok"}),
            )
            .await
            .unwrap();
        }
    }

    async fn next_frame(&mut self) -> Value {
        serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(3), read_frame(&mut self.core.reader))
                .await
                .expect("no frame from the gateway")
                .unwrap(),
        )
        .unwrap()
    }

    /// 次の要求を読み、ok を返す。provisional は `interrupt` の前後どちらにも来うるので読み飛ばす。
    async fn next_interrupt(&mut self) -> Value {
        loop {
            let frame = self.next_frame().await;
            write_json(
                &mut self.core.writer,
                &json!({"id": frame["id"], "m": "ok"}),
            )
            .await
            .unwrap();
            if frame["m"] == "interrupt" {
                return frame;
            }
            assert!(
                matches!(
                    frame["m"].as_str(),
                    Some("provisional" | "provisional_cancel")
                ),
                "unexpected request: {frame}"
            );
        }
    }

    /// `interrupt` が来ないことを確かめる（provisional は ok を返して読み飛ばす）。
    async fn assert_no_interrupt(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        loop {
            let next = tokio::time::timeout_at(deadline, read_frame(&mut self.core.reader)).await;
            let Ok(frame) = next else {
                return;
            };
            let frame: Value = serde_json::from_slice(&frame.unwrap()).unwrap();
            assert_ne!(frame["m"], "interrupt", "unexpected interrupt: {frame}");
            write_json(
                &mut self.core.writer,
                &json!({"id": frame["id"], "m": "ok"}),
            )
            .await
            .unwrap();
        }
    }

    fn speak(&self, user: u64, seq: u64) {
        self.manager.enqueue_speech_event(20, user, snapshot(seq));
    }

    /// 再生へ渡した文（VOICEVOX が受けた本文のうち、実際に `play` された数だけ）。
    fn played_texts(&self) -> Vec<String> {
        let played = self.player.played.lock().unwrap().len();
        self.recorded
            .tts_queries
            .lock()
            .unwrap()
            .iter()
            .take(played)
            .map(|(text, _)| text.clone())
            .collect()
    }

    fn queried(&self) -> Vec<String> {
        self.recorded
            .tts_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }

    async fn wait_played(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.player.played.lock().unwrap().len() < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("TTS playback did not happen");
    }

    /// 最も古い読み上げの文が最後まで流れたことにする（songbird のトラック終了に当たる）。
    fn finish_track(&self) {
        self.player.finish_track();
    }
}

#[tokio::test]
async fn g1_a_long_reply_is_spoken_in_full_without_the_cap_suffix() {
    let mut s = setup(vec![(0, "unused")], 0).await;
    // 40 文字の文を 10 個（400 文字）。旧上限（300 文字）を超える。
    let sentence = format!("{}。", "あ".repeat(39));
    let text = sentence.repeat(10);
    s.say("s1", &text).await;
    s.wait_played(10).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.queried(), vec![sentence; 10]);
    assert!(s.queried().iter().all(|t| !t.contains("以下省略")));
    assert_eq!(s.transport.bodies().concat(), text);
}

#[tokio::test]
async fn g6_speech_while_the_bot_plays_sends_one_interrupt_with_what_was_heard() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    s.partial("一文目です。二文目です。").await;
    s.wait_played(2).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.finish_track(); // 1 文目は最後まで流れた。2 文目は流れている途中。

    s.speak(30, 0);
    let interrupt = s.next_interrupt().await;
    assert_eq!(interrupt["binding_id"], BINDING, "{interrupt}");
    assert_eq!(interrupt["delivered"], "一文目です。", "{interrupt}");
    assert_eq!(*s.player.stopped.lock().unwrap(), vec![20u64]);

    // 同じ話者が話し続けても、2 回目は送らない。
    s.speak(30, 1);
    s.assert_no_interrupt().await;
}

#[tokio::test]
async fn g6_anyone_in_the_vc_can_barge_in_but_not_the_bot_itself() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    s.partial("一文目です。").await;
    s.wait_played(1).await;
    s.player.busy.store(true, Ordering::SeqCst);

    // ボット自身の声では止めない。
    s.speak(999, 0);
    s.assert_no_interrupt().await;
    // オーナーでも信頼済みでもない人でも止められる。
    s.speak(77, 0);
    let interrupt = s.next_interrupt().await;
    assert_eq!(interrupt["delivered"], "", "{interrupt}");
}

#[tokio::test]
async fn g6_no_interrupt_while_the_bot_is_silent() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    s.speak(30, 0);
    s.assert_no_interrupt().await;
    assert!(s.player.stopped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn g6_a_short_partial_does_not_barge_in() {
    let mut s = setup(vec![(0, "えっ")], 0).await;
    s.partial("一文目です。").await;
    s.wait_played(1).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.speak(30, 0);
    s.assert_no_interrupt().await;
    assert!(s.player.stopped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn g2_g3_queued_sentences_are_dropped_after_the_interrupt() {
    // 合成を遅らせ、1 文目の再生中に 2〜4 文目が読み上げ待ちになっている状態を作る。
    let mut s = setup(vec![(0, "ちょっと待って")], 150).await;
    s.partial("一文目です。二文目です。三文目です。四文目です。")
        .await;
    s.wait_played(1).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.speak(30, 0);
    s.next_interrupt().await;
    s.player.busy.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(s.played_texts(), vec!["一文目です。".to_string()]);
    assert_eq!(*s.player.stopped.lock().unwrap(), vec![20u64]);
}

#[tokio::test]
async fn g4_later_partials_and_the_final_say_of_the_cut_stream_are_not_spoken() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    s.partial("一文目です。").await;
    s.wait_played(1).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.speak(30, 0);
    s.next_interrupt().await;
    s.player.busy.store(false, Ordering::SeqCst);

    s.partial("一文目です。二文目です。").await;
    s.say("s1", "一文目です。二文目です。三文目です。").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.played_texts(), vec!["一文目です。".to_string()]);
    // 文字投稿は今のまま全文。
    assert_eq!(
        s.transport.bodies(),
        vec!["一文目です。二文目です。三文目です。".to_string()]
    );

    // 次の stream は普通に読む。
    s.say_on(
        "s2",
        "次の返事です。",
        "5f0d2a3e-3333-4c1c-9c1c-000000000004",
    )
    .await;
    s.wait_played(2).await;
    assert_eq!(s.played_texts().last().unwrap(), "次の返事です。");
}

#[tokio::test]
async fn g5_delivered_is_only_the_sentences_whose_playback_finished() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    s.partial("一文目です。二文目です。三文目です。").await;
    s.wait_played(3).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.finish_track();
    s.finish_track(); // 1・2 文目は流れ終わり、3 文目は途中。
    s.speak(30, 0);
    let interrupt = s.next_interrupt().await;
    assert_eq!(
        interrupt["delivered"], "一文目です。二文目です。",
        "{interrupt}"
    );
}

#[tokio::test]
async fn g5_barge_in_during_an_aizuchi_after_a_finished_reply_delivers_nothing() {
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    // 返事 1 は最後まで流れた。
    s.partial("一文目です。").await;
    s.wait_played(1).await;
    s.finish_track();
    // 相づちは読み上げキューを通らず player で直接鳴る。読み上げ待ちは無く、player だけが鳴っている。
    s.player.busy.store(true, Ordering::SeqCst);

    s.speak(30, 0);
    let interrupt = s.next_interrupt().await;
    assert_eq!(interrupt["delivered"], "", "{interrupt}");
    s.player.busy.store(false, Ordering::SeqCst);

    // 返事 1 の stream は止めた扱いにしない。この後に届くその stream の文は読む。
    s.say("s1", "一文目です。続きです。").await;
    s.wait_played(2).await;
    assert_eq!(s.played_texts().last().unwrap(), "続きです。");
}

#[tokio::test]
async fn g5_a_reply_queued_behind_the_playing_one_does_not_erase_what_was_heard() {
    const B: &str = "5f0d2a3e-3333-4c1c-9c1c-000000000005";
    const C: &str = "5f0d2a3e-3333-4c1c-9c1c-000000000006";
    let mut s = setup(vec![(0, "ちょっと待って")], 0).await;
    // 返事 A: 1・2 文目は流れ終わり、3 文目は流れている途中。
    s.partial("一文目です。二文目です。三文目です。").await;
    s.wait_played(3).await;
    s.player.busy.store(true, Ordering::SeqCst);
    s.finish_track();
    s.finish_track();
    // 返事 B が A の後ろに積まれる（22:23:42 の「ん、どうした？」）。
    s.say_on("b1", "ん、どうした？", B).await;
    s.wait_played(4).await;

    s.speak(30, 0);
    let interrupt = s.next_interrupt().await;
    assert_eq!(
        interrupt["delivered"], "一文目です。二文目です。",
        "{interrupt}"
    );
    s.player.busy.store(false, Ordering::SeqCst);

    // A も B も止めた扱い。この後に届く partial・say は読まない。
    s.partial("一文目です。二文目です。三文目です。四文目です。")
        .await;
    s.say("a1", "一文目です。二文目です。三文目です。四文目です。")
        .await;
    s.say_on("b2", "ん、どうした？大丈夫？", B).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(s.played_texts().len(), 4, "{:?}", s.played_texts());

    // 次の stream は普通に読む。
    s.say_on("c1", "次の返事です。", C).await;
    s.wait_played(5).await;
    assert_eq!(s.played_texts().last().unwrap(), "次の返事です。");
}

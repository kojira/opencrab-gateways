//! D-1085 P5（§10・§11）: 話している間の先行生成（provisional）と、その返事の保留・昇格。
//! 区切りの出来事を VoiceManager の話者キューへ積み、mock core（UDS）・実際の say consumer・
//! mock STT/VOICEVOX を通す。
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::config::SystemReactions;
use crate::ops::SessionDeliveryTargets;
use crate::transport::testfake::RecordingTransport;
use crate::voice::aizuchi::{AIZUCHI_STATEMENT, PREWARM_PHRASES};
use crate::voice::audio::{EndReason, EndVerdict, SegmenterEvent};
use crate::voice::session::VoiceManager;

const STREAM: &str = "5f0d2a3e-2222-4c1c-9c1c-000000000002";

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

async fn setup(stt: Vec<(u64, &'static str)>) -> Setup {
    let (mock, recorded) = mock_http::spawn_sequenced(stt).await;
    *recorded.engine_version.lock().unwrap() = Some("0.25.2".into());
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let transport = Arc::new(RecordingTransport::default());
    let manager = manager_with(&core, path, player.clone(), transport.clone());
    manager.join(BINDING, Some("555"), None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while recorded.synthesis_started.lock().unwrap().len() < 2 * PREWARM_PHRASES.len() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prewarm did not finish");
    tokio::time::sleep(Duration::from_millis(100)).await;
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

fn snapshot(seq: u64, onset: bool) -> SegmenterEvent {
    SegmenterEvent::Snapshot {
        utt: 0,
        seq,
        pcm: loud_pcm(),
        onset,
        verdict: EndVerdict::default(),
    }
}

fn end(onset_seq: Option<u64>) -> SegmenterEvent {
    SegmenterEvent::End {
        utt: 0,
        pcm: loud_pcm(),
        onset_seq,
        reason: EndReason::Candidate,
        quiet_packets: 0,
    }
}

impl Setup {
    fn event(&self, event: SegmenterEvent) {
        self.manager.enqueue_speech_event(20, 30, event);
    }

    /// gateway が送った次の要求を読み、ok を返す。
    async fn ack_next(&mut self) -> Value {
        let frame: Value = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(3), read_frame(&mut self.core.reader))
                .await
                .expect("no request from the gateway")
                .unwrap(),
        )
        .unwrap();
        // said の ok は seq を伴い、provisional / provisional_cancel の ok は伴わない。
        let ok = if frame["m"] == "said" {
            json!({"id": frame["id"], "m": "ok", "seq": 1})
        } else {
            json!({"id": frame["id"], "m": "ok"})
        };
        write_json(&mut self.core.writer, &ok).await.unwrap();
        frame
    }

    /// 次の要求が無いことを確かめる。
    async fn assert_no_request(&mut self) {
        let next = tokio::time::timeout(
            Duration::from_millis(300),
            read_frame(&mut self.core.reader),
        )
        .await;
        assert!(next.is_err(), "unexpected request: {next:?}");
    }

    async fn send(&mut self, frame: Value) {
        write_json(&mut self.core.writer, &frame).await.unwrap();
    }

    async fn provisional_partial(&mut self, provisional_id: &Value, text: &str) {
        self.send(
            json!({"m":"partial","binding_id":BINDING,"stream_id":STREAM,"text":text,
                         "provisional_id":provisional_id}),
        )
        .await;
    }

    async fn say(&mut self, id: &str, text: &str) {
        self.send(json!({"id":id,"m":"say","binding_id":BINDING,
                         "payload":{"text":text,"stream_id":STREAM}}))
            .await;
        let ok: Value =
            serde_json::from_slice(&read_frame(&mut self.core.reader).await.unwrap()).unwrap();
        assert_eq!(ok["id"], id, "{ok}");
        assert_eq!(ok["m"], "ok", "{ok}");
    }

    fn stt_count(&self) -> usize {
        self.recorded.stt_bodies.lock().unwrap().len()
    }

    fn synth_count(&self) -> usize {
        self.recorded.synthesis_started.lock().unwrap().len()
    }

    fn played_count(&self) -> usize {
        self.player.played.lock().unwrap().len()
    }

    /// 合成のために VOICEVOX へ渡した本文（先読みを除く）。
    fn spoken_queries(&self) -> Vec<String> {
        self.recorded
            .tts_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .filter(|text| !PREWARM_PHRASES.contains(&text.as_str()))
            .collect()
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn a_provisional_starts_at_five_chars_with_the_aizuchi_of_that_text() {
    let mut s = setup(vec![(0, "今日は"), (0, "今日はいい天気")]).await;
    s.event(snapshot(0, false));
    s.assert_no_request().await;
    s.event(snapshot(1, false));
    let provisional = s.ack_next().await;
    assert_eq!(provisional["m"], "provisional", "{provisional}");
    assert_eq!(provisional["text"], "（音声）今日はいい天気");
    assert_eq!(provisional["model_profile"], "realtime");
    let prelude = provisional["agent_prelude"].as_str().unwrap();
    assert!(AIZUCHI_STATEMENT.contains(&prelude), "{provisional}");
    assert_eq!(s.played_count(), 0, "nothing plays while the user speaks");
}

#[tokio::test]
async fn growth_of_eight_chars_replaces_the_provisional_and_cancels_the_old_one() {
    // G-S3: 置き換えた古い先行生成は中止し、その partial は保留も再生もしない。
    let mut s = setup(vec![
        (0, "今日はいい天気"),
        (0, "今日はいい天気ですね"),
        (0, "今日はいい天気ですね明日も晴れるかな"),
    ])
    .await;
    s.event(snapshot(0, false));
    let first = s.ack_next().await;
    assert_eq!(first["m"], "provisional");
    s.event(snapshot(1, false));
    s.assert_no_request().await; // +3 文字では置き換えない。
    s.event(snapshot(2, false));
    let cancel = s.ack_next().await;
    assert_eq!(cancel["m"], "provisional_cancel", "{cancel}");
    assert_eq!(cancel["provisional_id"], first["provisional_id"]);
    let second = s.ack_next().await;
    assert_eq!(second["m"], "provisional", "{second}");
    assert_eq!(
        second["text"],
        "（音声）今日はいい天気ですね明日も晴れるかな"
    );
    assert_ne!(second["provisional_id"], first["provisional_id"]);

    let synth_before = s.synth_count();
    s.provisional_partial(&first["provisional_id"], "はい。")
        .await;
    s.settle().await;
    assert_eq!(s.played_count(), 0);
    assert_eq!(
        s.synth_count(),
        synth_before,
        "a cancelled run is not synthesized"
    );
}

#[tokio::test]
async fn g_s1_s2_held_reply_plays_after_the_aizuchi_on_promotion() {
    let mut s = setup(vec![(0, "今日はいい天気ですね")]).await;
    s.event(snapshot(0, true));
    let provisional = s.ack_next().await;
    assert_eq!(provisional["m"], "provisional");
    let id = provisional["provisional_id"].clone();

    // G-S1: 先行生成の partial は合成だけして再生しない。
    s.provisional_partial(&id, "はい。").await;
    s.settle().await;
    assert_eq!(s.played_count(), 0, "held speech must not play");
    assert_eq!(s.spoken_queries(), vec!["はい。".to_string()]);
    let synth_after_hold = s.synth_count();

    // 確定: 話し終わり候補の本文をそのまま使う（STT し直さない）。
    s.event(end(Some(0)));
    let said = s.ack_next().await;
    assert_eq!(said["m"], "said", "{said}");
    assert_eq!(said["text"], "（音声）今日はいい天気ですね");
    assert_eq!(said["provisional_id"], id);
    assert_eq!(said["agent_prelude"], provisional["agent_prelude"]);
    assert_eq!(s.stt_count(), 1, "the commit reuses the onset transcript");

    // G-S2: 昇格したら相づち、保留した文の順に流し、最終 say は残りだけを読む。
    s.send(
        json!({"m":"provisional_result","binding_id":BINDING,"provisional_id":id,
                  "promoted":true}),
    )
    .await;
    s.provisional_partial(&id, "はい。いい天気ですね。").await;
    s.settle().await;
    // 最終 say を待たずに、保留した文（キャッシュ）と続きの文が流れる。
    assert_eq!(
        s.played_count(),
        3,
        "aizuchi + held + next sentence before the say"
    );
    assert_eq!(
        s.synth_count(),
        synth_after_hold + 1,
        "the held sentence plays from the cache"
    );
    s.say("s1", "はい。いい天気ですね。散歩日和です。").await;
    s.settle().await;
    assert_eq!(
        s.spoken_queries(),
        vec![
            "はい。".to_string(),
            "いい天気ですね。".to_string(),
            "散歩日和です。".to_string()
        ]
    );
    assert_eq!(
        s.played_count(),
        4,
        "aizuchi + three sentences, each played once"
    );
    assert_eq!(s.synth_count(), synth_after_hold + 2);
    assert_eq!(
        s.transport.bodies(),
        vec!["はい。いい天気ですね。散歩日和です。".to_string()]
    );
}

#[tokio::test]
async fn a_changed_onset_transcript_replaces_and_an_unpromoted_hold_is_dropped() {
    let mut s = setup(vec![(0, "今日はいい天気"), (0, "今日はいい天気だ")]).await;
    s.event(snapshot(0, false));
    let first = s.ack_next().await;
    assert_eq!(first["m"], "provisional");
    // 無音 100ms の全体認識で本文が変わったら（+1 文字でも）置き換える。
    s.event(snapshot(1, true));
    assert_eq!(s.ack_next().await["m"], "provisional_cancel");
    let second = s.ack_next().await;
    assert_eq!(second["text"], "（音声）今日はいい天気だ");
    let id = second["provisional_id"].clone();
    s.provisional_partial(&id, "そうだね。").await;
    s.event(end(Some(1)));
    let said = s.ack_next().await;
    assert_eq!(said["provisional_id"], id);
    s.settle().await;
    // provisional_id 付きの said では、受理しても相づちを流さない（昇格の知らせで流す）。
    assert_eq!(s.played_count(), 0, "the said ok must not play the aizuchi");

    // 昇格しなければ相づちだけ流して保留を捨て、通常のターンの partial と say で読む。
    s.send(
        json!({"m":"provisional_result","binding_id":BINDING,"provisional_id":id,
                  "promoted":false}),
    )
    .await;
    s.settle().await;
    assert_eq!(s.played_count(), 1, "the aizuchi plays on the result");
    s.say("s1", "うん、晴れだね。").await;
    s.settle().await;
    assert_eq!(s.played_count(), 2);
}

#[tokio::test]
async fn only_the_promotion_result_plays_the_aizuchi_then_the_held_speech() {
    // said の ok（話者キュー）と昇格の知らせ（live ループ）は並行に届く。相づちを流すのは昇格の
    // 知らせの側だけにして、読み上げ待ちで飛ばされたり保留した文と順が入れ替わったりしないようにする。
    let mut s = setup(vec![(0, "今日はいい天気ですね")]).await;
    s.event(snapshot(0, true));
    let provisional = s.ack_next().await;
    let id = provisional["provisional_id"].clone();
    assert!(provisional["agent_prelude"].is_string(), "{provisional}");
    s.provisional_partial(&id, "はい。").await;
    s.settle().await;
    s.event(end(Some(0)));
    let said = s.ack_next().await;
    assert_eq!(said["m"], "said", "{said}");
    s.settle().await;
    assert_eq!(s.played_count(), 0, "the said ok must not play the aizuchi");
    s.send(
        json!({"m":"provisional_result","binding_id":BINDING,"provisional_id":id,
                  "promoted":true}),
    )
    .await;
    s.settle().await;
    assert_eq!(s.played_count(), 2, "aizuchi once, then the held sentence");
}

#[tokio::test]
async fn a_rejected_said_cancels_its_provisional() {
    let mut s = setup(vec![(0, "今日はいい天気ですね")]).await;
    s.event(snapshot(0, true));
    let provisional = s.ack_next().await;
    let id = provisional["provisional_id"].clone();
    s.event(end(Some(0)));
    let said: Value = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(3), read_frame(&mut s.core.reader))
            .await
            .expect("no said")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(said["m"], "said", "{said}");
    s.send(json!({"id": said["id"], "m": "err", "code": "bad_request", "detail": null}))
        .await;
    let next = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut s.core.reader)).await;
    assert!(
        next.is_ok(),
        "no provisional_cancel after the rejected said"
    );
    let cancel: Value = serde_json::from_slice(&next.unwrap().unwrap()).unwrap();
    write_json(&mut s.core.writer, &json!({"id": cancel["id"], "m": "ok"}))
        .await
        .unwrap();
    assert_eq!(cancel["m"], "provisional_cancel", "{cancel}");
    assert_eq!(cancel["provisional_id"], id);
    s.settle().await;
    assert_eq!(s.played_count(), 0);
}

//! D-1083: core の partial（生成途中の完成した文）を VC で先に読み上げ、最終 say では残りだけを読む。
//! mock core（UDS）から partial / say を流し、実際の say consumer と VoiceManager を通す。
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, manager_with, wait_played, Core, FakePlayer, BINDING};
use super::mock_http::{self, Recorded};
use crate::config::SystemReactions;
use crate::ops::BindingDeliveryTargets;
use crate::transport::testfake::RecordingTransport;
use crate::voice::session::VoiceManager;

const STREAM: &str = "5f0d2a3e-1111-4c1c-9c1c-000000000001";

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

async fn setup(join: bool) -> Setup {
    let (mock, recorded) = mock_http::spawn("unused").await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let transport = Arc::new(RecordingTransport::default());
    let manager = manager_with(&core, path, player.clone(), transport.clone());
    if join {
        manager.join(BINDING, Some("555"), None).await.unwrap();
    }
    let targets: BindingDeliveryTargets =
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

impl Setup {
    /// core の partial 通知（id 無し・応答しない）。
    async fn partial(&mut self, stream_id: &str, text: &str) {
        write_json(
            &mut self.core.writer,
            &json!({"m":"partial","binding_id":BINDING,"stream_id":stream_id,"text":text}),
        )
        .await
        .unwrap();
    }

    /// core の say。`stream_id` があれば payload に載せる。gateway の ok を待つ。
    async fn say(&mut self, id: &str, text: &str, stream_id: Option<&str>) {
        let mut payload = json!({ "text": text });
        if let Some(stream_id) = stream_id {
            payload["stream_id"] = json!(stream_id);
        }
        write_json(
            &mut self.core.writer,
            &json!({"id":id,"m":"say","binding_id":BINDING,"payload":payload}),
        )
        .await
        .unwrap();
        let ok: Value =
            serde_json::from_slice(&read_frame(&mut self.core.reader).await.unwrap()).unwrap();
        assert_eq!(ok["id"], id, "{ok}");
        assert_eq!(ok["m"], "ok", "{ok}");
    }

    /// 投稿が `n` 件になるまで待つ（say の ok は投稿の前に返る）。
    async fn wait_posts(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.transport.bodies().len() < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("say was not posted");
    }

    /// 合成した文（VOICEVOX へ渡した本文）を、それ以上増えないのを確かめてから返す。
    async fn settled_speech(&self) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.recorded
            .tts_queries
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn g_a_partial_sentence_is_spoken_before_the_final_say() {
    let mut s = setup(true).await;
    s.partial(STREAM, "一文目です。").await;
    wait_played(&s.player, 1).await;
    assert!(s.transport.bodies().is_empty(), "no say was posted yet");
    assert_eq!(s.settled_speech().await, strings(&["一文目です。"]));

    s.say("s1", "一文目です。", Some(STREAM)).await;
    s.wait_posts(1).await;
    assert_eq!(s.transport.bodies(), strings(&["一文目です。"]));
    assert_eq!(s.settled_speech().await, strings(&["一文目です。"]));
}

#[tokio::test]
async fn g_b_final_say_speaks_only_the_remainder_and_posts_the_full_text() {
    let mut s = setup(true).await;
    s.partial(STREAM, "一文目です。").await;
    s.partial(STREAM, "一文目です。二文目です。").await;
    wait_played(&s.player, 2).await;
    assert!(s.transport.bodies().is_empty(), "no say was posted yet");

    s.say("s1", "一文目です。二文目です。三文目です。", Some(STREAM))
        .await;
    s.wait_posts(1).await;
    assert_eq!(
        s.transport.bodies(),
        strings(&["一文目です。二文目です。三文目です。"])
    );
    assert_eq!(
        s.settled_speech().await,
        strings(&["一文目です。", "二文目です。", "三文目です。"])
    );
}

#[tokio::test]
async fn g_c_changed_final_sentence_is_spoken_in_its_new_form() {
    let mut s = setup(true).await;
    s.partial(STREAM, "一文目です。二文目です。").await;
    wait_played(&s.player, 2).await;

    s.say("s1", "一文目です。二文目でした。三文目です。", Some(STREAM))
        .await;
    s.wait_posts(1).await;
    assert_eq!(
        s.transport.bodies(),
        strings(&["一文目です。二文目でした。三文目です。"])
    );
    assert_eq!(
        s.settled_speech().await,
        strings(&[
            "一文目です。",
            "二文目です。",
            "二文目でした。",
            "三文目です。"
        ])
    );
}

#[tokio::test]
async fn say_without_a_stream_id_is_spoken_whole_as_today() {
    let mut s = setup(true).await;
    s.partial(STREAM, "一文目です。").await;
    wait_played(&s.player, 1).await;

    // stream_id 無しの say は従来どおり全文を読む。
    s.say("s1", "一文目です。二文目です。", None).await;
    s.wait_posts(1).await;
    assert_eq!(
        s.settled_speech().await,
        strings(&["一文目です。", "一文目です。", "二文目です。"])
    );
}

#[tokio::test]
async fn g_d_partial_without_a_voice_session_is_ignored() {
    let mut s = setup(false).await;
    s.partial(STREAM, "一文目です。").await;
    s.say("s1", "一文目です。二文目です。", Some(STREAM)).await;
    s.wait_posts(1).await;
    assert_eq!(s.transport.bodies(), strings(&["一文目です。二文目です。"]));
    assert!(s.settled_speech().await.is_empty());
    assert!(s.player.played.lock().unwrap().is_empty());

    // セッション無しの partial は状態を残さない。後から入っても同じ stream の say は全文を読む。
    s.partial(STREAM, "三文目です。").await;
    s.manager.join(BINDING, Some("555"), None).await.unwrap();
    s.say("s2", "三文目です。四文目です。", Some(STREAM)).await;
    s.wait_posts(2).await;
    assert_eq!(
        s.settled_speech().await,
        strings(&["三文目です。", "四文目です。"])
    );
}

#[tokio::test]
async fn g_e_leave_drops_the_draft_and_later_partials_are_ignored() {
    let mut s = setup(true).await;
    s.partial(STREAM, "一文目です。").await;
    wait_played(&s.player, 1).await;
    s.manager.leave(BINDING).await.unwrap();

    s.partial(STREAM, "一文目です。二文目です。").await;
    assert_eq!(s.settled_speech().await, strings(&["一文目です。"]));

    // leave で下書きは消えている。入り直した後の同じ stream の say は全文を読む。
    s.manager.join(BINDING, Some("555"), None).await.unwrap();
    s.say("s1", "一文目です。二文目です。", Some(STREAM)).await;
    s.wait_posts(1).await;
    assert_eq!(
        s.settled_speech().await,
        strings(&["一文目です。", "一文目です。", "二文目です。"])
    );
}

#[tokio::test]
async fn g_f_cumulative_speech_is_capped_with_the_suffix_once() {
    let mut s = setup(true).await;
    // 40 文字の文を 10 個（400 文字）。partial は 1 文ずつ伸びる。
    let sentence = format!("{}。", "あ".repeat(39));
    let mut text = String::new();
    for _ in 0..10 {
        text.push_str(&sentence);
        s.partial(STREAM, &text).await;
    }
    wait_played(&s.player, 8).await;
    s.say("s1", &text, Some(STREAM)).await;
    s.wait_posts(1).await;
    assert_eq!(s.transport.bodies().concat(), text);

    let spoken = s.settled_speech().await;
    let mut expected = vec![sentence.clone(); 7];
    expected.push(format!("{}、以下省略", "あ".repeat(20)));
    assert_eq!(spoken, expected);
    let chars: usize = spoken
        .iter()
        .map(|t| t.trim_end_matches("、以下省略").chars().count())
        .sum();
    assert_eq!(chars, 300);
    assert_eq!(spoken.iter().filter(|t| t.contains("以下省略")).count(), 1);
}

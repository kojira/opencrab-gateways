//! STT→said と say/reply→TTS を、mock core（UDS）と mock STT/VOICEVOX（HTTP）で通す。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencrab_gate_client::client::InstanceClient;
use opencrab_gate_client::wire::{read_frame, write_json};
use opencrab_gate_client::{InvokeHandler, InvokeOutcome};
use serde_json::{json, Value};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;

use super::mock_http;
use crate::config::{AccessConfig, SystemReactions};
use crate::ops::{BindingDeliveryTargets, DiscordInvokeHandler};
use crate::transport::{DiscordTransport, DryRunTransport};
use crate::voice::session::{VoiceManager, VoiceManagerConfig, VoicePlayer};

pub(super) const BINDING: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[derive(Default)]
pub(super) struct FakePlayer {
    pub(super) joined: Mutex<Vec<(u64, u64)>>,
    left: Mutex<Vec<u64>>,
    pub(super) played: Mutex<Vec<(u64, Vec<u8>)>>,
    pub(super) played_at: Mutex<Vec<std::time::Instant>>,
}

#[async_trait::async_trait]
impl VoicePlayer for FakePlayer {
    async fn join(&self, guild: u64, channel: u64, _: Arc<VoiceManager>) -> anyhow::Result<()> {
        self.joined.lock().unwrap().push((guild, channel));
        Ok(())
    }
    async fn leave(&self, guild: u64) -> anyhow::Result<()> {
        self.left.lock().unwrap().push(guild);
        Ok(())
    }
    async fn play(&self, guild: u64, wav: Vec<u8>) -> anyhow::Result<()> {
        self.played.lock().unwrap().push((guild, wav));
        self.played_at
            .lock()
            .unwrap()
            .push(std::time::Instant::now());
        Ok(())
    }
}

pub(super) struct Core {
    pub(super) reader: OwnedReadHalf,
    pub(super) writer: OwnedWriteHalf,
    pub(super) client: Arc<InstanceClient>,
    pub(super) address: String,
    _dir: tempfile::TempDir,
}

pub(super) async fn core() -> Core {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("core.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let connect_path = socket.clone();
    let connect = tokio::spawn(async move {
        InstanceClient::connect(
            &connect_path,
            "11111111-1111-4111-8111-111111111111".into(),
            1,
            "999".into(),
            "0".repeat(64),
        )
        .await
        .unwrap()
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let hello: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
    write_json(&mut writer, &json!({"id":hello["id"],"m":"ok"}))
        .await
        .unwrap();
    let client = connect.await.unwrap();
    let address = crate::map::address_for("agent", "20", "10");
    write_json(
        &mut writer,
        &json!({"id":"bind-1","m":"bind","binding_id":BINDING,"address":address}),
    )
    .await
    .unwrap();
    let ok: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
    assert_eq!(ok["m"], "ok");
    Core {
        reader,
        writer,
        client,
        address,
        _dir: dir,
    }
}

fn manager(
    core: &Core,
    settings_path: std::path::PathBuf,
    player: Arc<FakePlayer>,
) -> Arc<VoiceManager> {
    manager_with(core, settings_path, player, Arc::new(DryRunTransport))
}

pub(super) fn manager_with(
    core: &Core,
    settings_path: std::path::PathBuf,
    player: Arc<FakePlayer>,
    transport: Arc<dyn DiscordTransport>,
) -> Arc<VoiceManager> {
    let manager = VoiceManager::new(
        VoiceManagerConfig {
            agent_id: "agent".into(),
            self_bot_id: "999".into(),
            access: AccessConfig {
                owners: vec!["30".into()],
                ..Default::default()
            },
            addresses: vec![core.address.clone()],
            settings_path,
        },
        player,
        transport,
    );
    manager.attach_client(core.client.clone());
    manager
}

pub(super) fn loud_pcm() -> Vec<i16> {
    (0..96_000)
        .map(|i| if i % 4 < 2 { 3000 } else { -3000 })
        .collect()
}

pub(super) async fn wait_played(player: &FakePlayer, n: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while player.played.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("TTS playback did not happen");
}

#[tokio::test]
async fn join_requires_settings_and_a_guild_conversation() {
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager(&core, dir.path().join("settings.json"), player.clone());
    let err = m.join(BINDING, Some("555"), None).await.unwrap_err();
    assert!(err.contains("not configured"), "{err}");
    assert!(player.joined.lock().unwrap().is_empty());
    std::fs::write(
        dir.path().join("settings.json"),
        mock_http::settings_json("http://127.0.0.1:9"),
    )
    .unwrap();
    let err = m
        .join("unknown-binding", Some("555"), None)
        .await
        .unwrap_err();
    assert!(err.contains("conversation"), "{err}");
    let err = m.join(BINDING, Some("555"), Some("77")).await.unwrap_err();
    assert!(err.contains("text channel"), "{err}");
    let ok = m.join(BINDING, Some("555"), None).await.unwrap();
    assert_eq!(ok["status"], "joined");
    assert_eq!(*player.joined.lock().unwrap(), vec![(20, 555)]);
    let left = m.leave(BINDING).await.unwrap();
    assert_eq!(left["status"], "left");
    assert_eq!(*player.left.lock().unwrap(), vec![20]);
}

#[tokio::test]
async fn transcribed_speech_becomes_said_on_the_text_channel() {
    let (mock, recorded) = mock_http::spawn("こんにちは").await;
    let mut core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager(&core, path, player);
    m.join(BINDING, Some("555"), None).await.unwrap();

    // 自分の声と無音は STT へ送らない。
    m.process_segment(20, 999, loud_pcm()).await;
    m.process_segment(20, 30, vec![0; 96_000]).await;
    assert!(recorded.stt_bodies.lock().unwrap().is_empty());

    let task = {
        let m = m.clone();
        tokio::spawn(async move { m.process_segment(20, 30, loud_pcm()).await })
    };
    let said: Value = serde_json::from_slice(&read_frame(&mut core.reader).await.unwrap()).unwrap();
    assert_eq!(said["m"], "said");
    assert_eq!(said["binding_id"], BINDING);
    assert!(
        said["origin"]
            .as_str()
            .unwrap()
            .starts_with("discord:voice:v1:10:20:30:"),
        "{said}"
    );
    assert_eq!(said["author_id"], "30");
    assert_eq!(said["text"], "（音声）こんにちは");
    assert_eq!(said["caller"]["role"], "owner");
    assert_eq!(said["start_turn"], true);
    write_json(&mut core.writer, &json!({"id":said["id"],"m":"ok","seq":1}))
        .await
        .unwrap();
    task.await.unwrap();

    let bodies = recorded.stt_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    let at = body
        .windows(4)
        .position(|w| w == b"RIFF")
        .expect("WAV part");
    let rate = u32::from_le_bytes(body[at + 24..at + 28].try_into().unwrap());
    assert_eq!(rate, 16_000, "STT へは 16kHz モノラル WAV");
}

#[tokio::test]
async fn posted_say_and_reply_are_spoken_in_order() {
    let (mock, recorded) = mock_http::spawn("unused").await;
    let mut core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager(&core, path, player.clone());

    // VC に入っていないチャンネルの投稿は読み上げない。
    m.speak_posted("10", "まだ入っていない");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(player.played.lock().unwrap().is_empty());

    m.join(BINDING, Some("555"), None).await.unwrap();
    let targets: BindingDeliveryTargets =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let transport: Arc<dyn DiscordTransport> = Arc::new(DryRunTransport);
    let consumer = crate::run::spawn_say_consumer(
        core.client.clone(),
        core.address.clone(),
        transport.clone(),
        "agent".into(),
        SystemReactions::default(),
        targets.clone(),
        Some(m.clone()),
    );
    write_json(
        &mut core.writer,
        &json!({"id":"s1","m":"say","binding_id":BINDING,"payload":{"text":"こんにちは <@1> https://x.example"}}),
    )
    .await
    .unwrap();
    let say_ok: Value =
        serde_json::from_slice(&read_frame(&mut core.reader).await.unwrap()).unwrap();
    assert_eq!(say_ok["id"], "s1");
    wait_played(&player, 1).await;

    let handler = DiscordInvokeHandler::with_voice(transport, targets, Some(m.clone()));
    let out = handler
        .handle(
            "r1",
            BINDING,
            "reply",
            &json!({"event": "discord:message:v1:10:200", "text": "返信です"}),
        )
        .await;
    assert!(matches!(out, InvokeOutcome::Ok(_)));
    wait_played(&player, 2).await;

    let played = player.played.lock().unwrap();
    assert!(played
        .iter()
        .all(|(guild, wav)| *guild == 20 && wav == mock_http::WAV_FROM_TTS));
    let queries = recorded.tts_queries.lock().unwrap();
    assert_eq!(
        *queries,
        vec![
            ("こんにちは リンク".to_string(), "8".to_string()),
            ("返信です".to_string(), "8".to_string()),
        ]
    );
    consumer.abort();
}

/// core 側で said を `n` 件受け、受けた順に (author_id, text) を返す。各 said には ok を返す。
async fn receive_saids(core: &mut Core, n: usize) -> Vec<(String, String)> {
    let mut got = Vec::new();
    while got.len() < n {
        let frame: Value =
            tokio::time::timeout(Duration::from_secs(5), read_frame(&mut core.reader))
                .await
                .expect("said did not arrive")
                .map(|bytes| serde_json::from_slice(&bytes).unwrap())
                .unwrap();
        assert_eq!(frame["m"], "said", "{frame}");
        write_json(
            &mut core.writer,
            &json!({"id":frame["id"],"m":"ok","seq":got.len() + 1}),
        )
        .await
        .unwrap();
        got.push((
            frame["author_id"].as_str().unwrap().to_string(),
            frame["text"].as_str().unwrap().to_string(),
        ));
    }
    got
}

#[tokio::test]
async fn split_speech_of_one_speaker_is_posted_in_order() {
    // 1 つ目の区間の STT が 2 つ目より遅くても、同じ話者の発言は区切った順に said になる。
    let (mock, _) = mock_http::spawn_sequenced(vec![(400, "前半"), (0, "後半")]).await;
    let mut core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let m = manager(&core, path, Arc::new(FakePlayer::default()));
    m.join(BINDING, Some("555"), None).await.unwrap();

    m.enqueue_segment(20, 30, loud_pcm());
    m.enqueue_segment(20, 30, loud_pcm());
    let saids = receive_saids(&mut core, 2).await;
    assert_eq!(
        saids,
        vec![
            ("30".to_string(), "（音声）前半".to_string()),
            ("30".to_string(), "（音声）後半".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_slow_speaker_does_not_hold_back_another_speaker() {
    // 話者ごとの順序は守るが、話者 30 の STT 待ちで話者 40 の発言を止めない。
    let (mock, _) = mock_http::spawn_sequenced(vec![(600, "遅い人"), (0, "速い人")]).await;
    let mut core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let m = manager(&core, path, Arc::new(FakePlayer::default()));
    m.join(BINDING, Some("555"), None).await.unwrap();

    m.enqueue_segment(20, 30, loud_pcm());
    tokio::time::sleep(Duration::from_millis(100)).await;
    m.enqueue_segment(20, 40, loud_pcm());
    let saids = receive_saids(&mut core, 2).await;
    assert_eq!(
        saids,
        vec![
            ("40".to_string(), "（音声）速い人".to_string()),
            ("30".to_string(), "（音声）遅い人".to_string()),
        ]
    );
}

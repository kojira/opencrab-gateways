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

const BINDING: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[derive(Default)]
struct FakePlayer {
    joined: Mutex<Vec<(u64, u64)>>,
    left: Mutex<Vec<u64>>,
    played: Mutex<Vec<(u64, Vec<u8>)>>,
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
        Ok(())
    }
}

struct Core {
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
    client: Arc<InstanceClient>,
    address: String,
    _dir: tempfile::TempDir,
}

async fn core() -> Core {
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
        Arc::new(DryRunTransport),
    );
    manager.attach_client(core.client.clone());
    manager
}

fn loud_pcm() -> Vec<i16> {
    (0..96_000)
        .map(|i| if i % 4 < 2 { 3000 } else { -3000 })
        .collect()
}

async fn wait_played(player: &FakePlayer, n: usize) {
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
    let err = m.join(BINDING, "555", None).await.unwrap_err();
    assert!(err.contains("not configured"), "{err}");
    assert!(player.joined.lock().unwrap().is_empty());
    std::fs::write(
        dir.path().join("settings.json"),
        mock_http::settings_json("http://127.0.0.1:9"),
    )
    .unwrap();
    let err = m.join("unknown-binding", "555", None).await.unwrap_err();
    assert!(err.contains("conversation"), "{err}");
    let err = m.join(BINDING, "555", Some("77")).await.unwrap_err();
    assert!(err.contains("text channel"), "{err}");
    let ok = m.join(BINDING, "555", None).await.unwrap();
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
    m.join(BINDING, "555", None).await.unwrap();

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

    m.join(BINDING, "555", None).await.unwrap();
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

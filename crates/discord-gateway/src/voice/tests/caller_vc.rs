//! `join_voice` の channel_id 省略時: この会話で直近に said が受理された人間の発言者が今いる VC に入る。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::{json, Value};

use super::e2e::{core, loud_pcm, manager_with, Core, FakePlayer, BINDING};
use super::mock_http;
use crate::config::AccessConfig;
use crate::transport::{DiscordTransport, TransportOutcome};
use crate::voice::session::VoiceManager;

/// voice state だけを持つ transport。(guild, user) → VC の voice state。無ければ 404 相当（Rejected）。
#[derive(Default)]
struct VoiceStates {
    states: HashMap<(String, String), Value>,
    lookups: Mutex<Vec<(String, String)>>,
}

impl VoiceStates {
    fn with(mut self, guild: &str, user: &str, state: Value) -> Self {
        self.states.insert((guild.into(), user.into()), state);
        self
    }
}

#[async_trait::async_trait]
impl DiscordTransport for VoiceStates {
    async fn create_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn reply_message(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn add_reaction(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn add_system_reaction(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn get_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn get_user(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
    async fn get_voice_state(&self, guild_id: &str, user_id: &str) -> TransportOutcome {
        self.lookups
            .lock()
            .unwrap()
            .push((guild_id.into(), user_id.into()));
        match self.states.get(&(guild_id.into(), user_id.into())) {
            Some(state) => TransportOutcome::Ok(state.clone()),
            None => TransportOutcome::Rejected,
        }
    }
    async fn broadcast_typing(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }
}

fn in_vc(guild: &str, channel: &str, user: &str) -> Value {
    json!({"guild_id": guild, "channel_id": channel, "user_id": user, "session_id": "s"})
}

struct Setup {
    core: Core,
    player: Arc<FakePlayer>,
    transport: Arc<VoiceStates>,
    manager: Arc<VoiceManager>,
    _dir: tempfile::TempDir,
}

async fn setup(transport: VoiceStates, stt_base: &str) -> Setup {
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(stt_base)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let transport = Arc::new(transport);
    let manager = manager_with(&core, path, player.clone(), transport.clone());
    Setup {
        core,
        player,
        transport,
        manager,
        _dir: dir,
    }
}

/// テキストチャンネル（guild 20 / channel 10）で `author` が発言し、core が said を受理する。
async fn speaks(s: &mut Setup, message_id: &str, author: &str, bot: bool) {
    let line = json!({
        "id": message_id, "channel_id": "10", "guild_id": "20",
        "author": {"id": author, "bot": bot, "username": "u"},
        "content": "VC に来て",
    })
    .to_string();
    let client = s.core.client.clone();
    let voice = s.manager.clone();
    let task = tokio::spawn(async move {
        let access = AccessConfig {
            owners: vec!["30".into()],
            ..Default::default()
        };
        crate::run::handle_incoming(&client, "agent", "999", &access, &line, None, Some(&voice))
            .await;
    });
    let said: Value =
        serde_json::from_slice(&read_frame(&mut s.core.reader).await.unwrap()).unwrap();
    assert_eq!(said["m"], "said");
    assert_eq!(said["author_id"], author);
    write_json(
        &mut s.core.writer,
        &json!({"id": said["id"], "m": "ok", "seq": 1}),
    )
    .await
    .unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn join_without_channel_id_joins_the_vc_of_the_last_human_speaker() {
    let transport = VoiceStates::default()
        .with("20", "31", in_vc("20", "778", "31"))
        .with("20", "30", in_vc("20", "777", "30"));
    let mut s = setup(transport, "http://127.0.0.1:9").await;
    speaks(&mut s, "1", "31", false).await;
    speaks(&mut s, "2", "30", false).await;

    let ok = s.manager.join(BINDING, None, None).await.unwrap();
    assert_eq!(ok["status"], "joined");
    assert_eq!(ok["vc_channel_id"], "777");
    assert_eq!(*s.player.joined.lock().unwrap(), vec![(20, 777)]);
    assert_eq!(
        *s.transport.lookups.lock().unwrap(),
        vec![("20".to_string(), "30".to_string())]
    );
}

#[tokio::test]
async fn bot_authors_are_not_the_caller() {
    let transport = VoiceStates::default()
        .with("20", "30", in_vc("20", "777", "30"))
        .with("20", "40", in_vc("20", "779", "40"));
    let mut s = setup(transport, "http://127.0.0.1:9").await;
    // bot だけが話した会話では呼びかけ手は分からない。
    speaks(&mut s, "1", "40", true).await;
    let err = s.manager.join(BINDING, None, None).await.unwrap_err();
    assert!(err.contains("no recent human speaker"), "{err}");

    speaks(&mut s, "2", "30", false).await;
    speaks(&mut s, "3", "40", true).await;
    s.manager.join(BINDING, None, None).await.unwrap();
    assert_eq!(*s.player.joined.lock().unwrap(), vec![(20, 777)]);
}

#[tokio::test]
async fn join_without_channel_id_fails_when_no_speaker_is_known() {
    let transport = VoiceStates::default().with("20", "30", in_vc("20", "777", "30"));
    let s = setup(transport, "http://127.0.0.1:9").await;
    let err = s.manager.join(BINDING, None, None).await.unwrap_err();
    assert!(err.contains("no recent human speaker"), "{err}");
    assert!(s.player.joined.lock().unwrap().is_empty());
    assert!(s.transport.lookups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn join_without_channel_id_fails_when_the_speaker_is_not_in_a_vc() {
    // 30 の voice state は無い（Discord は 404）。別の人（31）が VC にいても選ばない。
    let transport = VoiceStates::default().with("20", "31", in_vc("20", "778", "31"));
    let mut s = setup(transport, "http://127.0.0.1:9").await;
    speaks(&mut s, "1", "30", false).await;
    let err = s.manager.join(BINDING, None, None).await.unwrap_err();
    assert!(err.contains("not in a voice channel"), "{err}");
    assert!(s.player.joined.lock().unwrap().is_empty());

    // VC から抜けた直後の state（channel_id null）も「いない」。
    let transport = VoiceStates::default().with(
        "20",
        "30",
        json!({"guild_id": "20", "channel_id": null, "user_id": "30"}),
    );
    let mut s = setup(transport, "http://127.0.0.1:9").await;
    speaks(&mut s, "1", "30", false).await;
    let err = s.manager.join(BINDING, None, None).await.unwrap_err();
    assert!(err.contains("not in a voice channel"), "{err}");
}

#[tokio::test]
async fn join_without_channel_id_fails_when_the_speakers_vc_is_in_another_guild() {
    let transport = VoiceStates::default().with("20", "30", in_vc("21", "777", "30"));
    let mut s = setup(transport, "http://127.0.0.1:9").await;
    speaks(&mut s, "1", "30", false).await;
    let err = s.manager.join(BINDING, None, None).await.unwrap_err();
    assert!(err.contains("another server"), "{err}");
    assert!(s.player.joined.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_explicit_channel_id_still_joins_that_vc_without_a_lookup() {
    let mut s = setup(VoiceStates::default(), "http://127.0.0.1:9").await;
    speaks(&mut s, "1", "30", false).await;
    s.manager.join(BINDING, Some("555"), None).await.unwrap();
    assert_eq!(*s.player.joined.lock().unwrap(), vec![(20, 555)]);
    assert!(s.transport.lookups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_voice_transcript_speaker_counts_as_the_caller() {
    let (stt, _) = mock_http::spawn("こんにちは").await;
    let transport = VoiceStates::default().with("20", "32", in_vc("20", "780", "32"));
    let mut s = setup(transport, &stt).await;
    s.manager.join(BINDING, Some("555"), None).await.unwrap();
    let task = {
        let m = s.manager.clone();
        tokio::spawn(async move { m.process_segment(20, 32, loud_pcm()).await })
    };
    let said: Value =
        serde_json::from_slice(&read_frame(&mut s.core.reader).await.unwrap()).unwrap();
    assert_eq!(said["author_id"], "32");
    write_json(
        &mut s.core.writer,
        &json!({"id": said["id"], "m": "ok", "seq": 1}),
    )
    .await
    .unwrap();
    task.await.unwrap();
    s.manager.leave(BINDING).await.unwrap();

    s.manager.join(BINDING, None, None).await.unwrap();
    assert_eq!(*s.player.joined.lock().unwrap(), vec![(20, 555), (20, 780)]);
}

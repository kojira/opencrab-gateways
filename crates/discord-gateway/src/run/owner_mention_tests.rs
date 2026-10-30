//! D-1097: an owner @mentioning the bot in a channel it does not listen to enables that channel;
//! nobody else, and nothing without a mention, does. D-1099: core is not asked.

use super::owner_mention::wants_channel;
use crate::config::AccessConfig;
use crate::map::parse_event_line;

const BOT: &str = "555555555555555555";
const OWNER: &str = "222222222222222222";

fn access() -> AccessConfig {
    AccessConfig {
        owners: vec![OWNER.into()],
        trusted_users: vec!["333333333333333333".into()],
        ..AccessConfig::default()
    }
}

fn message(author: &str, mentions: &[&str]) -> crate::map::IncomingMessage {
    let line = serde_json::json!({"id":"7","channel_id":"200","guild_id":"100",
        "author":{"id":author},"content":"hi","mentions":mentions});
    parse_event_line(&line.to_string()).unwrap()
}

#[test]
fn d1097_mentions_are_parsed_from_the_message_line() {
    assert_eq!(message(OWNER, &[BOT]).mentions, vec![BOT]);
    let old = parse_event_line(r#"{"id":"7","channel_id":"200","author":{"id":"1"}}"#).unwrap();
    assert!(
        old.mentions.is_empty(),
        "lines without the field still parse"
    );
}

#[test]
fn d1097_only_an_owner_mention_in_an_unknown_channel_asks() {
    let address = "discord-agent-100-200";
    assert!(wants_channel(
        &access(),
        BOT,
        &message(OWNER, &[BOT]),
        false
    ));
    assert!(
        !wants_channel(&access(), BOT, &message(OWNER, &[]), false),
        "no mention"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message(OWNER, &["999999999999999999"]),
            false
        ),
        "someone else mentioned"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message("333333333333333333", &[BOT]),
            false
        ),
        "a trusted user is not an owner"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message("444444444444444444", &[BOT]),
            false
        ),
        "a stranger"
    );
    assert!(
        !wants_channel(&access(), BOT, &message(OWNER, &[BOT]), true),
        "the bot already listens to the channel"
    );
    let _ = address;
}

mod end_to_end {
    use super::*;
    use crate::channel_request::{serve, ChannelBinder};
    use crate::run::owner_mention::{listening, Listening};
    use opencrab_gate_client::client::InstanceClient;
    use opencrab_gate_client::wire::{read_frame, write_json};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";

    /// The daemon stand-in: records what the child tells it.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ChannelBinder for Recorder {
        async fn record_address(&self, _instance: &str, address: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(address.to_string());
            Ok(())
        }
    }

    struct World {
        _dir: tempfile::TempDir,
        client: Arc<InstanceClient>,
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        recorder: Arc<Recorder>,
        socket: std::path::PathBuf,
        listening: Listening,
    }

    async fn world() -> World {
        // A short directory: Unix socket paths are limited to about 104 bytes.
        let dir = tempfile::Builder::new()
            .prefix("om")
            .tempdir_in("/tmp")
            .unwrap();
        let core_path = dir.path().join("core.sock");
        let listener = tokio::net::UnixListener::bind(&core_path).unwrap();
        let path = core_path.clone();
        let connect = tokio::spawn(async move {
            InstanceClient::connect(&path, INSTANCE.into(), "agent".into(), BOT.into())
                .await
                .unwrap()
        });
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let hello: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
        assert_eq!(hello["agent_id"], "agent", "hello names the agent");
        write_json(&mut writer, &json!({"id":hello["id"],"m":"ok"}))
            .await
            .unwrap();
        let client = connect.await.unwrap();
        let recorder = Arc::new(Recorder::default());
        let socket = crate::channel_request::socket_path(dir.path(), INSTANCE);
        tokio::spawn(serve(socket.clone(), INSTANCE.into(), recorder.clone()));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        World {
            _dir: dir,
            client,
            reader,
            writer,
            recorder,
            socket,
            listening: listening(&[]),
        }
    }

    fn line(author: &str, mentions: &[&str]) -> String {
        json!({"id":"7","channel_id":"200","guild_id":"100","author":{"id":author},
            "content":"<@555555555555555555> hello","mentions":mentions})
        .to_string()
    }

    /// The child's per-line handling: owner-mention enable, then the normal said path.
    fn receive(w: &World, line: String) -> tokio::task::JoinHandle<()> {
        let client = w.client.clone();
        let socket = w.socket.clone();
        let listening = w.listening.clone();
        tokio::spawn(async move {
            let access = access();
            let mention = ("agent", BOT, &access);
            let enable = (&listening, Some(socket.as_path()), None);
            super::super::owner_mention::maybe_enable(mention, enable, &line).await;
            crate::run::handle_incoming(
                &client,
                ("agent", BOT, &access, &listening),
                &line,
                None,
                None,
            )
            .await;
        })
    }

    async fn next(reader: &mut OwnedReadHalf) -> Value {
        serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(5), read_frame(reader))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    }

    /// D-1099: the owner's message goes straight to core as a said of the channel's session key
    /// (nothing is registered in core first), and the daemon records the channel.
    #[tokio::test]
    async fn d1097_owner_mention_in_a_new_channel_delivers_that_message() {
        let mut w = world().await;
        let address = crate::map::address_for("agent", "100", "200");
        let task = receive(&w, line(OWNER, &[BOT]));
        let said = next(&mut w.reader).await;
        assert_eq!(said["m"], "said", "{said}");
        assert_eq!(said["session"], address);
        assert_eq!(said["ref"], "7", "the message id is its reference");
        assert_eq!(said["author_id"], OWNER);
        assert!(said["text"].as_str().unwrap().contains("hello"));
        write_json(
            &mut w.writer,
            &json!({"id": said["id"], "m": "ok", "seq": 1, "session_id": address}),
        )
        .await
        .unwrap();
        task.await.unwrap();
        for _ in 0..200 {
            if !w.recorder.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(w.recorder.0.lock().unwrap().clone(), vec![address.clone()]);
        assert!(super::super::owner_mention::listens(&w.listening, &address));
    }

    #[tokio::test]
    async fn d1097_a_non_owner_mention_or_an_owner_without_one_does_nothing() {
        let mut w = world().await;
        for line in [line("444444444444444444", &[BOT]), line(OWNER, &[])] {
            receive(&w, line).await.unwrap();
        }
        assert!(w.recorder.0.lock().unwrap().is_empty());
        let nothing =
            tokio::time::timeout(Duration::from_millis(200), read_frame(&mut w.reader)).await;
        assert!(nothing.is_err(), "no frame reaches core");
    }
}

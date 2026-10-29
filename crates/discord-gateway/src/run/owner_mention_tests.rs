//! D-1097: an owner @mentioning the bot in a channel it does not have asks the daemon for that
//! channel; nobody else, and nothing without a mention, does.

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
        "the channel is already bound"
    );
    let _ = address;
}

mod end_to_end {
    use super::*;
    use crate::channel_request::{serve, ChannelBinder};
    use opencrab_gate_client::client::InstanceClient;
    use opencrab_gate_client::wire::{read_frame, write_json};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    const NEW_BINDING: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    /// A daemon stand-in: records the request and makes "core" send the bind to the child.
    struct Binder {
        asked: Mutex<Vec<String>>,
        core_writer: tokio::sync::Mutex<OwnedWriteHalf>,
    }

    #[async_trait::async_trait]
    impl ChannelBinder for Binder {
        async fn bind_address(&self, _instance: &str, address: &str) -> anyhow::Result<()> {
            self.asked.lock().unwrap().push(address.to_string());
            write_json(
                &mut *self.core_writer.lock().await,
                &json!({"id":"bind-2","m":"bind","binding_id":NEW_BINDING,"address":address}),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            Ok(())
        }
    }

    struct World {
        _dir: tempfile::TempDir,
        client: Arc<InstanceClient>,
        reader: OwnedReadHalf,
        binder: Arc<Binder>,
        socket: std::path::PathBuf,
    }

    async fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let core_path = dir.path().join("core.sock");
        let listener = tokio::net::UnixListener::bind(&core_path).unwrap();
        let path = core_path.clone();
        let connect = tokio::spawn(async move {
            InstanceClient::connect(
                &path,
                "11111111-1111-4111-8111-111111111111".into(),
                1,
                BOT.into(),
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
        let binder = Arc::new(Binder {
            asked: Mutex::new(Vec::new()),
            core_writer: tokio::sync::Mutex::new(writer),
        });
        let socket = crate::channel_request::socket_path(dir.path(), "instance");
        tokio::spawn(serve(socket.clone(), "instance".into(), binder.clone()));
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
            binder,
            socket,
        }
    }

    fn line(author: &str, mentions: &[&str]) -> String {
        json!({"id":"7","channel_id":"200","guild_id":"100","author":{"id":author},
            "content":"<@555555555555555555> hello","mentions":mentions})
        .to_string()
    }

    /// Runs the child's per-line handling: owner-mention enable, then the normal said path.
    async fn receive(w: &World, line: String) -> tokio::task::JoinHandle<()> {
        let client = w.client.clone();
        let socket = w.socket.clone();
        tokio::spawn(async move {
            let access = access();
            let mention = (&*client, "agent", BOT, &access);
            super::super::owner_mention::maybe_enable(mention, Some(&socket), &line).await;
            crate::run::handle_incoming(&client, "agent", BOT, &access, &line, None, None).await;
        })
    }

    #[tokio::test]
    async fn d1097_owner_mention_in_a_new_channel_binds_then_delivers_that_message() {
        let mut w = world().await;
        let task = receive(&w, line(OWNER, &[BOT])).await;
        // Core sees the bind ack first, then the said for the very message that asked.
        let ack: Value = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(5), read_frame(&mut w.reader))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ack["id"], "bind-2");
        assert_eq!(ack["m"], "ok");
        let said: Value = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(5), read_frame(&mut w.reader))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(said["m"], "said");
        assert_eq!(said["binding_id"], NEW_BINDING);
        assert_eq!(said["author_id"], OWNER);
        assert!(said["text"].as_str().unwrap().contains("hello"));
        write_json(
            &mut *w.binder.core_writer.lock().await,
            &json!({"id": said["id"], "m": "ok", "seq": 1}),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(
            w.binder.asked.lock().unwrap().clone(),
            vec![crate::map::address_for("agent", "100", "200")]
        );
    }

    #[tokio::test]
    async fn d1097_a_non_owner_mention_or_an_owner_without_one_does_nothing() {
        let mut w = world().await;
        for line in [line("444444444444444444", &[BOT]), line(OWNER, &[])] {
            receive(&w, line).await.await.unwrap();
        }
        assert!(w.binder.asked.lock().unwrap().is_empty());
        let nothing =
            tokio::time::timeout(Duration::from_millis(200), read_frame(&mut w.reader)).await;
        assert!(nothing.is_err(), "no frame reaches core");
    }
}

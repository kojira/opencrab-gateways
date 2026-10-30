use super::*;
use crate::transport::TransportOutcome;
use opencrab_gate_client::wire::{read_frame, write_json};
use serde_json::json;
use tokio::net::UnixListener;

struct RecordingTransport {
    reactions: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl RecordingTransport {
    fn new() -> Self {
        Self {
            reactions: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl DiscordTransport for RecordingTransport {
    async fn create_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({"message_id":"300"}))
    }

    async fn reply_message(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }
    async fn reply_message_with_files(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Vec<crate::transport::ReplyFile>,
    ) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }

    async fn add_reaction(&self, _: &str, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }

    async fn add_system_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> TransportOutcome {
        self.reactions.lock().unwrap().push((
            channel_id.to_string(),
            message_id.to_string(),
            emoji.to_string(),
        ));
        TransportOutcome::Ok(json!({}))
    }

    async fn get_message(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }

    async fn get_user(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }

    async fn get_voice_state(&self, _: &str, _: &str) -> TransportOutcome {
        TransportOutcome::Rejected
    }

    async fn broadcast_typing(&self, _: &str) -> TransportOutcome {
        TransportOutcome::Ok(json!({}))
    }
}

#[tokio::test]
async fn event_loop_reacts_to_a_visible_b_silent_and_completion_without_extras() {
    let socket = std::env::temp_dir().join(format!("oc-discord-{}.sock", uuid::Uuid::new_v4()));
    let listener = UnixListener::bind(&socket).unwrap();
    let connect_path = socket.clone();
    let connect = tokio::spawn(async move {
        InstanceClient::connect(
            &connect_path,
            "11111111-1111-4111-8111-111111111111".into(),
            "agent".into(),
            "bot".into(),
        )
        .await
        .unwrap()
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let hello =
        serde_json::from_slice::<serde_json::Value>(&read_frame(&mut reader).await.unwrap())
            .unwrap();
    write_json(&mut writer, &json!({"id":hello["id"],"m":"ok"}))
        .await
        .unwrap();
    let client = connect.await.unwrap();
    let agent_id = "agent";
    let address = crate::map::address_for(agent_id, "20", "10");
    let binding = address.clone();

    let recording = Arc::new(RecordingTransport::new());
    let transport: Arc<dyn DiscordTransport> = recording.clone();
    let targets: SessionDeliveryTargets =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let consumer = spawn_say_consumer(
        client,
        address,
        transport,
        agent_id.into(),
        SystemReactions {
            accepted: "👀".into(),
            completed: "🏁".into(),
            failed: "❌".into(),
            no_reply: "🤐".into(),
        },
        targets,
        None,
        None,
    );
    let activity_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    for frame in [
        json!({"m":"activity","session":binding,"activity_id":activity_id,"state":"started"}),
        json!({"m":"activity","session":binding,"activity_id":activity_id,"state":"read","origin":"discord:message:v1:10:100"}),
        json!({"m":"activity","session":binding,"activity_id":activity_id,"state":"read","origin":"discord:message:v1:10:200"}),
        json!({"id":"visible-a","m":"say","session":binding,"payload":{"text":"visible A"}}),
    ] {
        write_json(&mut writer, &frame).await.unwrap();
    }
    let say_ok =
        serde_json::from_slice::<serde_json::Value>(&read_frame(&mut reader).await.unwrap())
            .unwrap();
    assert_eq!(say_ok["id"], "visible-a");
    write_json(
        &mut writer,
        &json!({
            "m":"activity",
            "session":binding,
            "activity_id":activity_id,
            "state":"ended",
            "completed_target":"visible-a",
            "silent_origins":["discord:message:v1:10:200"]
        }),
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if recording.reactions.lock().unwrap().len() == 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        recording.reactions.lock().unwrap().as_slice(),
        [
            ("10".into(), "100".into(), "👀".into()),
            ("10".into(), "200".into(), "👀".into()),
            ("10".into(), "300".into(), "🏁".into()),
            ("10".into(), "200".into(), "🤐".into()),
        ]
    );
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert_eq!(recording.reactions.lock().unwrap().len(), 4, "no extras");
    consumer.abort();
    let _ = std::fs::remove_file(socket);
}

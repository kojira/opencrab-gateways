// ==================== D-1097: owner mention enables a channel (real core) ====================

/// An owner @mentioning the bot in a channel it does not listen to: the bot starts listening and
/// that same message reaches real core as a said of the channel's session key, which opens the
/// session (D-1099: nothing registered first). A non-owner mention and an owner message without
/// a mention in other channels reach core with nothing.
#[tokio::test]
async fn d1099_owner_mention_enables_the_channel_in_real_core() {
    use std::io::Write as _;
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();

    let instance_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        addresses: vec![address()],
        config_b64,
        channel_socket: None,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(core.sock.clone(), &place, &config_bytes, None, overrides, None)
        .expect("spawn_instance");
    wait_connected(&client, &address()).await;

    let line = |id: &str, channel: &str, author: &str, mentions: &[&str]| {
        serde_json::json!({"id": id, "channel_id": channel, "guild_id": GUILD,
            "author": {"id": author, "bot": false, "username": "u"},
            "content": format!("{M_SAY} hello"), "mentions": mentions})
        .to_string()
    };
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.path)
            .unwrap();
        writeln!(f, "{}", line("801", "777", "999999", &[SELF_BOT])).unwrap(); // not an owner
        writeln!(f, "{}", line("802", "778", AUTHOR, &[])).unwrap(); // owner, no mention
        writeln!(f, "{}", line("803", "779", AUTHOR, &[SELF_BOT])).unwrap(); // owner mention
    }

    let new_address = format!("discord-{AGENT_ID}-{GUILD}-779");
    let mut said_on_new = None;
    for _ in 0..250 {
        said_on_new = {
            let conn = core.extgate.db.lock().unwrap();
            conn.query_row(
                "SELECT o.binding_id FROM external_origins o
                   JOIN gate_bindings b ON b.binding_id = o.binding_id
                  WHERE b.session_key = ?1",
                [&new_address],
                |r| r.get::<_, String>(0),
            )
            .ok()
        };
        if said_on_new.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    said_on_new.expect("the owner's mention became a said of the new session key");

    let other: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM gate_bindings WHERE instance_id=?1 AND session_key IN (?2, ?3)",
            rusqlite::params![
                instance_id,
                format!("discord-{AGENT_ID}-{GUILD}-777"),
                format!("discord-{AGENT_ID}-{GUILD}-778"),
            ],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(other, 0, "a non-owner mention or an owner without a mention reaches core with nothing");

    // The bot's plain answer is posted to the newly enabled channel.
    let posted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == "779" && c.body.contains(B_SAY))
        })
        .await
    };
    assert!(posted, "no answer on the new channel: {:?}", captured(&buf));
    assert!(
        !captured(&buf)
            .iter()
            .any(|c| c.kind == "say" && (c.channel == "777" || c.channel == "778")),
        "nothing is posted where the channel was not enabled"
    );
}

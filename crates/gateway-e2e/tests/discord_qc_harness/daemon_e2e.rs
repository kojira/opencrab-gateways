// ==================== D-1098: Discord daemon cases against a real core ====================

mod daemon_cases {
    use super::daemon_e2e::*;
    use super::*;

    /// A stand-in for Discord's `GET /users/@me`: the one token it knows is a bot with `id`.
    async fn fake_discord_api(token: &'static str, id: &'static str) -> String {
        let app = axum::Router::new().route(
            "/users/@me",
            axum::routing::get(move |headers: axum::http::HeaderMap| async move {
                let ok = headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some(&format!("Bot {token}"));
                if ok {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({"id": id, "bot": true})),
                    )
                } else {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    /// A stand-in for core's `GET /api/agents` listing `agents`.
    async fn fake_core_api(agents: &'static [&'static str]) -> String {
        let app = axum::Router::new().route(
            "/api/agents",
            axum::routing::get(move || async move {
                axum::Json(serde_json::Value::Array(
                    agents
                        .iter()
                        .map(|a| serde_json::json!({"id": a, "name": a}))
                        .collect(),
                ))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    const NEW_AGENT: &str = "agent-new-bot";
    const NEW_BOT: &str = "555555555555555555";
    const OWNER: &str = "222222222222222222";

    /// Test 1: a bot for an agent that has none, created from the settings HTTP API with no
    /// credential configured anywhere, comes up and answers its owner.
    #[tokio::test]
    async fn d1099_a_new_bot_created_from_the_settings_page_answers_its_owner() {
        let buf = install_capture();
        let w = world(&[NEW_AGENT], |_, _, _| {}).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(opencrab_discord_gateway::settings_http::serve_with(
            listener,
            w.admin_socket.clone(),
            w.dir.path().join("voice/settings.json"),
            opencrab_discord_gateway::settings_http::DiscordApi::new(
                fake_discord_api("new-bot-token", NEW_BOT).await,
            ),
            Some(fake_core_api(&[NEW_AGENT]).await),
        ));
        let http = reqwest::Client::new();
        let agents: serde_json::Value = http
            .get(format!("{base}/api/agents"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(agents["agents"][0]["has_bot"], false, "{agents}");

        let created = http
            .post(format!("{base}/api/instances"))
            .json(
                &serde_json::json!({"agent_id": NEW_AGENT, "credential": "new-bot-token",
                "owners": [OWNER], "guild_id": GUILD, "channel_id": "910"}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 200);
        let created: serde_json::Value = created.json().await.unwrap();
        let instance_id = created["instance_id"].as_str().unwrap().to_string();
        assert_eq!(created["self_bot_id"], NEW_BOT);

        // The daemon starts the bot; its child says hello and core finds or creates the
        // instance (D-1099: nothing was registered in core before).
        let address = format!("discord-{NEW_AGENT}-{GUILD}-910");
        w.until(|w| w.client_of(&instance_id).is_some()).await;
        assert!(w.bound(&instance_id, &address).await.is_some(), "connected");
        let instances: i64 = w
            .core
            .extgate
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM gate_instances WHERE instance_id=?1",
                [&instance_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(instances, 1, "the hello created the instance in core");

        // The owner speaks; the bot answers in the channel.
        w.say(NEW_AGENT, "9101", "910", OWNER, &format!("{M_SAY} hi"), &[]);
        assert!(
            posted(&buf, "say", "910", B_SAY).await,
            "{:?}",
            captured(&buf)
        );
        let agents: serde_json::Value = http
            .get(format!("{base}/api/agents"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(agents["agents"][0]["has_bot"], true);
        w.daemon.shutdown().await;
    }

    fn legacy_placement(
        dir: &std::path::Path,
        agent: &str,
        instance_id: &str,
        channel: &str,
    ) -> std::path::PathBuf {
        use base64::Engine as _;
        let config = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({"agent_id": agent,
                "self_bot_id": "111111111111111111", "access": {"owners": [OWNER]}}))
            .unwrap(),
        );
        let path = dir.join(format!("{agent}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"core_socket": "/tmp/core.sock",
                "instances": [{"instance_id": instance_id, "revision": 3,
                    "addresses": [format!("discord-{agent}-{GUILD}-{channel}")],
                    "config_b64": config}]}))
            .unwrap(),
        )
        .unwrap();
        path
    }

    fn secret_file(path: &std::path::Path, value: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, value).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    const LEGACY: [&str; 3] = ["agent-kairo", "agent-nostaro", "agent-labomi"];

    /// Test 2: the 3 instance-mode bots are imported under their existing v4 ids, say hello under
    /// those ids, and each one answers its owner.
    #[tokio::test]
    async fn d1099_three_imported_bots_keep_their_ids_and_answer() {
        let buf = install_capture();
        let ids: Vec<String> = LEGACY
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let ids_for_store = ids.clone();
        let w = world(&LEGACY, move |_, store, dir| {
            let tokens = dir.join("tokens");
            std::fs::create_dir(&tokens).unwrap();
            let mut placements = Vec::new();
            for (i, (agent, id)) in LEGACY.iter().zip(&ids_for_store).enumerate() {
                placements.push(legacy_placement(dir, agent, id, &format!("92{i}")));
                secret_file(
                    &tokens.join(format!("{agent}.token")),
                    &format!("token-{agent}"),
                );
            }
            let outcome = opencrab_discord_gateway::legacy_import::import_legacy(
                store,
                &opencrab_discord_gateway::legacy_import::LegacyImport {
                    placements,
                    token_dir: tokens,
                },
                &KEY,
            )
            .unwrap();
            assert_eq!(
                outcome,
                opencrab_discord_gateway::legacy_import::ImportOutcome::Imported(3)
            );
        })
        .await;
        w.until(|w| ids.iter().all(|id| w.client_of(id).is_some()))
            .await;
        for (i, (agent, id)) in LEGACY.iter().zip(&ids).enumerate() {
            let parsed = uuid::Uuid::parse_str(id).unwrap();
            assert_eq!(parsed.get_version_num(), 4);
            assert!(
                w.bound(id, &format!("discord-{agent}-{GUILD}-92{i}"))
                    .await
                    .is_some(),
                "{agent} connected under its existing id"
            );
            let channel = format!("92{i}");
            w.say(
                agent,
                &format!("93{i}"),
                &channel,
                OWNER,
                &format!("{M_SAY} hi"),
                &[],
            );
            assert!(
                posted(&buf, "say", &channel, B_SAY).await,
                "{agent} answers: {:?}",
                captured(&buf)
            );
        }
        w.daemon.shutdown().await;
    }

    /// Test 3: an owner added from the admin socket is an owner from the next message on.
    #[tokio::test]
    async fn d1099_an_owner_edit_applies_live() {
        let buf = install_capture();
        const AGENT: &str = "agent-owner-edit";
        const NEW_OWNER: &str = "333333333333333333";
        let w = world(&[AGENT], |_, _, _| {}).await;
        let created = admin_call(
            &w.admin_socket,
            serde_json::json!({"id":"c","op":"create","agent_id": AGENT,"credential":"t",
                "self_bot_id":"111111111111111111","owners":[OWNER],
                "addresses":[format!("discord-{AGENT}-{GUILD}-940")]}),
        )
        .await;
        let id = created["result"]["instance_id"]
            .as_str()
            .unwrap()
            .to_string();
        w.until(|w| w.client_of(&id).is_some()).await;
        let first = w.client_of(&id).unwrap();

        // Before the edit the new owner has no owner rights: no owner mention can enable a channel.
        let edited = admin_call(
            &w.admin_socket,
            serde_json::json!({"id":"e","op":"set_config","instance_id": id,
                "config": {"agent_id": AGENT, "self_bot_id": "111111111111111111",
                    "access": {"owners": [OWNER, NEW_OWNER]}}}),
        )
        .await;
        assert_eq!(edited["ok"], true, "{edited}");
        // The daemon rebuilds only this bot with the new access.
        w.until(|w| w.client_of(&id).is_some_and(|c| !Arc::ptr_eq(&c, &first)))
            .await;
        let address = format!("discord-{AGENT}-{GUILD}-940");
        assert!(w.bound(&id, &address).await.is_some(), "connected");
        // The new owner's mention in a new channel enables it: only an owner can.
        w.say(
            AGENT,
            "9401",
            "941",
            NEW_OWNER,
            &format!("{M_SAY} hi"),
            &["111111111111111111"],
        );
        assert!(
            posted(&buf, "say", "941", B_SAY).await,
            "{:?}",
            captured(&buf)
        );
        let row = w.daemon.store().lock().unwrap().get(&id).unwrap().unwrap();
        assert!(row
            .addresses
            .contains(&format!("discord-{AGENT}-{GUILD}-941")));
        w.daemon.shutdown().await;
    }

    /// Test 4: an owner @mention in a channel the bot does not have enables it, the message is
    /// answered there, and the channel is still in the gateway store after a restart.
    #[tokio::test]
    async fn d1099_owner_mention_enables_a_channel_that_survives_a_restart() {
        let buf = install_capture();
        const AGENT: &str = "agent-mention";
        let w = world(&[AGENT], |_, _, _| {}).await;
        let created = admin_call(
            &w.admin_socket,
            serde_json::json!({"id":"c","op":"create","agent_id": AGENT,"credential":"t",
                "self_bot_id":"111111111111111111","owners":[OWNER],"addresses":[]}),
        )
        .await;
        assert_eq!(created["ok"], true, "{created}");
        let id = created["result"]["instance_id"]
            .as_str()
            .unwrap()
            .to_string();
        w.until(|w| w.client_of(&id).is_some()).await;

        w.say(
            AGENT,
            "9501",
            "950",
            "444444444444444444",
            "hi",
            &["111111111111111111"],
        );
        w.say(AGENT, "9502", "951", OWNER, "hi", &[]);
        w.say(
            AGENT,
            "9503",
            "952",
            OWNER,
            &format!("{M_SAY} hi"),
            &["111111111111111111"],
        );
        assert!(
            posted(&buf, "say", "952", B_SAY).await,
            "{:?}",
            captured(&buf)
        );
        let address = format!("discord-{AGENT}-{GUILD}-952");
        for _ in 0..250 {
            if w.daemon
                .store()
                .lock()
                .unwrap()
                .get(&id)
                .unwrap()
                .unwrap()
                .addresses
                .contains(&address)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = w.daemon.store().lock().unwrap().get(&id).unwrap().unwrap();
        assert_eq!(
            row.addresses,
            vec![address.clone()],
            "only the owner's mention enabled one"
        );

        // Restart: a fresh store on the same file still listens to the enabled channel.
        w.daemon.shutdown().await;
        let db = w.dir.path().join("discord.db");
        let store = opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap();
        assert_eq!(
            store.get(&id).unwrap().unwrap().addresses,
            vec![address],
            "the enabled channel survives the restart (gateway state)"
        );
    }
}

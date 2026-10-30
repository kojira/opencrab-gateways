// ==================== D-1099: sessions core already holds, through the Discord daemon ====================
//
// The gateway names a session key (the channel address); core continues the session it already
// has for that key (migrated rows keep theirs). These cases check that the bot answers in the
// session the address already had, with nothing registered in core beforehand.

mod daemon_bindings {
    use super::daemon_e2e::*;
    use super::*;

    const OWNER: &str = "222222222222222222";
    const BOT: &str = "111111111111111111";

    /// Core's turn logs of `session_id` (each answered message runs one turn there).
    fn turns_in(w: &World, session_id: &str) -> i64 {
        w.core
            .extgate
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM llm_logs WHERE session_id=?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    async fn turns_reach(w: &World, session_id: &str, at_least: i64) -> bool {
        for _ in 0..250 {
            if turns_in(w, session_id) >= at_least {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// A core session record in the shape the pre-D-1098 migration left: a row id no gateway
    /// derives (v4), session = the key, a title other than the default. The instance row is
    /// what the migrated production DB holds.
    fn seed_migrated_session(core: &Core, instance_id: &str, row_id: &str, key: &str) {
        let mut conn = core.extgate.db.lock().unwrap();
        let tx = conn.transaction().unwrap();
        let subject: i64 = tx
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id=(?1)",
                [agent_of(key)],
                |r| r.get(0),
            )
            .unwrap();
        tx.execute(
            "INSERT INTO gate_instances (instance_id, kind_id, subject_id, revision, enabled,
                config_b64, config_digest, created_at, updated_at, deleted_at)
             VALUES (?1, 'discord', ?2, 3, 1, 'e30=', ?3, 1, 1, NULL)
             ON CONFLICT(instance_id) DO NOTHING",
            rusqlite::params![instance_id, subject, opencrab_extgate::config_digest(b"{}")],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO sessions (id, title, created_at, updated_at)
             VALUES (?1, 'migrated title', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [key],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
            rusqlite::params![agent_of(key), key],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
             VALUES (?1, ?2, ?3, 1, NULL, ?3)",
            rusqlite::params![row_id, instance_id, key],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn agent_of(address: &str) -> String {
        LEGACY
            .iter()
            .find(|agent| address.starts_with(&format!("discord-{agent}-")))
            .unwrap()
            .to_string()
    }

    const LEGACY: [&str; 3] = ["agent-kairo", "agent-nostaro", "agent-labomi"];

    fn legacy_placement(
        dir: &std::path::Path,
        agent: &str,
        instance_id: &str,
        address: &str,
    ) -> std::path::PathBuf {
        use base64::Engine as _;
        let config = base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({"agent_id": agent,
                "self_bot_id": BOT, "access": {"owners": [OWNER]}}))
            .unwrap(),
        );
        let path = dir.join(format!("{agent}.json"));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"core_socket": "/tmp/core.sock",
                "instances": [{"instance_id": instance_id, "revision": 3,
                    "addresses": [address], "config_b64": config}]}))
            .unwrap(),
        )
        .unwrap();
        path
    }

    /// (a) Production today: core already holds the 3 bots' channels as migrated session
    /// records. The imported bots key their saids by those addresses and each answers in the
    /// session it already had; core keeps the same record.
    #[tokio::test]
    async fn d1099_imported_bots_continue_their_migrated_sessions() {
        let buf = install_capture();
        let ids: Vec<String> = LEGACY
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let migrated: Vec<String> = LEGACY
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let addresses: Vec<String> = (0..3)
            .map(|i| format!("discord-{}-{GUILD}-96{i}", LEGACY[i]))
            .collect();
        let (ids_seed, migrated_seed, addresses_seed) =
            (ids.clone(), migrated.clone(), addresses.clone());
        let w = world(&LEGACY, move |core, store, dir| {
            let tokens = dir.join("tokens");
            std::fs::create_dir(&tokens).unwrap();
            let mut placements = Vec::new();
            for i in 0..3 {
                seed_migrated_session(core, &ids_seed[i], &migrated_seed[i], &addresses_seed[i]);
                placements.push(legacy_placement(
                    dir,
                    LEGACY[i],
                    &ids_seed[i],
                    &addresses_seed[i],
                ));
                let token = tokens.join(format!("{}.token", LEGACY[i]));
                std::fs::write(&token, "t").unwrap();
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            opencrab_discord_gateway::legacy_import::import_legacy(
                store,
                &opencrab_discord_gateway::legacy_import::LegacyImport {
                    placements,
                    token_dir: tokens,
                },
                &KEY,
            )
            .unwrap();
        })
        .await;
        for i in 0..3 {
            assert!(w.bound(&ids[i], &addresses[i]).await.is_some(), "connected");
            let channel = format!("96{i}");
            w.say(
                LEGACY[i],
                &format!("970{i}"),
                &channel,
                OWNER,
                &format!("{M_SAY} hi"),
                &[],
            );
            assert!(
                posted(&buf, "say", &channel, B_SAY).await,
                "{} answers",
                LEGACY[i]
            );
            assert!(
                turns_reach(&w, &addresses[i], 1).await,
                "the turn ran in the existing session"
            );
            assert_eq!(
                w.core_binding(&addresses[i]),
                Some((migrated[i].clone(), addresses[i].clone())),
                "core kept the migrated record (no second one)"
            );
        }
        w.daemon.shutdown().await;
    }

    /// (b) Removing a channel in the settings API and adding it back keeps its session: the bot
    /// stops answering there, then answers again and the turns continue in the same session.
    #[tokio::test]
    async fn d1099_a_channel_removed_and_added_back_keeps_its_session() {
        let buf = install_capture();
        const AGENT: &str = "agent-readd";
        let w = world(&[AGENT], |_, _, _| {}).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(opencrab_discord_gateway::settings_http::serve_with(
            listener,
            w.admin_socket.clone(),
            w.dir.path().join("voice/settings.json"),
            opencrab_discord_gateway::settings_http::DiscordApi::new("http://127.0.0.1:9"),
            None,
        ));
        let keep = format!("discord-{AGENT}-{GUILD}-980");
        let readd = format!("discord-{AGENT}-{GUILD}-981");
        let created = admin_call(
            &w.admin_socket,
            serde_json::json!({"id":"c","op":"create","agent_id": AGENT,"credential":"t",
                "self_bot_id": BOT,"owners":[OWNER],"addresses":[keep, readd]}),
        )
        .await;
        let id = created["result"]["instance_id"]
            .as_str()
            .unwrap()
            .to_string();
        w.bound(&id, &readd).await.expect("connected");
        w.say(AGENT, "9801", "981", OWNER, &format!("{M_SAY} one"), &[]);
        assert!(posted(&buf, "say", "981", B_SAY).await);
        let (first, session) = w.core_binding(&readd).expect("core opened the session");
        assert!(turns_reach(&w, &session, 1).await);

        let http = reqwest::Client::new();
        let removed = http
            .put(format!("{base}/api/instances/{id}/addresses"))
            .json(&serde_json::json!({"addresses": [keep]}))
            .send()
            .await
            .unwrap();
        assert_eq!(removed.status(), 200);
        w.until(|w| {
            w.daemon
                .store()
                .lock()
                .unwrap()
                .get(&id)
                .unwrap()
                .unwrap()
                .lifecycle_state
                == opencrab_process_supervisor::lifecycle::LifecycleState::Running
        })
        .await;
        // The gateway no longer listens there; core's session record is untouched.
        assert_eq!(w.core_binding(&readd), Some((first.clone(), session.clone())));

        let added = http
            .post(format!("{base}/api/instances/{id}/channels"))
            .json(&serde_json::json!({"guild_id": GUILD, "channel_id": "981"}))
            .send()
            .await
            .unwrap();
        assert_eq!(added.status(), 200);
        loop_bound(&w, &id, &readd).await;
        assert_eq!(
            w.core_binding(&readd),
            Some((first.clone(), session.clone())),
            "the same session record"
        );
        w.say(AGENT, "9802", "981", OWNER, &format!("{M_SAY} two"), &[]);
        assert!(
            turns_reach(&w, &session, 2).await,
            "the turn continues the same session"
        );
        w.daemon.shutdown().await;
    }

    /// Waits until the restarted child listens to `address` again.
    async fn loop_bound(w: &World, id: &str, address: &str) -> String {
        for _ in 0..250 {
            w.daemon.reconcile_once().await.unwrap();
            let row = w.daemon.store().lock().unwrap().get(id).unwrap().unwrap();
            if row.addresses.iter().any(|a| a == address)
                && row.lifecycle_state
                    == opencrab_process_supervisor::lifecycle::LifecycleState::Running
            {
                if let Some(bound) = w.bound(id, address).await {
                    return bound;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the channel was not listened to again");
    }

    /// (c) A real daemon restart: a channel the owner enabled by @mention is still in the store,
    /// the new daemon's child listens to it and the bot answers there in the same session.
    #[tokio::test]
    async fn d1099_after_a_daemon_restart_the_mention_enabled_channel_answers() {
        let buf = install_capture();
        const AGENT: &str = "agent-restart";
        let mut w = world(&[AGENT], |_, _, _| {}).await;
        let created = admin_call(
            &w.admin_socket,
            serde_json::json!({"id":"c","op":"create","agent_id": AGENT,"credential":"t",
                "self_bot_id": BOT,"owners":[OWNER],"addresses":[]}),
        )
        .await;
        let id = created["result"]["instance_id"]
            .as_str()
            .unwrap()
            .to_string();
        w.until(|w| w.client_of(&id).is_some()).await;
        w.say(AGENT, "9901", "990", OWNER, &format!("{M_SAY} hi"), &[BOT]);
        assert!(posted(&buf, "say", "990", B_SAY).await);
        let address = format!("discord-{AGENT}-{GUILD}-990");
        let (binding, session) = w.core_binding(&address).expect("enabled by the mention");
        assert!(turns_reach(&w, &session, 1).await);
        let started_before = w.clients.lock().unwrap().len();
        let turns_before = turns_in(&w, &session);

        // The old process is gone; the new daemon finds the child lost and starts a new one on
        // the stored channels.
        // Only what arrives after the restart is in the event stream of the new child.
        std::fs::write(&w.events[AGENT], "").unwrap();
        w.restart_daemon().await;
        w.until(|w| w.clients.lock().unwrap().len() > started_before)
            .await;
        assert!(w.bound(&id, &address).await.is_some(), "listens again");
        assert_eq!(w.core_binding(&address), Some((binding, session.clone())));
        w.say(AGENT, "9902", "990", OWNER, &format!("{M_SAY} again"), &[]);
        assert!(
            turns_reach(&w, &session, turns_before + 1).await,
            "answered again in the same session"
        );
        w.daemon.shutdown().await;
    }
}

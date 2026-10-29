// ==================== D-1098: the Discord daemon against a real core ====================
//
// Real pieces: core (extgate + AppState + mock LLM) on its gate socket, the Discord daemon with
// the real `ProvisionReconciler`, its admin socket, the settings HTTP API, and the real child
// code (`spawn_instance_with_voice` reading the placement the daemon wrote). The child runs in
// this process with a fake event file and the dry-run transport instead of serenity, which is
// the only stand-in: no Discord. No gate-admin credential exists anywhere.

mod daemon_e2e {
    use super::*;
    use opencrab_discord_gateway::daemon::{
        DiscordDaemon, ProductionFactory, ProvisionReconciler, SpawnerFactory,
    };
    use opencrab_discord_gateway::store::{DiscordStore, InstanceRow};
    use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
    use std::path::PathBuf;

    pub(crate) const KEY: [u8; 32] = [7; 32];

    /// Every child the daemon started, with its instance id, oldest first.
    pub(crate) type StartedChildren = Arc<Mutex<Vec<(String, Arc<InstanceClient>)>>>;

    /// The real child (`main`'s path), in this process: reads the daemon-written placement,
    /// connects to core and sends `hello` with the provisioned revision and digest.
    pub(crate) struct InProcessFactory {
        pub(crate) production: ProductionFactory,
        /// Events per agent: what "Discord" delivers to that bot.
        pub(crate) events: std::collections::BTreeMap<String, PathBuf>,
        pub(crate) clients: StartedChildren,
    }

    /// The child's own runtime: killing it drops every task and the core connection, like
    /// a process exit.
    struct Child {
        stop: Option<std::sync::mpsc::Sender<()>>,
        exited: Option<tokio::sync::oneshot::Receiver<()>>,
    }
    #[async_trait::async_trait]
    impl SupervisedChild for Child {
        async fn wait_exit(&mut self) -> String {
            if let Some(exited) = self.exited.take() {
                let _ = exited.await;
            }
            "killed".into()
        }
        async fn kill(&mut self) {
            self.stop.take();
        }
        /// A real child has a pid; the daemon records the start only with one.
        fn pid(&self) -> Option<u32> {
            Some(std::process::id())
        }
    }

    struct Spawner {
        target: String,
        placement: PathBuf,
        events: PathBuf,
        clients: StartedChildren,
    }

    #[async_trait::async_trait]
    impl ChildSpawner for Spawner {
        async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
            let place = opencrab_discord_gateway::config::Placement::load(&self.placement)
                .map_err(std::io::Error::other)?;
            let mut inst = place.instances[0].clone();
            inst.channel_socket = place.channel_socket.as_deref().map(PathBuf::from);
            let bytes =
                opencrab_discord_gateway::config::decode_config_b64(&inst.config_b64).unwrap();
            let events = self.events.clone();
            let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
            let (client_tx, client_rx) = std::sync::mpsc::channel();
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                let client = rt.block_on(async {
                    opencrab_discord_gateway::run::spawn_instance_with_voice(
                        PathBuf::from(&place.core_socket),
                        &inst,
                        &bytes,
                        None,
                        HarnessOverrides {
                            fake_events: Some(events),
                            dry_run: true,
                        },
                        (None, None),
                        None,
                        None,
                        Default::default(),
                    )
                });
                let _ = client_tx.send(client.map_err(|e| e.to_string()));
                let _ = stop_rx.recv();
                rt.shutdown_timeout(Duration::from_secs(1));
                let _ = exit_tx.send(());
            });
            let client = client_rx
                .recv()
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)?;
            self.clients
                .lock()
                .unwrap()
                .push((self.target.clone(), client));
            Ok(Box::new(Child {
                stop: Some(stop_tx),
                exited: Some(exit_rx),
            }))
        }
        fn target_id(&self) -> &str {
            &self.target
        }
    }

    impl SpawnerFactory for InProcessFactory {
        fn cleanup_artifacts(&self, row: &InstanceRow) -> anyhow::Result<()> {
            self.production.cleanup_artifacts(row)
        }
        fn channel_socket(&self, row: &InstanceRow) -> Option<PathBuf> {
            self.production.channel_socket(row)
        }
        fn for_instance(
            &self,
            row: &InstanceRow,
            credential: zeroize::Zeroizing<Vec<u8>>,
            nonce: &str,
        ) -> anyhow::Result<Arc<dyn ChildSpawner>> {
            // Writes the real placement exactly as production does.
            self.production.for_instance(row, credential, nonce)?;
            Ok(Arc::new(Spawner {
                target: row.instance_id.clone(),
                placement: self
                    .production
                    .placement_dir
                    .join(format!("{}.json", row.instance_id)),
                events: self.events[&row.agent_id].clone(),
                clients: self.clients.clone(),
            }))
        }
    }

    pub(crate) struct World {
        pub(crate) core: Core,
        pub(crate) dir: tempfile::TempDir,
        pub(crate) daemon: Arc<DiscordDaemon<ProvisionReconciler, InProcessFactory>>,
        pub(crate) admin_socket: PathBuf,
        pub(crate) clients: StartedChildren,
        pub(crate) events: std::collections::BTreeMap<String, PathBuf>,
    }

    pub(crate) fn add_agent(core: &Core, agent_id: &str) {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: agent_id.into(),
                name: agent_id.into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    /// A daemon on a real core, with an event file per agent in `agents`.
    pub(crate) async fn world(
        agents: &[&str],
        store: impl FnOnce(&DiscordStore, &std::path::Path),
    ) -> World {
        let core = start_core(Arc::new(RoutedMock::new()) as Arc<dyn LlmProvider>).await;
        for agent in agents {
            add_agent(&core, agent);
        }
        let dir = tempfile::Builder::new()
            .prefix("dd")
            .tempdir_in("/tmp")
            .unwrap();
        let db = dir.path().join("discord.db");
        let discord_store = DiscordStore::open(&db).unwrap();
        store(&discord_store, dir.path());
        let mut events = std::collections::BTreeMap::new();
        for agent in agents {
            let path = dir.path().join(format!("{agent}.jsonl"));
            std::fs::write(&path, "").unwrap();
            events.insert(agent.to_string(), path);
        }
        let clients = Arc::new(Mutex::new(Vec::new()));
        let factory = InProcessFactory {
            production: ProductionFactory {
                child_binary: "/bin/true".into(),
                core_socket: core.sock.clone(),
                placement_dir: dir.path().join("p"),
                attachment_spool_root: None,
                attachment_outbox_root: None,
                database_path: db.clone(),
                voice_settings: dir.path().join("voice/settings.json"),
            },
            events: events.clone(),
            clients: clients.clone(),
        };
        let daemon = Arc::new(
            DiscordDaemon::new(
                discord_store,
                KEY,
                Arc::new(ProvisionReconciler::new(core.sock.clone(), "discord")),
                Arc::new(factory),
            )
            .unwrap(),
        );
        daemon.serve_channel_requests(Arc::new(opencrab_discord_gateway::daemon::DaemonChannels(
            Arc::downgrade(&daemon),
        )));
        let admin_socket = dir.path().join("admin.sock");
        opencrab_discord_gateway::admin::spawn(admin_socket.clone(), daemon.store(), Arc::new(KEY));
        for _ in 0..200 {
            if admin_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        World {
            core,
            dir,
            daemon,
            admin_socket,
            clients,
            events,
        }
    }

    impl World {
        /// Runs the daemon loop until `done` holds.
        pub(crate) async fn until(&self, done: impl Fn(&World) -> bool) {
            for _ in 0..200 {
                self.daemon.reconcile_once().await.unwrap();
                if done(self) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("the daemon did not reach the expected state");
        }

        pub(crate) fn client_of(&self, instance_id: &str) -> Option<Arc<InstanceClient>> {
            self.clients
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(id, _)| id == instance_id)
                .map(|(_, c)| c.clone())
        }

        pub(crate) fn say(
            &self,
            agent: &str,
            id: &str,
            channel: &str,
            author: &str,
            text: &str,
            mentions: &[&str],
        ) {
            use std::io::Write as _;
            let line = serde_json::json!({"id": id, "channel_id": channel, "guild_id": GUILD,
                "author": {"id": author, "bot": false, "username": "u"},
                "content": text, "mentions": mentions});
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.events[agent])
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }
    }

    pub(crate) async fn admin_call(
        socket: &std::path::Path,
        request: serde_json::Value,
    ) -> serde_json::Value {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut line)
            .await
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// Whether a dry-run post of `kind` with `body` reached `channel`.
    pub(crate) async fn posted(
        buf: &Arc<Mutex<Vec<Captured>>>,
        kind: &str,
        channel: &str,
        body: &str,
    ) -> bool {
        let buf = buf.clone();
        let (kind, channel, body) = (kind.to_string(), channel.to_string(), body.to_string());
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == kind && c.channel == channel && c.body.contains(&body))
        })
        .await
    }
}

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
    async fn d1098_a_new_bot_created_from_the_settings_page_answers_its_owner() {
        let buf = install_capture();
        let w = world(&[NEW_AGENT], |_, _| {}).await;
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

        // The daemon provisions it in core and starts the bot; its child binds the channel.
        let address = format!("discord-{NEW_AGENT}-{GUILD}-910");
        w.until(|w| w.client_of(&instance_id).is_some()).await;
        let client = w.client_of(&instance_id).unwrap();
        for _ in 0..250 {
            if client.binding_for_address(&address).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            client.binding_for_address(&address).await.is_some(),
            "bound in core"
        );
        let (kind, config): (String, String) = {
            let conn = w.core.extgate.db.lock().unwrap();
            conn.query_row(
                "SELECT kind_id, config_b64 FROM gate_instances WHERE instance_id=?1",
                [&instance_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(kind, "discord");
        assert_eq!(
            config,
            opencrab_discord_gateway::daemon::DEFAULT_CORE_CONFIG_B64
        );

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

    /// Test 2: the 3 instance-mode bots are imported under their existing v4 ids, provisioned
    /// in core as they are, and each one answers its owner.
    #[tokio::test]
    async fn d1098_three_imported_bots_keep_their_ids_and_answer() {
        let buf = install_capture();
        let ids: Vec<String> = LEGACY
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let ids_for_store = ids.clone();
        let w = world(&LEGACY, move |store, dir| {
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
            let kind: String = w
                .core
                .extgate
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT kind_id FROM gate_instances WHERE instance_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(kind, "discord", "{agent} provisioned under its existing id");
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
    async fn d1098_an_owner_edit_applies_live() {
        let buf = install_capture();
        const AGENT: &str = "agent-owner-edit";
        const NEW_OWNER: &str = "333333333333333333";
        let w = world(&[AGENT], |_, _| {}).await;
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
        let client = w.client_of(&id).unwrap();
        let address = format!("discord-{AGENT}-{GUILD}-940");
        for _ in 0..250 {
            if client.binding_for_address(&address).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
    /// answered there, and the channel is still provisioned after the daemon restarts.
    #[tokio::test]
    async fn d1098_owner_mention_enables_a_channel_that_survives_a_restart() {
        let buf = install_capture();
        const AGENT: &str = "agent-mention";
        let w = world(&[AGENT], |_, _| {}).await;
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

        // Restart: a fresh daemon on the same store provisions the recorded channel again.
        w.daemon.shutdown().await;
        let db = w.dir.path().join("discord.db");
        let store = opencrab_discord_gateway::store::DiscordStore::open(&db).unwrap();
        let generation = store.get(&id).unwrap().unwrap().desired_generation;
        store.mark_pending(&id, generation).unwrap();
        let reconciler = opencrab_discord_gateway::daemon::ProvisionReconciler::new(
            w.core.sock.clone(),
            "discord",
        );
        use opencrab_discord_gateway::daemon::GateReconciler as _;
        // The child is gone (shutdown), so core accepts a provision of the stored state.
        let mut verified = None;
        for _ in 0..100 {
            match reconciler
                .reconcile(&store.get(&id).unwrap().unwrap())
                .await
            {
                Ok(v) => {
                    verified = Some(v);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        let verified = verified.expect("provisioned again after the restart");
        assert_eq!(verified.bindings.len(), 1);
        let open: i64 = w.core.extgate.db.lock().unwrap()
            .query_row(
                "SELECT COUNT(*) FROM gate_bindings WHERE instance_id=?1 AND address=?2 AND closed_at IS NULL",
                rusqlite::params![id, address],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 1, "the enabled channel stays bound in core");
    }
}

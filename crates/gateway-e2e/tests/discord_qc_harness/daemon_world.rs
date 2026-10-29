// ==================== D-1098: the Discord daemon against a real core ====================
//
// Real pieces: core (extgate + AppState + mock LLM) on its gate socket, the Discord daemon with
// the real `ProvisionReconciler`, its admin socket, the settings HTTP API, and the real child
// code (`spawn_instance_with_voice` reading the placement the daemon wrote). The child runs in
// this process with a fake event file and the dry-run transport instead of serenity, which is
// the only stand-in: no Discord. No core credential is configured anywhere.

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

    /// Instance ids whose in-process child is alive (what a process table says for a real one).
    pub(crate) type LiveChildren = Arc<Mutex<std::collections::BTreeSet<String>>>;

    /// The real child (`main`'s path), in this process: reads the daemon-written placement,
    /// connects to core and sends `hello` with the provisioned revision and digest, then reports
    /// ready on the control socket like `main`.
    pub(crate) struct InProcessFactory {
        pub(crate) production: ProductionFactory,
        /// Events per agent: what "Discord" delivers to that bot.
        pub(crate) events: std::collections::BTreeMap<String, PathBuf>,
        pub(crate) clients: StartedChildren,
        pub(crate) live: LiveChildren,
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
        live: LiveChildren,
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
            let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&self.placement)?)
                .map_err(std::io::Error::other)?;
            let control = raw["control_socket"].as_str().map(PathBuf::from);
            let nonce = raw["start_nonce"].as_str().map(str::to_owned);
            let (live, target) = (self.live.clone(), self.target.clone());
            live.lock().unwrap().insert(target.clone());
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
                if let (Ok(client), Some(control), Some(nonce)) = (&client, control, nonce) {
                    // `main`'s readiness report: once every placed address is bound.
                    let (client, addresses) = (client.clone(), inst.addresses.clone());
                    rt.spawn(async move {
                        use tokio::io::AsyncWriteExt as _;
                        loop {
                            let mut all = client.connection_live().await;
                            for address in &addresses {
                                all = all && client.remembered_binding(address).await.is_some();
                            }
                            if all {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        if let Ok(mut stream) = tokio::net::UnixStream::connect(control).await {
                            let message =
                                serde_json::json!({"nonce": nonce, "pid": std::process::id()});
                            let _ = stream.write_all(message.to_string().as_bytes()).await;
                        }
                    });
                }
                let _ = client_tx.send(client.map_err(|e| e.to_string()));
                let _ = stop_rx.recv();
                rt.shutdown_timeout(Duration::from_secs(1));
                live.lock().unwrap().remove(&target);
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
        fn observe_process(
            &self,
            row: &InstanceRow,
        ) -> anyhow::Result<opencrab_process_supervisor::lifecycle::ProcessObservation> {
            use opencrab_process_supervisor::lifecycle::ProcessObservation;
            Ok(if self.live.lock().unwrap().contains(&row.instance_id) {
                ProcessObservation::StaleLive
            } else {
                ProcessObservation::Missing
            })
        }
        fn control_socket(&self, row: &InstanceRow, nonce: &str) -> Option<PathBuf> {
            self.production.control_socket(row, nonce)
        }
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
                live: self.live.clone(),
            }))
        }
    }

    pub(crate) struct World {
        pub(crate) core: Core,
        pub(crate) dir: tempfile::TempDir,
        pub(crate) daemon: Arc<DiscordDaemon<ProvisionReconciler, InProcessFactory>>,
        pub(crate) admin: tokio::task::JoinHandle<anyhow::Result<()>>,
        pub(crate) admin_socket: PathBuf,
        pub(crate) clients: StartedChildren,
        pub(crate) live: LiveChildren,
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

    /// The daemon of one gateway process: store, reconciler, children, channel listeners and
    /// admin socket, all on the files under `dir`.
    pub(crate) fn start_daemon(
        core: &Core,
        dir: &std::path::Path,
        events: &std::collections::BTreeMap<String, PathBuf>,
        (clients, live): (&StartedChildren, &LiveChildren),
    ) -> (
        Arc<DiscordDaemon<ProvisionReconciler, InProcessFactory>>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let db = dir.join("discord.db");
        let factory = InProcessFactory {
            production: ProductionFactory {
                child_binary: "/bin/true".into(),
                core_socket: core.sock.clone(),
                placement_dir: dir.join("p"),
                attachment_spool_root: None,
                attachment_outbox_root: None,
                database_path: db.clone(),
                voice_settings: dir.join("voice/settings.json"),
            },
            events: events.clone(),
            clients: clients.clone(),
            live: live.clone(),
        };
        let daemon = Arc::new(
            DiscordDaemon::new(
                DiscordStore::open(&db).unwrap(),
                KEY,
                Arc::new(ProvisionReconciler::new(core.sock.clone(), "discord")),
                Arc::new(factory),
            )
            .unwrap(),
        );
        daemon.serve_channel_requests(Arc::new(opencrab_discord_gateway::daemon::DaemonChannels(
            Arc::downgrade(&daemon),
        )));
        let admin = opencrab_discord_gateway::admin::spawn(
            dir.join("admin.sock"),
            daemon.store(),
            Arc::new(KEY),
        );
        (daemon, admin)
    }

    /// A daemon on a real core, with an event file per agent in `agents`. `seed` runs before
    /// the daemon starts, with core and the gateway store.
    pub(crate) async fn world(
        agents: &[&str],
        seed: impl FnOnce(&Core, &DiscordStore, &std::path::Path),
    ) -> World {
        let core = start_core(Arc::new(RoutedMock::new()) as Arc<dyn LlmProvider>).await;
        for agent in agents {
            add_agent(&core, agent);
        }
        let dir = tempfile::Builder::new()
            .prefix("dd")
            .tempdir_in("/tmp")
            .unwrap();
        seed(
            &core,
            &DiscordStore::open(&dir.path().join("discord.db")).unwrap(),
            dir.path(),
        );
        let mut events = std::collections::BTreeMap::new();
        for agent in agents {
            let path = dir.path().join(format!("{agent}.jsonl"));
            std::fs::write(&path, "").unwrap();
            events.insert(agent.to_string(), path);
        }
        let clients: StartedChildren = Arc::new(Mutex::new(Vec::new()));
        let live: LiveChildren = Arc::default();
        let (daemon, admin) = start_daemon(&core, dir.path(), &events, (&clients, &live));
        let admin_socket = dir.path().join("admin.sock");
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
            admin,
            admin_socket,
            clients,
            live,
            events,
        }
    }

    impl World {
        /// Stops the gateway process (every child, the admin socket) and starts a new one on
        /// the same store: the new daemon runs its startup recovery like a fresh process.
        pub(crate) async fn restart_daemon(&mut self) {
            self.daemon.shutdown().await;
            self.admin.abort();
            let (daemon, admin) = start_daemon(
                &self.core,
                self.dir.path(),
                &self.events,
                (&self.clients, &self.live),
            );
            self.daemon = daemon;
            self.admin = admin;
            for _ in 0..200 {
                if tokio::net::UnixStream::connect(&self.admin_socket)
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// Core's open binding for `address`: `(binding_id, session_id)`.
        pub(crate) fn core_binding(&self, address: &str) -> Option<(String, String)> {
            use rusqlite::OptionalExtension as _;
            self.core
                .extgate
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT binding_id, session_id FROM gate_bindings
                     WHERE address=?1 AND closed_at IS NULL",
                    [address],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap()
        }

        /// Waits until the running child of `instance_id` has `address` bound in core.
        pub(crate) async fn bound(&self, instance_id: &str, address: &str) -> Option<String> {
            for _ in 0..250 {
                if let Some(client) = self.client_of(instance_id) {
                    if let Some(id) = client.binding_for_address(address).await {
                        return Some(id);
                    }
                }
                self.daemon.reconcile_once().await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            None
        }

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

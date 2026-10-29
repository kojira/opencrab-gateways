// ==================== D-1098: the Nostr daemon provisions and reaches hello ok ====================

mod nostr_daemon_provision {
    use super::*;
    use opencrab_nostr_gateway::daemon::{
        NostrDaemon, ProductionFactory, ProvisionReconciler, SpawnerFactory,
    };
    use opencrab_nostr_gateway::store::{InstanceRow, NostrStore};
    use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};

    struct Child;
    #[async_trait::async_trait]
    impl SupervisedChild for Child {
        async fn wait_exit(&mut self) -> String {
            std::future::pending::<()>().await;
            String::new()
        }
        async fn kill(&mut self) {}
        fn pid(&self) -> Option<u32> {
            Some(std::process::id())
        }
    }

    /// The real child code (`main`'s path) in this process, reading the daemon-written placement.
    struct Spawner {
        target: String,
        placement: PathBuf,
        clients: Arc<Mutex<Vec<Arc<InstanceClient>>>>,
    }
    #[async_trait::async_trait]
    impl ChildSpawner for Spawner {
        async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
            let place = opencrab_nostr_gateway::config::Placement::load(&self.placement)
                .map_err(std::io::Error::other)?;
            let inst = &place.instances[0];
            let bytes = opencrab_nostr_gateway::config::decode_config_b64(&inst.config_b64)
                .map_err(std::io::Error::other)?;
            let client = spawn_instance(
                PathBuf::from(&place.core_socket),
                inst,
                &bytes,
                None,
                opencrab_nostr_gateway::run_env::SecretOwner::ThisInstance,
                PathBuf::from(&place.nostaro_bin),
                HarnessOverrides {
                    fake_watch: Some(self.placement.with_extension("watch")),
                    dry_run: true,
                },
                None,
            )
            .map_err(std::io::Error::other)?;
            self.clients.lock().unwrap().push(client);
            Ok(Box::new(Child))
        }
        fn target_id(&self) -> &str {
            &self.target
        }
    }

    struct InProcess {
        production: ProductionFactory,
        clients: Arc<Mutex<Vec<Arc<InstanceClient>>>>,
    }
    impl SpawnerFactory for InProcess {
        fn for_instance(
            &self,
            row: &InstanceRow,
            credential: zeroize::Zeroizing<Vec<u8>>,
            nonce: &str,
        ) -> anyhow::Result<Arc<dyn ChildSpawner>> {
            self.production.for_instance(row, credential, nonce)?;
            Ok(Arc::new(Spawner {
                target: row.instance_id.clone(),
                placement: self
                    .production
                    .placement_dir
                    .join(format!("{}.json", row.instance_id)),
                clients: self.clients.clone(),
            }))
        }
    }

    /// Test 5: a Nostr instance in the daemon's store is provisioned in core with no gate-admin
    /// credential, its child says `hello` with the provisioned revision and core accepts it
    /// (the address is bound, which core does only after `hello ok`).
    #[tokio::test]
    async fn d1098_a_nostr_instance_is_provisioned_and_reaches_hello_ok() {
        let core = start_core(Arc::new(RoutedMock::new()) as Arc<dyn LlmProvider>).await;
        let dir = tempfile::Builder::new()
            .prefix("nd")
            .tempdir_in("/tmp")
            .unwrap();
        let key = [9_u8; 32];
        let store = NostrStore::open(&dir.path().join("nostr.db")).unwrap();
        let instance_id = uuid::Uuid::new_v4().to_string();
        let address = format!("nostr-{AGENT_ID}");
        store
            .upsert_desired(
                &instance_id,
                AGENT_ID,
                &opencrab_extgate::encode_config_b64(&nostr_config(None)),
                std::slice::from_ref(&address),
                "nsec-not-used-by-the-dry-run-child",
                true,
                &key,
            )
            .unwrap();
        // The settings page keeps who may talk to the bot apart from the runtime config.
        store
            .set_access(
                &instance_id,
                &opencrab_nostr_gateway::config::AccessConfig {
                    owner: vec![author_pk()],
                    ..Default::default()
                },
            )
            .unwrap();
        let clients = Arc::new(Mutex::new(Vec::new()));
        let daemon = NostrDaemon::new(
            store,
            key,
            Arc::new(ProvisionReconciler::new(core.sock.clone(), "nostr")),
            Arc::new(InProcess {
                production: ProductionFactory {
                    child_binary: "/bin/true".into(),
                    core_socket: core.sock.clone(),
                    placement_dir: dir.path().join("p"),
                    nostaro_bin: "/nonexistent/nostaro".into(),
                    attachment_outbox_root: None,
                },
                clients: clients.clone(),
            }),
        )
        .unwrap();
        let mut bound = None;
        for _ in 0..200 {
            daemon.reconcile_once().await.unwrap();
            let client = clients.lock().unwrap().last().cloned();
            if let Some(client) = client {
                if let Some(binding) = client.binding_for_address(&address).await {
                    bound = Some(binding);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let binding_id = bound.expect("hello ok and bound");
        assert_eq!(
            binding_id,
            opencrab_gate_client::provision::binding_id_for(&instance_id, &address).unwrap()
        );
        let (kind, config): (String, String) = core
            .extgate
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT kind_id, config_b64 FROM gate_instances WHERE instance_id=?1",
                [&instance_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "nostr");
        assert_eq!(config, "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==");
        daemon.shutdown().await;
    }
}

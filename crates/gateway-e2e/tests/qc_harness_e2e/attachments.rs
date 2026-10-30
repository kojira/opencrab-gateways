// Production Nostr declarations through the real generic core projector and UDS
// dispatcher. Upload/publisher protocol is covered separately at the Nostr handler.
#[tokio::test]
async fn nostr_workspace_files_project_dispatch_metadata_and_cleanup() {
    use opencrab_extgate::ExtgateOpsGatewayActions;
    use opencrab_gate_client::{
        FinalDelivery, InvokeHandler, InvokeOutcome, RuntimeCapabilities, SayPolicy,
    };
    use opencrab_gateway::{GatewayActions, GatewayCallContext};
    use serde_json::{json, Value};

    struct Capture {
        payload: Arc<Mutex<Option<Value>>>,
        release: Arc<tokio::sync::Notify>,
        uncertain: bool,
    }
    #[async_trait::async_trait]
    impl InvokeHandler for Capture {
        async fn handle(&self, _: &str, _: &str, op: &str, payload: &Value) -> InvokeOutcome {
            assert_eq!(op, "say");
            *self.payload.lock().unwrap() = Some(payload.clone());
            self.release.notified().await;
            if self.uncertain {
                InvokeOutcome::Indeterminate
            } else {
                InvokeOutcome::Ok(Value::Null)
            }
        }
    }
    for uncertain in [false, true] {
        let core = start_core(Arc::new(FifoMock::new())).await;
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let outbox = dir.path().join("outbox");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outbox).unwrap();
        std::fs::write(workspace.join("note.txt"), b"attachment fixture").unwrap();
        core.extgate
            .set_workspace_base(workspace.to_str().unwrap().into());
        core.extgate.set_attachment_outbox_root(outbox.clone());
        let instance = uuid::Uuid::new_v4().to_string();
        let payload = Arc::new(Mutex::new(None));
        let release = Arc::new(tokio::sync::Notify::new());
        let client = InstanceClient::spawn_with_operations(
            core.sock.clone(),
            instance.clone(),
            AGENT_ID.to_string(),
            self_pk(),
            SayPolicy::AcceptToLiveQueue,
            Some(opencrab_nostr_gateway::ops::operation_declarations_with_files(true)),
            RuntimeCapabilities {
                final_delivery: FinalDelivery::OperationDriven,
            },
            Arc::new(Capture {
                payload: payload.clone(),
                release: release.clone(),
                uncertain,
            }),
        );
        assert!(
            wait_until(|| core
                .extgate
                .lock_registry()
                .unwrap()
                .get(&instance)
                .is_some())
            .await
        );
        // D-1099: the gateway's session key opens core's session; core keeps its own row id.
        let session = client.open("attachment-test", None).await.expect("open");
        let binding = opencrab_db::queries::find_gate_session(
            &core.extgate.db.lock().unwrap(),
            &instance,
            "attachment-test",
        )
        .unwrap()
        .expect("core record")
        .row_id;
        let projection = ExtgateOpsGatewayActions::for_binding(
            core.extgate.clone(),
            &instance,
            &binding,
            &session,
            AGENT_ID,
        )
        .unwrap();
        let result = projection
            .execute(
                "say",
                &json!({"text":"here", "files":["note.txt"]}),
                &GatewayCallContext::for_agent(AGENT_ID),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert!(wait_until(|| payload.lock().unwrap().is_some()).await);
        let dispatched = payload.lock().unwrap().clone().unwrap();
        let file = opencrab_gate_client::OutboundFile::parse(&dispatched["files"][0]).unwrap();
        assert_eq!(file.name, "note.txt");
        assert_eq!(
            opencrab_gate_client::open_verified(&outbox, &file).unwrap(),
            b"attachment fixture"
        );
        let logs = opencrab_db::queries::list_session_logs_by_session(
            &core.extgate.db.lock().unwrap(),
            &session,
        )
        .unwrap();
        let speech = logs.iter().find(|log| log.log_type == "speech").unwrap();
        let metadata: Value =
            serde_json::from_str(speech.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(metadata["attachments"], json!([{"name":"note.txt"}]));
        let copy = outbox.join(file.local_path);
        assert!(copy.exists());
        release.notify_one();
        assert!(
            wait_until(|| !copy.exists()).await,
            "terminal response/close must remove staged copy"
        );
        assert!(
            workspace.join("note.txt").exists(),
            "workspace source must survive"
        );
        drop(client);
    }
}

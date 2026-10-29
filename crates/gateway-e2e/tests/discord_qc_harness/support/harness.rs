use super::*;

fn register_mock_pricing(db: &opencrab_db::Db) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: "mock".to_string(),
            model: "gpt-4o".to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(200_000),
            max_output_tokens: Some(4_096),
            cached_input_price_per_1m: None,
            cache_write_price_per_1m: None,
        },
    )
    .expect("test model_pricing");
}

fn upsert_test_agent(db: &opencrab_db::Db) -> i64 {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: AGENT_ID.into(),
            name: "QC".into(),
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
    conn.query_row(
        "SELECT subject_id FROM agents WHERE agent_id = ?1",
        [AGENT_ID],
        |r| r.get(0),
    )
    .unwrap()
}

fn build_app_state(db: opencrab_db::Db, provider: Arc<dyn LlmProvider>) -> AppState {
    let mut router = LlmRouter::new();
    router.add_provider(provider);
    router.set_default_provider("mock");
    AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        workspace_base: std::env::temp_dir()
            .join("opencrab_discord_qc")
            .to_string_lossy()
            .to_string(),
        #[cfg(any())]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    }
}

pub(crate) struct Core {
    pub(crate) extgate: Arc<ExtgateState>,
    pub(crate) sock: PathBuf,
    /// #915: execute_shell を実走させる tools_config 等を per-test で触れるよう AppState を保持。
    pub(crate) state: AppState,
    _dir: tempfile::TempDir,
}

/// #915: echo と sleep だけを許可した shell 有効 tools 設定（date 相当＝echo・sleep 相当＝sleep）。
pub(crate) fn shell_enabled_tools_config() -> opencrab_actions::tools::ToolsConfig {
    opencrab_actions::tools::ToolsConfig {
        enabled: true,
        shell: Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: vec!["echo".to_string(), "sleep".to_string()],
            ..Default::default()
        }),
    }
}

/// 実 serve_uds core + 実 AppState を UDS で立ち上げる。nostr hooks は張らない（discord は generic 経路）。
pub(crate) async fn start_core(provider: Arc<dyn LlmProvider>) -> Core {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    register_mock_pricing(&db);
    upsert_test_agent(&db);
    let extgate = Arc::new(ExtgateState::new(db.clone()));
    let state = build_app_state(db.clone(), provider);
    // #925: 本番と同じ descriptor 登録（`register_production_descriptors`）＋ V3 heartbeat 受け口
    // （`ExtgateTimedFireSink`）を実型で配線する。これで scheduler seam（resolve_target →
    // run_one_heartbeat）が extgate session を解決し発火できる（未登録なら resolve_target None で
    // 配送 0＝赤）。descriptor は本番経路で登録するので、`register_production_descriptors` から
    // ExtgateFire が抜けると本ハーネスの heartbeat も赤になる（配線漏れを捕捉）。
    state
        .timed_fire_router
        .register_sink(Arc::new(opencrab_extgate::ExtgateTimedFireSink::new(
            extgate.clone(),
            state.clone(),
        )));

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    {
        let listen_state = Arc::clone(&extgate);
        let runtime = state.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(listen_state, runtime, path).await;
        });
    }
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    Core {
        extgate,
        sock,
        state,
        _dir: dir,
    }
}

/// D-1098: declares the instance and its complete binding set on core's gate socket, the way a
/// gateway does before `hello`. Returns the binding ids (UUIDv5(instance, address)).
pub(crate) async fn provision(
    core: &Core,
    instance_id: &str,
    kind_id: &str,
    config_b64: &str,
    addresses: &[String],
) -> Vec<String> {
    let provisioned = opencrab_gate_client::provision::ProvisionClient::new(core.sock.clone())
        .provision(&opencrab_gate_client::provision::ProvisionDesired {
            instance_id,
            kind_id,
            agent_id: AGENT_ID,
            enabled: true,
            config_b64,
            addresses,
        })
        .await
        .expect("provision");
    addresses
        .iter()
        .map(|address| {
            provisioned
                .bindings
                .iter()
                .find(|binding| &binding.address == address)
                .expect("provisioned binding")
                .binding_id
                .clone()
        })
        .collect()
}

pub(crate) fn discord_config() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "agent_id": AGENT_ID,
        "self_bot_id": SELF_BOT,
        "name": "crab",
        "delivery_mode": "say",
        "access": {"owners": [AUTHOR]},
    }))
    .unwrap()
}

/// instance + binding を登録し、discord gateway（fake_events + dry_run）を起動して bind ack を待つ。
pub(crate) async fn wire_instance(core: &Core, fixture: &Fixture) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = address();

    provision(core, &instance_id, "discord", &config_b64, &[addr.clone()]).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
        core_config_b64: None,
        channel_socket: None,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(
        core.sock.clone(),
        &place,
        &config_bytes,
        None,
        overrides,
        None,
    )
    .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない");
    client
}

/// 指定チャンネルで instance+binding を張り gateway を起動する（#915: typing 隔離テスト用）。
/// BUFFER は binary 内で共有・累積かつ CI は並列実行なので、typing（scope key を持たない capture）
/// を他テストと分離するために、他テストが使わない専用チャンネルへ束ねる。
pub(crate) async fn wire_instance_on_channel(
    core: &Core,
    fixture: &Fixture,
    channel: &str,
) -> Arc<InstanceClient> {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let config_bytes = discord_config();
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let addr = format!("discord-{AGENT_ID}-{GUILD}-{channel}");

    provision(core, &instance_id, "discord", &config_b64, &[addr.clone()]).await;

    let place = InstancePlacement {
        instance_id: instance_id.clone(),
        revision: 1,
        addresses: vec![addr.clone()],
        config_b64,
        core_config_b64: None,
        channel_socket: None,
    };
    let overrides = HarnessOverrides {
        fake_events: Some(fixture.path.clone()),
        dry_run: true,
    };
    let client = spawn_instance(
        core.sock.clone(),
        &place,
        &config_bytes,
        None,
        overrides,
        None,
    )
    .expect("spawn_instance");

    let mut bound = false;
    for _ in 0..250 {
        if client.binding_for_address(&addr).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bound, "binding が ack されない（専用チャンネル {channel}）");
    client
}

pub(crate) fn count_kind(buf: &Arc<Mutex<Vec<Captured>>>, kind: &str) -> usize {
    captured(buf).iter().filter(|c| c.kind == kind).count()
}

/// 指定チャンネルに限定した kind 別キャプチャ数（#915: typing を専用チャンネルで数える）。
pub(crate) fn count_kind_on_channel(
    buf: &Arc<Mutex<Vec<Captured>>>,
    kind: &str,
    channel: &str,
) -> usize {
    captured(buf)
        .iter()
        .filter(|c| c.kind == kind && c.channel == channel)
        .count()
}

pub(crate) fn own_speech_rows(core: &Core) -> i64 {
    let conn = core.extgate.db.lock().unwrap();
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' AND speaker_id='{AGENT_ID}'"
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

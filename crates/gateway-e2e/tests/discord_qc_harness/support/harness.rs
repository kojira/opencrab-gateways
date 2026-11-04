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
    let mut config = opencrab_server::test_config("mock", "gpt-4o");
    config.agent.workspace_path = std::env::temp_dir()
        .join("opencrab_discord_qc")
        .to_string_lossy()
        .to_string();
    AppState::new(db, router, config)
}

pub(crate) struct Core {
    pub(crate) extgate: Arc<ExtgateState>,
    pub(crate) sock: PathBuf,
    _dir: tempfile::TempDir,
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
        _dir: dir,
    }
}

/// D-1099: waits until core accepted the instance's hello (nothing is registered first).
pub(crate) async fn wait_connected(client: &InstanceClient, what: &str) {
    for _ in 0..250 {
        if client.connection_live().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("hello was not accepted ({what})");
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

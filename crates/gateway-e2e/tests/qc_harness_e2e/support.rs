use std::path::PathBuf;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencrab_llm::message::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;
use opencrab_server::AppState;

use opencrab_extgate::{serve_uds, ExtgateState};
use opencrab_gate_client::client::InstanceClient;

use opencrab_nostr_gateway::harness::HarnessOverrides;
use opencrab_nostr_gateway::run::spawn_instance;




const AGENT_ID: &str = "agent-qc";

fn self_pk() -> String {
    "11".repeat(32)
}
fn author_pk() -> String {
    "22".repeat(32)
}

// ==================== 観測: dry-run say キャプチャ ====================

/// 述語が真になるまで最大 ~5s ポーリングする。
async fn wait_until(pred: impl Fn() -> bool) -> bool {
    wait_until_for(Duration::from_secs(5), pred).await
}

/// 述語が真になるまで指定時間ポーリングする。
async fn wait_until_for(timeout: Duration, pred: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return pred();
        }
        tokio::time::sleep(Duration::from_millis(20).min(deadline - now)).await;
    }
}

/// An LLM that is never called: the remaining cases only connect gateways to core.
struct UnusedLlm;

#[async_trait::async_trait]
impl LlmProvider for UnusedLlm {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        Err(anyhow::anyhow!("UnusedLlm: no turn expected"))
    }
}

// ==================== 共通 helpers ====================

/// mock モデルの予算 envelope を満たす（#826 fail-loud 対策）。
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
        .join("opencrab_qc_harness")
        .to_string_lossy()
        .to_string();
    AppState::new(db, router, config)
}

struct Core {
    extgate: Arc<ExtgateState>,
    sock: PathBuf,
    _dir: tempfile::TempDir,
}

/// 実 serve_uds core + 実 AppState runtime を UDS で立ち上げ、nostr admit/watch hooks を配線する。
async fn start_core(provider: Arc<dyn LlmProvider>) -> Core {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    register_mock_pricing(&db);
    upsert_test_agent(&db);
    let extgate = Arc::new(ExtgateState::new(db.clone()));

    let state = build_app_state(db.clone(), provider);
    // #925: 本番と同じ descriptor 登録＋ V3 heartbeat 受け口を実型で配線する（Nostr レーンも
    // canonical session は `extgate-<binding_id>` で同一 descriptor が受ける）。未登録なら
    // resolve_target None で配送 0＝赤。
    state.timed_fire_router.register_sink(Arc::new(opencrab_extgate::ExtgateTimedFireSink::new(
            extgate.clone(),
            state.clone(),
        )),
    );

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gate.sock");
    {
        let listen_state = Arc::clone(&extgate);
        let runtime = state.clone();
        let path = sock.clone();
        tokio::spawn(async move {
            let _ = serve_uds(listen_state, runtime, path)
            .await;
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

fn nostr_config(watches: Option<serde_json::Value>) -> Vec<u8> {
    let mut cfg = serde_json::json!({
        "relays": ["wss://relay.invalid"],
        "self_pubkey": self_pk(),
        "name": "crab",
        "delivery_mode": "say",
        "access": {"owner": [author_pk()]},
    });
    if let Some(w) = watches {
        cfg["watches"] = w;
    }
    serde_json::to_vec(&cfg).unwrap()
}


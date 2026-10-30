//! rust-unit: frame parser と in-process HTTP。共通 conformance の合格数には算入しない。
//! 旧 `tests/conformance.rs` の in-process 検査を移設した。緩和していない。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use opencrab_web_gateway::v3::client::InstanceClient;
use opencrab_web_gateway::v3::http::{router, HttpState};
use opencrab_web_gateway::v3::wire::{
    hello_frame, ok_frame, parse_frame_bytes, read_frame, write_json, CoreMsg, FrameError,
};
use serde_json::{json, Value};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const AGENT: &str = "agent";
/// The web conversation's session key (D-1099: core sessions are keyed by it).
const ADDRESS: &str = "web-agent-conv";
const AUTHOR: &str = "owner-1";
const CLIENT_MSG: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

struct MockCore {
    incoming: mpsc::UnboundedReceiver<Value>,
    half: std::sync::Arc<tokio::sync::Mutex<Option<tokio::net::unix::OwnedWriteHalf>>>,
}

impl MockCore {
    fn spawn(path: std::path::PathBuf) -> Self {
        let listener = UnixListener::bind(&path).expect("bind mock core");
        let (tx, incoming) = mpsc::unbounded_channel();
        let half = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let half_task = half.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (mut reader, writer) = stream.into_split();
            *half_task.lock().await = Some(writer);
            while let Ok(bytes) = read_frame(&mut reader).await {
                let text = String::from_utf8_lossy(bytes.strip_suffix(b"\n").unwrap_or(&bytes));
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        Self { incoming, half }
    }

    async fn wait_writer(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if self.half.lock().await.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("accept timeout");
    }

    async fn send(&self, v: &Value) {
        self.wait_writer().await;
        let mut half = self.half.lock().await;
        let w = half.as_mut().expect("writer");
        write_json(w, v).await.expect("mock write");
    }

    async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(2), self.incoming.recv())
            .await
            .expect("recv timeout")
            .expect("closed")
    }
}

async fn connect_client(sock: &std::path::Path) -> Arc<InstanceClient> {
    InstanceClient::connect(
        sock,
        INSTANCE.to_string(),
        AGENT.to_string(),
        AUTHOR.to_string(),
    )
    .await
    .expect("connect")
}

/// Core answers the hello. D-1099: nothing else is needed before saids (no bind).
async fn hello_and_bind(mock: &mut MockCore) {
    let hello = mock.recv().await;
    assert_eq!(hello["m"], "hello");
    assert_eq!(hello["protocol"], 3);
    assert_eq!(hello["instance_id"], INSTANCE);
    assert_eq!(hello["agent_id"], AGENT);
    mock.send(&json!({"id": hello["id"], "m": "ok"})).await;
}

async fn wait_bound(client: &InstanceClient) {
    for _ in 0..50 {
        if client.connection_live().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("hello not acknowledged");
}

/// HTTP state whose conversation registry already owns [`ADDRESS`] for `client`.
fn state_with_conversation(client: Arc<InstanceClient>) -> HttpState {
    let conversations =
        opencrab_web_gateway::conversations::Conversations::open(None).expect("registry");
    conversations
        .record(ADDRESS, &client.instance_id, ADDRESS)
        .expect("record");
    HttpState {
        instances: vec![client],
        agent_clients: std::collections::HashMap::new(),
        conversations: Arc::new(conversations),
    }
}

fn empty_state() -> HttpState {
    HttpState {
        instances: Vec::new(),
        agent_clients: std::collections::HashMap::new(),
        conversations: Arc::new(
            opencrab_web_gateway::conversations::Conversations::open(None).expect("registry"),
        ),
    }
}

#[tokio::test]
async fn rust_unit_conversation_status_is_owned_by_web_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;
    let app = router(state_with_conversation(client));

    let ready = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/web-conversations/{ADDRESS}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&ready.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body, json!({"session_id": ADDRESS, "state": "ready"}));

    let missing = app
        .oneshot(
            Request::builder()
                .uri("/api/web-conversations/not-owned")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// D-1099: creating a web conversation asks core to `open` a session key and records which
/// instance owns it; the new conversation is ready at once.
#[tokio::test]
async fn rust_unit_create_opens_a_session_and_records_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("core.sock");
    let mut mock = MockCore::spawn(sock.clone());
    let connect = tokio::spawn(async move { connect_client(&sock).await });
    hello_and_bind(&mut mock).await;
    let client = connect.await.unwrap();
    wait_bound(&client).await;
    let mut state = empty_state();
    state.instances = vec![client.clone()];
    state.agent_clients.insert(AGENT.to_string(), client);
    let app = router(state);
    let pending = tokio::spawn(
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/web-conversations")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"agent_id":"{AGENT}","name":"n"}}"#
                )))
                .unwrap(),
        ),
    );
    let open = mock.recv().await;
    assert_eq!(open["m"], "open", "{open}");
    let key = open["session"].as_str().unwrap().to_string();
    assert!(key.starts_with("extgate-"), "{key}");
    assert_eq!(open["title"], "n");
    mock.send(&json!({"id": open["id"], "m": "ok", "session_id": key}))
        .await;
    let response = pending.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["state"], "ready");
    assert_eq!(body["session_id"], key);

    let status = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/web-conversations/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
}

#[test]
fn rust_unit_frame_grammar_unit() {
    let _ = hello_frame("h", INSTANCE, AGENT);
    let _ = ok_frame("x");
    assert!(matches!(
        parse_frame_bytes(br#"{"m":1}"#).unwrap_or(CoreMsg::Invalid {
            id: None,
            code: "bad_request",
            m: String::new(),
        }),
        CoreMsg::Invalid { .. }
    ));
    let dup = parse_frame_bytes(br#"{"id":"1","m":"ok","id":"2"}"#);
    assert!(matches!(dup, Err(FrameError::BadRequest)));
    let _ = Uuid::parse_str(CLIENT_MSG).unwrap();
}

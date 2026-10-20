//! D-1087 配送経路のテスト（宛先あり/なし・clear・再起動・本文の不変）。
//!
//! production seam は [`ActivityDelivery::deliver_for_turn`]。ローカルの HTTP モックで
//! 「実際に届いたか」を確かめる（実 Discord へは出さない）。

use super::route::{RouteStore, SessionRoute};
use super::source::{ActivityEvent, ActivitySource, ActivityStatus, QueueActivitySource};
use super::{ActivityDelivery, LifecycleMeta};
use std::sync::Arc;
use std::time::Duration;

const AGENT: &str = "agent-1";
const CHANNEL: &str = "channel-9";
const BINDING: &str = "binding-1";

fn route(url: &str) -> SessionRoute {
    SessionRoute {
        agent_id: AGENT.into(),
        channel_id: CHANNEL.into(),
        url: url.into(),
        events: None,
        enabled: true,
    }
}

fn started_event() -> ActivityEvent {
    ActivityEvent {
        activity_id: "activity-1".into(),
        call_id: "call-1".into(),
        tool: "execute_shell".into(),
        status: ActivityStatus::Started,
        depth: 0,
        started_at: "2026-10-20T00:00:00Z".into(),
        duration_ms: None,
        args: serde_json::json!({"command": "ls"}),
        result: None,
        error: None,
    }
}

fn completed_event() -> ActivityEvent {
    ActivityEvent {
        status: ActivityStatus::Completed,
        duration_ms: Some(12),
        result: Some(serde_json::json!({"exit_code": 0, "stdout": "a.txt\n"})),
        ..started_event()
    }
}

fn delivery(store: RouteStore, source: Arc<QueueActivitySource>) -> ActivityDelivery {
    ActivityDelivery::new(store, source as Arc<dyn ActivitySource>)
}

/// 最小の HTTP モック（1 リクエスト = 1 接続）。受け取った body を記録する。
struct MockWebhook {
    url: String,
    bodies: Arc<std::sync::Mutex<Vec<String>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockWebhook {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = bodies.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        let read = match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..read]);
                        if let Some(p) =
                            buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
                        {
                            break p;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let len: usize = head
                        .split("\r\n")
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        let read = match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&chunk[..read]);
                    }
                    sink.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf[head_end..]).into_owned());
                    let resp =
                        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        MockWebhook {
            url: format!("http://{addr}/api/webhooks/1/tok"),
            bodies,
            _handle: handle,
        }
    }

    fn count(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    async fn wait_for(&self, n: usize, within: Duration) {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if self.count() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} request(s), got {}", self.count());
    }
}

#[tokio::test]
async fn set_then_a_turn_event_is_delivered() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    let delivery = delivery(store, source);

    delivery.deliver_for_turn(BINDING, AGENT, CHANNEL, None);
    mock.wait_for(1, Duration::from_secs(5)).await;
    let body = mock.bodies().remove(0);
    assert!(body.contains("tool_call_started"), "{body}");
    assert!(body.contains("execute_shell"), "{body}");
}

#[tokio::test]
async fn without_a_destination_nothing_is_sent() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    let delivery = delivery(store, source);

    delivery.deliver_for_turn(BINDING, AGENT, CHANNEL, None);
    // 送らない。モックに何も届かないことを、少し待って確かめる。
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn clearing_the_route_stops_delivery() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    assert!(store.clear(AGENT, CHANNEL).unwrap());
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    let delivery = delivery(store, source);

    delivery.deliver_for_turn(BINDING, AGENT, CHANNEL, None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0, "a cleared session must not deliver");
}

#[tokio::test]
async fn a_different_session_does_not_receive() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    let delivery = delivery(store, source);

    delivery.deliver_for_turn(BINDING, AGENT, "another-channel", None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn the_route_survives_a_restart_and_still_delivers() {
    let mock = MockWebhook::start().await;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gateway.db");
    {
        let store = RouteStore::open(&path).unwrap();
        store.set(&route(&mock.url), 1).unwrap();
    }
    // 再起動 = 同じ DB を開き直すだけ。設定は残る。
    let reopened = RouteStore::open(&path).unwrap();
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, completed_event());
    let delivery = delivery(reopened, source);

    delivery.deliver_for_turn(BINDING, AGENT, CHANNEL, None);
    mock.wait_for(1, Duration::from_secs(5)).await;
    let body = mock.bodies().remove(0);
    assert!(body.contains("tool_call_completed"), "{body}");
}

#[tokio::test]
async fn a_disabled_route_delivers_nothing() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    let mut disabled = route(&mock.url);
    disabled.enabled = false;
    store.set(&disabled, 1).unwrap();
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    let delivery = delivery(store, source);

    delivery.deliver_for_turn(BINDING, AGENT, CHANNEL, None);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0);
}

#[test]
fn event_filter_selects_which_events_go_out() {
    let config = super::WebhookConfig::from_parts(
        "https://example/1/t".into(),
        Some(vec!["completed".into()]),
    )
    .expect("config");
    assert!(!config.wants("tool_call_started"));
    assert!(config.wants("tool_call_completed"));
}

/// 本文は移設前と同一（同じ見出し・同じ並び）。配送内容を変えていないことの記録。
#[test]
fn tool_event_message_matches_the_pre_split_format() {
    let view = super::ToolEventView {
        event: "tool_call_completed".into(),
        tool_name: "execute_shell".into(),
        tool_call_id: "call-1".into(),
        depth: 0,
        status: "completed".into(),
        duration_ms: Some(12),
        args_summary: Some("command=ls".into()),
        result_summary: None,
        exit_code: Some(0),
        stdout_summary: Some("a.txt\n".into()),
        stderr_summary: None,
        truncated: false,
        rejection_reason: None,
        max_chars: 0,
    };
    let messages = super::build_tool_event_message(&view);
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].content,
        "✅ **tool_call_completed**\ntool: `execute_shell`\ncallId: `call-1`\ndepth: 0\n\
         duration: 12ms\nexit_code: `0`\nargs: command=ls\nstdout:\na.txt\n"
    );
}

/// 供給元のイベントが整形入力へ写るとき、status とイベント名の対応が崩れていないこと。
#[test]
fn queued_events_map_to_the_legacy_event_names() {
    let source = Arc::new(QueueActivitySource::default());
    source.push(BINDING, started_event());
    source.push(BINDING, completed_event());
    let drained = source.drain(BINDING);
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].status.event_name(), "tool_call_started");
    assert_eq!(drained[1].status.event_name(), "tool_call_completed");
    let view = super::deliver::tool_event_view(&drained[1]);
    assert_eq!(view.status, "completed");
    assert_eq!(view.exit_code, Some(0));
    assert_eq!(view.stdout_summary.as_deref(), Some("a.txt\n"));
}

#[test]
fn lifecycle_terminal_line_is_unchanged() {
    let meta = LifecycleMeta {
        label: "task".into(),
        run_id: "run-1".into(),
        session_key: "key".into(),
    };
    let line =
        super::build_terminal_message("completed", &meta.run_id, &meta.session_key, Some(5), "ok");
    assert_eq!(
        line,
        "✅ **subtask completed**\nrunId: `run-1`\nsessionKey: `key`\nduration: 5ms\nresult: ok"
    );
}

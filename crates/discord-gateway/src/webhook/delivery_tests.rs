//! D-1087 配送経路のテスト（宛先あり/なし・clear・再起動・本文の不変）。
//!
//! production seam は [`ActivityDelivery::event`]（1 件ごと）と [`ActivityDelivery::flush`]
//! （ターン終端の積み残し）。ローカルの HTTP モックで「実際に届いたか」を確かめる
//! （実 Discord へは出さない）。

use super::route::{RouteStore, SessionRoute};
use super::source::{ActivityEvent, ActivityItem, ActivityStatus, LifecycleEvent};
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

fn delivery(store: RouteStore) -> ActivityDelivery {
    ActivityDelivery::new(store)
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
    let delivery = delivery(store);
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
    mock.wait_for(1, Duration::from_secs(5)).await;
    let body = mock.bodies().remove(0);
    assert!(body.contains("tool_call_started"), "{body}");
    assert!(body.contains("execute_shell"), "{body}");
}

#[tokio::test]
async fn without_a_destination_nothing_is_sent() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    let delivery = delivery(store);
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
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
    let delivery = delivery(store);
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0, "a cleared session must not deliver");
}

#[tokio::test]
async fn a_different_session_does_not_receive() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let delivery = delivery(store);
    delivery.event(
        BINDING,
        AGENT,
        "another-channel",
        ActivityItem::Tool(started_event()),
    );
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
    let delivery = delivery(reopened);
    delivery.event(
        BINDING,
        AGENT,
        CHANNEL,
        ActivityItem::Tool(completed_event()),
    );
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
    let delivery = delivery(store);
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
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
    let queue = super::source::ActivityQueue::default();
    queue.push(BINDING, ActivityItem::Tool(started_event()));
    queue.push(BINDING, ActivityItem::Tool(completed_event()));
    let drained = queue.drain(BINDING);
    assert_eq!(drained.len(), 2);
    let ActivityItem::Tool(first) = &drained[0] else {
        panic!("tool event expected");
    };
    let ActivityItem::Tool(second) = &drained[1] else {
        panic!("tool event expected");
    };
    assert_eq!(first.status.event_name(), "tool_call_started");
    assert_eq!(second.status.event_name(), "tool_call_completed");
    let view = super::deliver::tool_event_view(second);
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

fn lifecycle(status: &str) -> LifecycleEvent {
    LifecycleEvent {
        run_id: "run-1".into(),
        status: status.into(),
        label: Some("task".into()),
        task: Some("do the thing".into()),
        detail: Some("halfway".into()),
        duration_ms: Some(7),
        result_text: Some("done".into()),
    }
}

/// D-1087 P0: サブタスクの開始・進捗・終端が実際に届く（移設前と同じ本文）。
#[tokio::test]
async fn subtask_lifecycle_started_progress_and_terminal_are_delivered() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let delivery = delivery(store);

    for status in ["started", "progress", "completed"] {
        delivery.event(
            BINDING,
            AGENT,
            CHANNEL,
            ActivityItem::Lifecycle(lifecycle(status)),
        );
    }
    mock.wait_for(3, Duration::from_secs(5)).await;
    let bodies = mock.bodies();
    assert!(
        bodies[0].contains("🟢 **subtask started**") && bodies[0].contains("task: do the thing"),
        "{}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("🔄 **subtask progress**") && bodies[1].contains("message: halfway"),
        "{}",
        bodies[1]
    );
    assert!(
        bodies[2].contains("✅ **subtask completed**")
            && bodies[2].contains("duration: 7ms")
            && bodies[2].contains("result: done"),
        "{}",
        bodies[2]
    );
}

/// 終端の状態は core の status をそのまま写す（failed / timed_out / aborted）。
#[tokio::test]
async fn subtask_terminal_status_keeps_the_failure_kind() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let delivery = delivery(store);

    delivery.event(
        BINDING,
        AGENT,
        CHANNEL,
        ActivityItem::Lifecycle(lifecycle("timed_out")),
    );
    mock.wait_for(1, Duration::from_secs(5)).await;
    let body = mock.bodies().remove(0);
    assert!(body.contains("⏱️ **subtask timed_out**"), "{body}");
}

/// D-1087 P1: 1 run のツール活動は 200 件まで。201 件目は抑制サマリ 1 通、以降は捨てる。
#[tokio::test]
async fn the_per_run_cap_sends_one_suppression_note_and_drops_the_rest() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let delivery = delivery(store);

    for _ in 0..(super::deliver::TOOL_EVENT_CAP + 2) {
        delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
    }
    mock.wait_for(super::deliver::TOOL_EVENT_CAP + 1, Duration::from_secs(15))
        .await;
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), super::deliver::TOOL_EVENT_CAP + 1);
    assert!(
        bodies[super::deliver::TOOL_EVENT_CAP]
            .contains("(+ further tool events suppressed after 200 this run)"),
        "{}",
        bodies[super::deliver::TOOL_EVENT_CAP]
    );

    // 次のターン（activity_id が変わる）は上限が戻る。
    let mut next = started_event();
    next.activity_id = "activity-2".into();
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(next));
    mock.wait_for(super::deliver::TOOL_EVENT_CAP + 2, Duration::from_secs(5))
        .await;
    let bodies = mock.bodies();
    assert!(bodies[super::deliver::TOOL_EVENT_CAP + 1].contains("tool_call_started"));
}

/// D-1087 P1: execute_shell の引数表示は移設前とバイト一致（cmd / args / stdin バイト数）。
#[test]
fn shell_arguments_render_exactly_like_the_pre_split_implementation() {
    use serde_json::json;
    let event = ActivityEvent {
        tool: "execute_shell".into(),
        args: json!({
            "command": "bun",
            "args": ["script.ts", "--flag"],
            "stdin": "secret payload",
        }),
        ..started_event()
    };
    let view = super::deliver::tool_event_view(&event);
    assert_eq!(
        view.args_summary.as_deref(),
        Some(r#"cmd: `bun` args: ["script.ts","--flag"] stdin: 14 bytes"#)
    );
    // 非 shell は JSON をそのまま（"null"/"{}" は出さない）。
    let plain = ActivityEvent {
        tool: "search_memory_index".into(),
        args: json!({"query": "x"}),
        ..started_event()
    };
    assert_eq!(
        super::deliver::tool_event_view(&plain)
            .args_summary
            .as_deref(),
        Some(r#"{"query":"x"}"#)
    );
    let empty = ActivityEvent {
        args: json!({}),
        ..started_event()
    };
    assert_eq!(super::deliver::tool_event_view(&empty).args_summary, None);
}

/// 拒否理由は core の構造マーカーを落として人間可読分だけ出す（移設前と同じ）。
#[test]
fn rejection_reason_drops_the_structured_prefix() {
    let event = ActivityEvent {
        status: ActivityStatus::Rejected,
        error: Some("rejected: action not allowed".into()),
        ..started_event()
    };
    let view = super::deliver::tool_event_view(&event);
    assert_eq!(view.rejection_reason.as_deref(), Some("action not allowed"));
}

/// `/webhook clear` は設定を消すだけ。webhook 自体（宛先 URL）もチャンネルも触らない。
///
/// kojira 2026-10-20: 「webhook clearでチャンネルを消すような処理は不要です。設定を消すだけで十分です」。
#[tokio::test]
async fn clear_removes_only_the_setting_and_keeps_the_webhook() {
    let mock = MockWebhook::start().await;
    let store = RouteStore::in_memory().unwrap();
    store.set(&route(&mock.url), 1).unwrap();
    let stored = store.get(AGENT, CHANNEL).unwrap().expect("route");
    assert_eq!(stored.url, mock.url);

    assert!(store.clear(AGENT, CHANNEL).unwrap());
    // 設定だけが消える。webhook の URL は消していないので、同じ宛先を再設定できる。
    assert!(store.get(AGENT, CHANNEL).unwrap().is_none());
    store.set(&route(&mock.url), 2).unwrap();
    // clear は Discord へ何も送らない（webhook 削除リクエストを出さない）。
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.count(), 0, "clear must not touch the webhook");

    let delivery = delivery(store);
    delivery.event(BINDING, AGENT, CHANNEL, ActivityItem::Tool(started_event()));
    mock.wait_for(1, Duration::from_secs(5)).await;
}

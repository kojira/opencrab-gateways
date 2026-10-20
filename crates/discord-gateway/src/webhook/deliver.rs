//! 活動イベントの配送（D-1087）。宛先があるときだけ送り、無いときは黙って捨てない。
//!
//! 流れ:
//! 1. 受信ループが [`ActivityQueue`] へ活動を積み、[`ActivityDelivery::event`] を呼ぶ。
//! 2. その binding の未配送イベントを取り出し、セッション（agent + channel）の宛先を
//!    gateway DB から引く（[`super::RouteStore`]）。
//! 3. 宛先が無ければ何も送らず、レート制限付きの debug を 1 行だけ残す（記録は core に残る）。
//! 4. あれば移設前と同一の本文・上限で整形し、binding ごとの配送 worker に渡す。
//!
//! 本文は移設前（core の `webhook_target` / `DiscordWebhookNotifier`）と同一:
//! ツール活動は `tool_call_*` の 1 イベント 1 通、サブタスクは started / progress / 終端行。
//! per-run 上限 200 件と、超過時の抑制サマリ 1 通もそのまま。

use std::collections::HashMap;
use std::sync::Mutex;

use super::route::{AbsenceLog, RouteStore};
use super::source::{ActivityEvent, ActivityItem, ActivityQueue, LifecycleEvent};
use super::{
    build_progress_message, build_started_messages, build_terminal_message,
    build_tool_event_message, exit_reason_to_status, spawn_run_worker_with_sink,
    summarize_shell_result, DeliveryBatch, GiveupSink, LifecycleMeta, ToolEventView, WebhookConfig,
};

/// 1 run（= 1 ターン）あたりに送るツール活動の上限。移設前と同じ値。
pub(super) const TOOL_EVENT_CAP: usize = 200;
/// ツール活動の本文プレビュー上限ヒント。移設前と同じ値。
const TOOL_EVENT_MAX_CHARS: usize = 1500;
/// core が拒否理由の先頭に付ける構造マーカー。表示では落とし、人間可読の理由だけ残す
/// （移設前の `REJECTION_CODE_PREFIX` と同じ扱い）。
const REJECTION_PREFIX: &str = "rejected: ";

/// 1 ターンのツール活動数（per-run 上限の判定）。
#[derive(Default)]
struct CapState {
    activity_id: String,
    count: usize,
}

/// 活動 webhook の配送口。
pub struct ActivityDelivery {
    store: RouteStore,
    queue: ActivityQueue,
    absence: AbsenceLog,
    client: reqwest::Client,
    /// binding ごとの配送 worker。同一 run の送出順を 1 本の worker で保証する。
    workers: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<DeliveryBatch>>>,
    /// ターン（activity_id）ごとの送出数。activity_id が変わったら 0 に戻す。
    cap: Mutex<CapState>,
}

impl ActivityDelivery {
    pub fn new(store: RouteStore) -> Self {
        Self {
            store,
            queue: ActivityQueue::default(),
            absence: AbsenceLog::new(std::time::Duration::from_secs(60)),
            client: reqwest::Client::new(),
            workers: Mutex::new(HashMap::new()),
            cap: Mutex::new(CapState::default()),
        }
    }

    /// 活動 1 件を受け取り、その場で配送する（core の通知は発生順に届く）。
    pub fn event(&self, binding_id: &str, agent_id: &str, channel_id: &str, item: ActivityItem) {
        self.queue.push(binding_id, item);
        self.flush(binding_id, agent_id, channel_id);
    }

    /// その binding の未配送分を宛先へ送る。宛先が無ければ捨てる（記録は core に残る）。
    pub fn flush(&self, binding_id: &str, agent_id: &str, channel_id: &str) {
        let events = self.queue.drain(binding_id);
        if events.is_empty() {
            return;
        }
        let Some(config) = self.route_config(agent_id, channel_id) else {
            if self.absence.should_log(agent_id, channel_id) {
                tracing::debug!(
                    agent_id,
                    channel_id,
                    dropped = events.len(),
                    "activity webhook: no destination configured for this session; nothing sent \
                     (the records stay in core)"
                );
            }
            return;
        };
        let Some(sender) = self.worker(binding_id, None) else {
            return;
        };
        for item in events {
            let messages = self.messages(&config, &item);
            if messages.is_empty() {
                continue;
            }
            let _ = sender.send(DeliveryBatch {
                url: config.url.clone(),
                messages,
            });
        }
    }

    /// このセッションの宛先設定（無効・未設定なら None）。
    fn route_config(&self, agent_id: &str, channel_id: &str) -> Option<WebhookConfig> {
        match self.store.get(agent_id, channel_id) {
            Ok(route) => route.and_then(|route| route.config()),
            Err(error) => {
                tracing::warn!(
                    agent_id,
                    channel_id,
                    error = %error,
                    "activity webhook route lookup failed"
                );
                None
            }
        }
    }

    /// binding の worker を返す（無ければ起動）。`on_giveup` は起動時にだけ渡す。
    fn worker(
        &self,
        binding_id: &str,
        on_giveup: Option<GiveupSink>,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<DeliveryBatch>> {
        let mut workers = self.workers.lock().ok()?;
        if let Some(existing) = workers.get(binding_id) {
            if !existing.is_closed() {
                return Some(existing.clone());
            }
        }
        let sender = spawn_run_worker_with_sink(self.client.clone(), on_giveup);
        workers.insert(binding_id.to_string(), sender.clone());
        Some(sender)
    }

    /// 1 件をメッセージ列へ整形する。購読していないイベントは空。
    fn messages(&self, config: &WebhookConfig, item: &ActivityItem) -> Vec<super::WebhookMessage> {
        match item {
            ActivityItem::Tool(event) => self.tool_messages(config, event),
            ActivityItem::Lifecycle(event) => self.lifecycle_messages(config, event),
        }
    }

    /// ツール活動 1 件。per-run 上限を超えたら抑制サマリ 1 通だけ送る。
    fn tool_messages(
        &self,
        config: &WebhookConfig,
        event: &ActivityEvent,
    ) -> Vec<super::WebhookMessage> {
        if !config.wants(event.status.event_name()) {
            return Vec::new();
        }
        let Some(index) = self.next_tool_index(&event.activity_id) else {
            return Vec::new();
        };
        if index == TOOL_EVENT_CAP {
            return vec![format!(
                "(+ further tool events suppressed after {TOOL_EVENT_CAP} this run)"
            )
            .into()];
        }
        if index > TOOL_EVENT_CAP {
            return Vec::new();
        }
        build_tool_event_message(&tool_event_view(event))
    }

    /// 終端状態の写像を含む、サブタスク lifecycle 1 件。
    fn lifecycle_messages(
        &self,
        config: &WebhookConfig,
        event: &LifecycleEvent,
    ) -> Vec<super::WebhookMessage> {
        // core の frame は表示用の status（`started`/`progress`/`completed`/`failed`/
        // `timed_out`/`aborted`）を載せるのでそのまま使う。未知の値は従来の
        // exit_reason（`timeout`/`error`/...）として写像する。
        let status = match event.status.as_str() {
            known @ ("started" | "progress" | "completed" | "failed" | "timed_out" | "aborted") => {
                known
            }
            other => exit_reason_to_status(other),
        };
        if !config.wants(status) {
            return Vec::new();
        }
        let meta = LifecycleMeta {
            label: event.label.clone().unwrap_or_default(),
            run_id: event.run_id.clone(),
            // core の frame は run id だけを運ぶ。sessionKey 行は値が無いので出さない
            // （run id 自体が相関キーなので、情報は落ちない）。
            session_key: String::new(),
        };
        match status {
            "started" => build_started_messages(&meta, event.task.as_deref().unwrap_or_default()),
            "progress" => vec![build_progress_message(
                &meta.run_id,
                &meta.session_key,
                event.detail.as_deref().unwrap_or_default(),
            )
            .into()],
            terminal => vec![build_terminal_message(
                terminal,
                &meta.run_id,
                &meta.session_key,
                event.duration_ms,
                event.result_text.as_deref().unwrap_or_default(),
            )
            .into()],
        }
    }

    /// この activity（ターン）の 0 始まり連番。上限判定と抑制サマリに使う。
    fn next_tool_index(&self, activity_id: &str) -> Option<usize> {
        let mut cap = self.cap.lock().ok()?;
        if cap.activity_id != activity_id {
            cap.activity_id = activity_id.to_string();
            cap.count = 0;
        }
        let index = cap.count;
        cap.count += 1;
        Some(index)
    }
}

/// 供給元のイベントを、移設前と同じ整形入力へ写す。
pub(super) fn tool_event_view(event: &ActivityEvent) -> ToolEventView {
    let mut view = ToolEventView {
        event: event.status.event_name().to_string(),
        tool_name: event.tool.clone(),
        tool_call_id: event.call_id.clone(),
        depth: event.depth,
        status: event.status.as_str().to_string(),
        duration_ms: event.duration_ms,
        max_chars: TOOL_EVENT_MAX_CHARS,
        ..Default::default()
    };
    view.args_summary = summarize_tool_args(&event.tool, &event.args);
    match event.status {
        super::source::ActivityStatus::Completed | super::source::ActivityStatus::Failed => {
            if event.tool == "execute_shell" {
                if let Some(data) = event.result.as_ref() {
                    let shell = summarize_shell_result(data);
                    view.exit_code = shell.exit_code;
                    view.stdout_summary = shell.stdout_summary;
                    view.stderr_summary = shell.stderr_summary;
                    view.truncated = shell.truncated;
                }
            } else if let Some(error) = event.error.as_deref() {
                view.result_summary = Some(error.to_string());
            } else if let Some(data) = event.result.as_ref() {
                view.result_summary = Some(data.to_string());
            }
        }
        super::source::ActivityStatus::Rejected => {
            // 構造マーカー接頭辞は表示では落とし、人間可読の理由だけ残す。
            view.rejection_reason = event.error.as_deref().map(|raw| {
                raw.strip_prefix(REJECTION_PREFIX)
                    .unwrap_or(raw)
                    .to_string()
            });
        }
        super::source::ActivityStatus::Started => {}
    }
    view
}

/// ツール引数の要約（execute_shell はコマンドを優先）。移設前と同一の文面。
///
/// covered 経路（work-channel 出力）のため redaction も length クランプも行わず、
/// command / args 配列をそのまま返す（docs/design-webhook-output-lossless.md §2 P4）。
/// stdin は本文を出さず、存在とバイト数のみ示す。
fn summarize_tool_args(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name == "execute_shell" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            let mut parts = vec![format!("cmd: `{cmd}`")];
            if let Some(arr) = args.get("args").and_then(|v| v.as_array()) {
                let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !items.is_empty() {
                    parts.push(format!("args: {}", serde_json::Value::from(items)));
                }
            }
            if let Some(stdin) = args.get("stdin").and_then(|v| v.as_str()) {
                if !stdin.is_empty() {
                    parts.push(format!("stdin: {} bytes", stdin.len()));
                }
            }
            return Some(parts.join(" "));
        }
    }
    let rendered = args.to_string();
    if rendered == "null" || rendered == "{}" {
        return None;
    }
    Some(rendered)
}

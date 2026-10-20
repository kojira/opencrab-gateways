//! 活動イベントの配送（D-1087）。宛先があるときだけ送り、無いときは黙って捨てない。
//!
//! 流れ:
//! 1. [`crate::webhook::source::ActivitySource`] から、その binding の未配送イベントを取る。
//! 2. セッション（agent + channel）の宛先を gateway DB から引く（[`super::RouteStore`]）。
//! 3. 宛先が無ければ何も送らず、レート制限付きの debug を 1 行だけ残す。
//! 4. あれば [`super::build_tool_event_message`] で整形し（移設前と同一の本文・上限）、
//!    run ごとの配送 worker に渡す。

use std::sync::Arc;

use super::route::{AbsenceLog, RouteStore};
use super::source::{ActivityEvent, ActivitySource};
use super::{
    build_tool_event_message, spawn_run_worker_with_sink, summarize_shell_result, DeliveryBatch,
    GiveupSink, ToolEventView,
};

/// 活動 webhook の配送口。
pub struct ActivityDelivery {
    store: RouteStore,
    source: Arc<dyn ActivitySource>,
    absence: AbsenceLog,
    client: reqwest::Client,
}

impl ActivityDelivery {
    pub fn new(store: RouteStore, source: Arc<dyn ActivitySource>) -> Self {
        Self {
            store,
            source,
            absence: AbsenceLog::new(std::time::Duration::from_secs(60)),
            client: reqwest::Client::new(),
        }
    }

    /// このセッションの宛先設定（無効・未設定なら None）。
    fn route_config(&self, agent_id: &str, channel_id: &str) -> Option<super::WebhookConfig> {
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

    /// 1 ターン分の活動を配送する。`on_giveup` は最終的に諦めたときの記録先。
    ///
    /// 宛先が無ければ何も送らない（記録は core 側に残っている）。
    pub fn deliver_for_turn(
        &self,
        binding_id: &str,
        agent_id: &str,
        channel_id: &str,
        on_giveup: Option<GiveupSink>,
    ) {
        let events = self.source.drain(binding_id);
        let Some(config) = self.route_config(agent_id, channel_id) else {
            if !events.is_empty() && self.absence.should_log(agent_id, channel_id) {
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
        let batches: Vec<DeliveryBatch> = events
            .iter()
            .filter(|event| config.wants(event.status.event_name()))
            .map(|event| DeliveryBatch {
                url: config.url.clone(),
                messages: build_tool_event_message(&tool_event_view(event)),
            })
            .collect();
        if batches.is_empty() {
            return;
        }
        let sender = spawn_run_worker_with_sink(self.client.clone(), on_giveup);
        for batch in batches {
            // worker が落ちていれば送れないが、その事実は worker 側のログに残る。
            let _ = sender.send(batch);
        }
    }
}

/// 供給元のイベントを、移設前と同じ整形入力へ写す。
pub(super) fn tool_event_view(event: &ActivityEvent) -> ToolEventView {
    let shell = event
        .result
        .as_ref()
        .map(summarize_shell_result)
        .unwrap_or_default();
    ToolEventView {
        event: event.status.event_name().to_string(),
        tool_name: event.tool.clone(),
        tool_call_id: event.call_id.clone(),
        depth: event.depth,
        status: event.status.as_str().to_string(),
        duration_ms: event.duration_ms,
        args_summary: summarize_args(&event.args),
        result_summary: event.result.as_ref().and_then(summarize_result),
        exit_code: shell.exit_code,
        stdout_summary: shell.stdout_summary,
        stderr_summary: shell.stderr_summary,
        truncated: shell.truncated,
        rejection_reason: event.error.clone(),
        max_chars: 0,
    }
}

/// 引数の短い要約（`key=value` を空白区切り）。
fn summarize_args(args: &serde_json::Value) -> Option<String> {
    let object = args.as_object()?;
    if object.is_empty() {
        return None;
    }
    let rendered: Vec<String> = object
        .iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        })
        .collect();
    Some(rendered.join(" "))
}

/// 結果の短い要約。`execute_shell` は exit_code/stdout/stderr を別枠で出すので要約しない。
fn summarize_result(result: &serde_json::Value) -> Option<String> {
    if result.get("exit_code").is_some() {
        return None;
    }
    serde_json::to_string(result).ok()
}

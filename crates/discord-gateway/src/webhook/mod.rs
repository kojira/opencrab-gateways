//! 活動 webhook（D-1087）。宛先の決定・整形・配送はすべて gateway が持つ。
//!
//! 「webhook」は gateway の概念なので core には置かない。core は自分の記録（セッション
//! ログ・ツールログ・ターン）を持ち続け、活動を**中立のイベント**として流すだけ。
//! この module はそのイベントを受け取り、gateway の DB にある**セッション単位の宛先**へ
//! 整形して配送する。
//!
//! 構成:
//! - `route`: セッション（agent + channel）ごとの宛先。保存先は gateway DB だけ。
//! - `source`: core の `tool_activity` / `subtask_lifecycle` を写した中立形と、
//!   binding ごとの待ち行列。
//! - `deliver`: 宛先があるときだけ送る配送経路（無いときはレート制限付きの debug 1 行）。
//! - `message`: 本文の整形（移設前と同一の閾値・文面。簡略化しない）。
//!
//! 配送内容は移設前と同じ:
//! - ツール活動は `tool_call_started/completed/failed/rejected` を 1 イベント 1 通。
//! - 2000 文字を超える本文は分割連投せず、出だしのプレビュー + 全文添付の **1 通**。
//! - 同一 run の配送は 1 本の mpsc + 1 worker で直列化し、順序を保つ。
//! - 429 は Retry-After を尊重し、その他失敗は backoff で再送する。

use std::time::Duration;

use tokio::sync::mpsc;

mod deliver;
mod message;
pub mod route;
pub mod source;

pub use deliver::ActivityDelivery;
pub use message::{
    attachment_filename, build_message_with_attachment_preview,
    build_message_with_optional_attachment, build_progress_message, build_started_messages,
    build_terminal_message, build_webhook_body, exit_reason_to_status, redact_webhook_url,
    validate_webhook_url, LifecycleMeta, WebhookAttachment, WebhookConfig, WebhookMessage,
    ATTACHMENT_MAX_BYTES, ATTACHMENT_PREVIEW_CHARS, ATTACHMENT_THRESHOLD_CHARS,
    DISCORD_MESSAGE_LIMIT,
};
pub use route::{AbsenceLog, RouteStore, SessionRoute};
pub use source::{ActivityEvent, ActivityItem, ActivityQueue, ActivityStatus, LifecycleEvent};

/// 送信を最終的にあきらめたとき、短いエラー説明文字列で呼ばれる give-up sink。
pub type GiveupSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// 1 回の webhook POST に許すハングの上限。
///
/// Discord webhook の応答は通常 1 秒未満。添付付き（multipart）は本文全体をボディに載せる
/// ぶん明確に重いので、遅い回線でも 8 MiB を送り切れる余裕として 60 秒を取る。ここで必ず
/// 打ち切ることで、接続が黙って死んだときに配送 worker が永久待ちに入るのを防ぐ。
const SEND_TIMEOUT_JSON: Duration = Duration::from_secs(30);
const SEND_TIMEOUT_MULTIPART: Duration = Duration::from_secs(60);

/// 配送 worker に渡す 1 バッチ。messages は同一 run 内で順序通りに送る。
#[derive(Clone, Debug)]
pub struct DeliveryBatch {
    pub url: String,
    pub messages: Vec<WebhookMessage>,
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    text.chars().take(limit).collect()
}

/// 配送チャネルと worker を起動し、送信側 sender を返す。
///
/// worker は受信した DeliveryBatch を順次処理し、各メッセージを Discord webhook へ
/// 直列送信する。sender が全て drop されチャネルが閉じると worker は終了する。
pub fn spawn_run_worker(client: reqwest::Client) -> mpsc::UnboundedSender<DeliveryBatch> {
    spawn_run_worker_with_sink(client, None)
}

/// 配送失敗時に give-up を通知するための sink を受け取れる版。
pub fn spawn_run_worker_with_sink(
    client: reqwest::Client,
    on_giveup: Option<GiveupSink>,
) -> mpsc::UnboundedSender<DeliveryBatch> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DeliveryBatch>();
    tokio::spawn(async move {
        while let Some(batch) = rx.recv().await {
            for msg in &batch.messages {
                send_with_retry(&client, &batch.url, msg, on_giveup.as_ref()).await;
            }
        }
    });
    tx
}

/// 1 メッセージ（本文 + 任意の添付）を Discord webhook へ **1 POST** で送る。
/// 429 は Retry-After を尊重し、その他失敗は best-effort backoff retry する。
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    message: &WebhookMessage,
    on_giveup: Option<&GiveupSink>,
) {
    // 即時 / 2s / 10s / 30s / 120s
    const BACKOFFS: [u64; 5] = [0, 2, 10, 30, 120];
    let mut attempt = 0usize;
    // covered 経路（配送 debug/log）では webhook URL をマスクしない。URL がマスクされると
    // 配送先の特定・障害切り分けが困難になりデバッグ性を損なうため、生 URL をそのまま記録する
    // （docs/design-webhook-output-lossless.md §2 P4）。
    let mut last_error;
    let mut attachment = message.attachment.as_ref();
    loop {
        let body = build_webhook_body(&message.content, false);
        let req = match attachment {
            Some(att) => {
                let part = reqwest::multipart::Part::bytes(att.data.clone())
                    .file_name(att.filename.clone())
                    .mime_str(&att.content_type)
                    .unwrap_or_else(|_| {
                        reqwest::multipart::Part::bytes(att.data.clone())
                            .file_name(att.filename.clone())
                    });
                // Discord webhook の multipart 仕様: メッセージ本体は `payload_json`、
                // 添付は `files[0]`。
                let form = reqwest::multipart::Form::new()
                    .text("payload_json", body.to_string())
                    .part("files[0]", part);
                client
                    .post(url)
                    .timeout(SEND_TIMEOUT_MULTIPART)
                    .multipart(form)
            }
            None => client.post(url).timeout(SEND_TIMEOUT_JSON).json(&body),
        };
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 429 {
                    let retry_after = parse_retry_after(&resp).unwrap_or(1.0);
                    tracing::warn!(url = %url, retry_after, "discord webhook 429, respecting Retry-After");
                    tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                    // 429 は同一メッセージを再送（attempt は進めない: ordering 維持）。
                    continue;
                }
                if status.is_success() {
                    return;
                }
                let response_text = resp.text().await.unwrap_or_default();
                let response_preview = truncate_chars(&response_text, 500);
                last_error = format!("http {}", status.as_u16());
                tracing::warn!(
                    url = %url,
                    status = status.as_u16(),
                    response = %response_preview,
                    "discord webhook non-success"
                );
                if attachment.is_some() && status.is_client_error() {
                    // 添付が拒否された。再送しても同じなので添付を捨て、本文だけで続行する。
                    tracing::warn!(
                        url = %url,
                        status = status.as_u16(),
                        "discord webhook rejected the attachment; retrying without it (preview only)"
                    );
                    attachment = None;
                    continue;
                }
            }
            Err(e) => {
                last_error = if e.is_timeout() {
                    "timeout".to_string()
                } else {
                    "request error".to_string()
                };
                tracing::warn!(url = %url, error = %e, "discord webhook request error");
            }
        }
        attempt += 1;
        if attempt >= BACKOFFS.len() {
            tracing::error!(url = %url, "discord webhook delivery gave up after retries");
            if let Some(sink) = on_giveup {
                sink(&last_error);
            }
            return;
        }
        tokio::time::sleep(Duration::from_secs(BACKOFFS[attempt])).await;
    }
}

// ---- Shell result summary ----

/// execute_shell の result data から抽出した出力。
#[derive(Clone, Debug, Default)]
pub struct ShellResultSummary {
    pub exit_code: Option<i64>,
    pub stdout_summary: Option<String>,
    pub stderr_summary: Option<String>,
    pub truncated: bool,
}

/// execute_shell の ActionResult.data から exit_code / stdout / stderr / truncated を取り出す。
///
/// covered 経路（work-channel 出力）のため、redaction も head/tail クランプも一切行わず
/// stdout/stderr を full のまま返す（docs/design-webhook-output-lossless.md §2 P4）。
pub fn summarize_shell_result(data: &serde_json::Value) -> ShellResultSummary {
    let exit_code = data.get("exit_code").and_then(|v| v.as_i64());
    let truncated = data
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let field = |key: &str| -> Option<String> {
        let raw = data.get(key).and_then(|v| v.as_str())?;
        if raw.is_empty() {
            return None;
        }
        Some(raw.to_string())
    };
    ShellResultSummary {
        exit_code,
        stdout_summary: field("stdout"),
        stderr_summary: field("stderr"),
        truncated,
    }
}

// ---- Tool event Discord formatting ----

/// ツールイベント 1 件の整形入力。
#[derive(Clone, Debug, Default)]
pub struct ToolEventView {
    pub event: String, // tool_call_started/completed/failed/rejected
    pub tool_name: String,
    pub tool_call_id: String,
    pub depth: u32,
    /// 入れ子の走行の相関キー（親ターンでは `None`）。どの走行で起きたかを行に出す。
    pub sub_session_id: Option<String>,
    pub status: String, // started/completed/failed/rejected
    pub duration_ms: Option<u64>,
    pub args_summary: Option<String>,
    pub result_summary: Option<String>,
    pub exit_code: Option<i64>,
    pub stdout_summary: Option<String>,
    pub stderr_summary: Option<String>,
    pub truncated: bool,
    pub rejection_reason: Option<String>,
    /// 本文プレビューの上限ヒント（0 なら既定）。#293 以降、長文は分割ではなく添付に
    /// 畳まれるため、この値は**プレビューを既定より更に短くしたいとき**にだけ効く。
    pub max_chars: usize,
}

/// ツールイベントを Discord 用メッセージへ整形する。
/// - covered 経路（work-channel 出力）のため redaction/masking は一切行わない。
/// - 1 通上限に収まれば JSON 1 通。超える場合は**分割連投せず**、出だしのプレビューを
///   本文に載せて**全文を添付ファイルにした 1 通**を返す（#293）。
pub fn build_tool_event_message(view: &ToolEventView) -> Vec<WebhookMessage> {
    let emoji = match view.status.as_str() {
        "started" => "▶️",
        "completed" => "✅",
        "failed" => "❌",
        "rejected" => "🚫",
        _ => "ℹ️",
    };
    let mut s = format!(
        "{emoji} **{}**\ntool: `{}`\ncallId: `{}`\ndepth: {}",
        view.event, view.tool_name, view.tool_call_id, view.depth
    );
    if let Some(run) = &view.sub_session_id {
        s.push_str(&format!("\nrun: `{run}`"));
    }
    if let Some(d) = view.duration_ms {
        s.push_str(&format!("\nduration: {d}ms"));
    }
    if let Some(code) = view.exit_code {
        s.push_str(&format!("\nexit_code: `{code}`"));
    }
    if let Some(reason) = &view.rejection_reason {
        s.push_str(&format!("\nrejection: {reason}"));
    }
    if let Some(args) = &view.args_summary {
        s.push_str(&format!("\nargs: {args}"));
    }
    if let Some(res) = &view.result_summary {
        s.push_str(&format!("\nresult: {res}"));
    }
    if let Some(out) = &view.stdout_summary {
        s.push_str(&format!("\nstdout:\n{out}"));
    }
    if let Some(errout) = &view.stderr_summary {
        s.push_str(&format!("\nstderr:\n{errout}"));
    }
    if view.truncated {
        // 上流（execute_shell の max_output_bytes 等）が webhook 層より前に切り捨てた
        // 部分出力。完全だと偽らず、partial であることを明示する（P5/AC5）。
        s.push_str(
            "\n⚠️ partial output: the tool truncated this before the webhook layer saw the \
             full data (upstream source limit); the omitted bytes are not available here.",
        );
    }
    vec![build_message_with_attachment_preview(
        &s,
        &format!("{}-{}", view.event, view.tool_name),
        view.max_chars,
    )]
}

/// Retry-After ヘッダ（秒）を読む。
fn parse_retry_after(resp: &reqwest::Response) -> Option<f64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod delivery_tests;

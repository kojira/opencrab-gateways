//! Webhook 宛先設定の値型と、Discord webhook へ載せる本文の整形（D-1087 で core から移設）。
//!
//! 移設前（core `crates/actions/src/webhook_target/message.rs`）と同じ整形・同じ閾値を
//! そのまま持つ。**内容の簡略化はしない**（活動の詳細と終端行を従来どおり送る）。
//! ここに残すのは値型と整形だけで、HTTP も宛先解決も持たない。

/// webhook 宛先の設定（この gateway が保持する最小形）。
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookConfig {
    pub url: String,
    /// 送信対象イベント名。None の場合は全イベントを送る。
    pub events: Option<Vec<String>>,
}

impl WebhookConfig {
    pub fn from_parts(url: String, events: Option<Vec<String>>) -> Option<WebhookConfig> {
        if url.trim().is_empty() {
            return None;
        }
        Some(WebhookConfig { url, events })
    }

    /// 指定イベントを送るべきか。events 未指定なら常に true。
    pub fn wants(&self, event: &str) -> bool {
        match &self.events {
            Some(list) => {
                // 比較は canonical な status 名で行う。活動 sink は
                // `tool_call_started`/`tool_call_completed`/... を、走行通知は
                // `subtask.started`/`started`/... を渡してくるため、両辺を正規化して
                // 同じ語彙（started/completed/failed/rejected/...）で突き合わせる。
                let want = normalize_event_name(event);
                if list.iter().any(|e| normalize_event_name(e) == want) {
                    return true;
                }
                // 後方互換: progress 導入前に作られた started/completed 購読は進捗も含む。
                want == "progress" && list.iter().any(|e| normalize_event_name(e) == "started")
            }
            None => true,
        }
    }
}

/// イベント名を canonical な status 名へ正規化する。
fn normalize_event_name(event: &str) -> &str {
    event
        .strip_prefix("subtask.")
        .or_else(|| event.strip_prefix("tool_call_"))
        .unwrap_or(event)
}

/// これを **超えたら**添付方式へ切り替える文字数（Discord の 1 メッセージ上限）。
pub const ATTACHMENT_THRESHOLD_CHARS: usize = 2000;

/// 添付に切り替えたとき、本文に載せる出だしプレビューの文字数。
pub const ATTACHMENT_PREVIEW_CHARS: usize = 600;

/// 添付ファイルの最大バイト数（これを超える分は切り詰めて明示する）。
pub const ATTACHMENT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 添付ファイルの content_type。全文はプレーンテキストなので固定。
pub const ATTACHMENT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// 添付ファイル名の最大長（拡張子込み）。
const ATTACHMENT_SLUG_MAX: usize = 48;

/// webhook に添える 1 ファイル。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookAttachment {
    /// 送信時のファイル名。**秘密・個人情報を含めない**（[`attachment_filename`] 参照）。
    pub filename: String,
    pub content_type: String,
    /// ファイル本体。`content` と**同一の文字列**から作る（マスク済みの本文がそのまま
    /// 添付になり、添付側だけ生の秘密が漏れることが構造的に起きない）。
    pub data: Vec<u8>,
    /// [`ATTACHMENT_MAX_BYTES`] を超えたため末尾を落としたか。
    pub truncated: bool,
}

/// webhook へ送る 1 通（本文 + 任意の添付）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookMessage {
    pub content: String,
    pub attachment: Option<WebhookAttachment>,
}

impl WebhookMessage {
    /// 添付なしの素のテキスト 1 通。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            attachment: None,
        }
    }

    pub fn has_attachment(&self) -> bool {
        self.attachment.is_some()
    }

    /// この 1 通で受け手に届く**全文**。添付があれば添付本体、無ければ本文そのもの。
    pub fn delivered_text(&self) -> String {
        match &self.attachment {
            Some(att) => String::from_utf8_lossy(&att.data).into_owned(),
            None => self.content.clone(),
        }
    }
}

impl From<String> for WebhookMessage {
    fn from(s: String) -> Self {
        WebhookMessage::text(s)
    }
}

impl From<&str> for WebhookMessage {
    fn from(s: &str) -> Self {
        WebhookMessage::text(s)
    }
}

/// slug から安全な添付ファイル名（`<slug>.txt`）を作る。
///
/// **秘密・個人情報をファイル名に載せない**ため、呼び出し側には「イベント名 / ツール名 /
/// 用途」といった静的な語彙だけを渡させ、ここで更に ASCII 英数と `-` `_` `.` 以外を
/// `-` へ潰し、長さも切り詰める。
pub fn attachment_filename(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['-', '.']).to_string();
    let stem: String = trimmed.chars().take(ATTACHMENT_SLUG_MAX).collect();
    if stem.is_empty() {
        "output.txt".to_string()
    } else {
        format!("{stem}.txt")
    }
}

/// `text` を [`ATTACHMENT_MAX_BYTES`] 以下のバイト列にする（UTF-8 境界を壊さない）。
fn attachment_bytes(text: &str) -> (Vec<u8>, bool) {
    if text.len() <= ATTACHMENT_MAX_BYTES {
        return (text.as_bytes().to_vec(), false);
    }
    const MARKER: &str =
        "\n\n--- [truncated] the rest was omitted because the attachment hit the size cap ---\n";
    let budget = ATTACHMENT_MAX_BYTES.saturating_sub(MARKER.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + MARKER.len());
    out.push_str(&text[..end]);
    out.push_str(MARKER);
    (out.into_bytes(), true)
}

/// 長文を「出だしのプレビュー + 全文添付」の **1 通**に畳む。
pub fn build_message_with_optional_attachment(text: &str, slug: &str) -> WebhookMessage {
    build_message_with_attachment_preview(text, slug, ATTACHMENT_PREVIEW_CHARS)
}

/// [`build_message_with_optional_attachment`] のプレビュー長を指定できる版。
pub fn build_message_with_attachment_preview(
    text: &str,
    slug: &str,
    preview_chars: usize,
) -> WebhookMessage {
    let total_chars = text.chars().count();
    if total_chars <= ATTACHMENT_THRESHOLD_CHARS {
        return WebhookMessage::text(text);
    }
    let preview_chars = if preview_chars == 0 {
        ATTACHMENT_PREVIEW_CHARS
    } else {
        preview_chars.min(ATTACHMENT_PREVIEW_CHARS)
    };
    let filename = attachment_filename(slug);
    let (data, truncated) = attachment_bytes(text);
    let preview: String = text.chars().take(preview_chars).collect();
    let note = if truncated {
        format!(
            "\n…\n\n📎 full text attached as `{filename}` ({total_chars} chars; the file was \
             truncated at {ATTACHMENT_MAX_BYTES} bytes and the tail is omitted)"
        )
    } else {
        format!("\n…\n\n📎 full text attached as `{filename}` ({total_chars} chars)")
    };
    WebhookMessage {
        content: format!("{preview}{note}"),
        attachment: Some(WebhookAttachment {
            filename,
            content_type: ATTACHMENT_CONTENT_TYPE.to_string(),
            data,
            truncated,
        }),
    }
}

/// Discord webhook の JSON body を組む。multipart のときは同じ JSON を `payload_json`
/// パートに載せる（Discord webhook の仕様）。
pub fn build_webhook_body(content: &str, suppress_mentions: bool) -> serde_json::Value {
    if suppress_mentions {
        serde_json::json!({
            "content": content,
            "allowed_mentions": { "parse": [] },
        })
    } else {
        serde_json::json!({ "content": content })
    }
}

/// webhook URL のトークン（末尾セグメント）をマスクして返す。ログ・応答用。
pub fn redact_webhook_url(url: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/[redacted]"),
        None => "[redacted]".to_string(),
    }
}

/// 共有 webhook URL の最小安全条件を検証する。
///
/// 個別 service の host や path 形式は解釈しない。理由文字列に raw URL は含めない。
pub fn validate_webhook_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("url is empty".to_string());
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "url must use https".to_string())?;
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| "url has no path".to_string())?;
    if authority.is_empty() {
        return Err("url has no host".to_string());
    }
    if authority.contains('@') {
        return Err("url must not contain credentials".to_string());
    }
    if path.is_empty() {
        return Err("url has no path".to_string());
    }
    Ok(())
}

// ---- 走行ライフサイクルの整形（subtask started / progress / terminal）----

/// lifecycle イベントの共通メタ情報（payload 整形用）。
#[derive(Clone, Debug)]
pub struct LifecycleMeta {
    pub label: String,
    pub run_id: String,
    pub session_key: String,
}

/// Discord の 1 メッセージ上限（terminal/progress の丸めに使う）。
pub const DISCORD_MESSAGE_LIMIT: usize = 2000;

/// started 用のメッセージ列を組み立てる。
///
/// 開始通知を **1 通・ヘッダ付き**で組む（`🟢 **subtask started**` + メタ + `task:` 本体）。
/// task 本体はインラインで畳み、長ければ**出だしのプレビュー + 全文添付**の 1 通にする。
/// 返り値は互換のため `Vec` だが常に 1 要素。
pub fn build_started_messages(meta: &LifecycleMeta, raw_task_text: &str) -> Vec<WebhookMessage> {
    let mut s = format!(
        "🟢 **subtask started**\nlabel: `{}`\nrunId: `{}`\nsessionKey: `{}`\nstatus: `started`",
        meta.label, meta.run_id, meta.session_key
    );
    if !raw_task_text.is_empty() {
        s.push_str("\ntask: ");
        s.push_str(raw_task_text);
    }
    vec![build_message_with_optional_attachment(
        &s,
        "subtask-started",
    )]
}

/// completed / failed / timed_out / aborted 用の簡潔なステータスメッセージ（終端行）。
pub fn build_terminal_message(
    status: &str,
    run_id: &str,
    session_key: &str,
    duration_ms: Option<u64>,
    detail: &str,
) -> String {
    let emoji = match status {
        "completed" => "✅",
        "failed" => "❌",
        "timed_out" => "⏱️",
        "aborted" => "🛑",
        _ => "ℹ️",
    };
    let dur = duration_ms
        .map(|d| format!("{}ms", d))
        .unwrap_or_else(|| "-".to_string());
    let mut s = format!(
        "{} **subtask {}**\nrunId: `{}`\nsessionKey: `{}`\nduration: {}",
        emoji, status, run_id, session_key, dur
    );
    if !detail.trim().is_empty() {
        // 1 通に収まるよう丸める（chunk 化はしない: terminal は概要のみ）。
        let label = if status == "completed" {
            "result"
        } else {
            "error"
        };
        let prefix = format!("\n{}: ", label);
        let remaining =
            DISCORD_MESSAGE_LIMIT.saturating_sub(s.chars().count() + prefix.chars().count());
        let trimmed = truncate_chars(detail, remaining);
        s.push_str(&prefix);
        s.push_str(&trimmed);
    }
    s
}

/// progress 用の短いステータスメッセージ。
pub fn build_progress_message(run_id: &str, session_key: &str, message: &str) -> String {
    let mut s = format!(
        "🔄 **subtask progress**\nrunId: `{}`\nsessionKey: `{}`",
        run_id, session_key
    );
    if !message.trim().is_empty() {
        let prefix = "\nmessage: ";
        let remaining =
            DISCORD_MESSAGE_LIMIT.saturating_sub(s.chars().count() + prefix.chars().count());
        let trimmed = truncate_chars(message, remaining);
        s.push_str(prefix);
        s.push_str(&trimmed);
    }
    s
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    text.chars().take(limit).collect()
}

/// exit_reason（走行の内部値）を webhook ステータスへ写像する。
pub fn exit_reason_to_status(exit_reason: &str) -> &'static str {
    match exit_reason {
        "timeout" => "timed_out",
        "error" => "failed",
        // "completed" / "stopped_by_limit" など
        _ => "completed",
    }
}

//! 活動イベントの供給口（D-1087 の差し込み点）。
//!
//! この module だけが「活動をどこから得るか」を知る。整形・宛先・配送はここを見ない。
//! core 側に活動イベントが載った時点で [`ActivitySource`] の実装を差し替えるだけで、
//! 配送経路は 1 行も変えない。
//!
//! # 期待する core 側の契約（option A、別タスクで実装）
//!
//! **フレーム名** `tool_activity`（core→gateway、id を持たない fire-and-forget 通知。
//! 同型の既存フレームは `activity` / `turn_failed` / `partial`。準拠 gateway は未知フレームを
//! 無視して keep するので、旧 gateway は書き込み 0 で素通しする）。
//!
//! ```text
//! {
//!   "m": "tool_activity",
//!   "binding_id": "<the binding of the turn that ran the tool>",
//!   "activity_id": "<turn activity id, same as activity frame>",
//!   "call_id": "<tool_call id>",
//!   "tool": "<tool name>",
//!   "status": "started" | "completed" | "failed" | "rejected",
//!   "depth": <u32>,
//!   "started_at": "<rfc3339>",
//!   "duration_ms": <u64>,            // terminal status only
//!   "args": <json>,                  // opaque; the gateway renders a summary
//!   "result": <json>,                // terminal only, opaque
//!   "error": "<string>"              // failed/rejected only
//! }
//! ```
//!
//! 制約（決定済み）:
//! - core は webhook / gateway の語彙を持たない（上の名前は「ツール活動」だけを指す）。
//! - 並びはターン内で発生順。フレームは同一 binding の stream に 1 本で流す。
//! - 購読者がいない（旧 gateway）ときも core の挙動と記録は変わらない。
//! - `args` / `result` は core が既に作っている値をそのまま載せる（要約は gateway 側）。

use std::sync::Arc;

/// 活動イベント 1 件（core のフレームを写した、webhook を持たない中立形）。
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEvent {
    /// ターンの activity id（同じターンのイベントは同じ値）。
    pub activity_id: String,
    pub call_id: String,
    pub tool: String,
    pub status: ActivityStatus,
    pub depth: u32,
    pub started_at: String,
    pub duration_ms: Option<u64>,
    pub args: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// ツール実行の状態。名前は従来のイベント語彙（`tool_call_*`）へ写像できる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityStatus {
    Started,
    Completed,
    Failed,
    Rejected,
}

impl ActivityStatus {
    /// 従来の webhook イベント名（`tool_call_<status>`）。
    pub fn event_name(self) -> &'static str {
        match self {
            ActivityStatus::Started => "tool_call_started",
            ActivityStatus::Completed => "tool_call_completed",
            ActivityStatus::Failed => "tool_call_failed",
            ActivityStatus::Rejected => "tool_call_rejected",
        }
    }

    /// 素の status 名（整形の見出しに使う従来値）。
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityStatus::Started => "started",
            ActivityStatus::Completed => "completed",
            ActivityStatus::Failed => "failed",
            ActivityStatus::Rejected => "rejected",
        }
    }
}

/// 活動イベントの供給元。
pub trait ActivitySource: Send + Sync {
    /// 未配送のイベントを発生順に取り出す。無ければ空。
    fn drain(&self, binding_id: &str) -> Vec<ActivityEvent>;
}

/// 今日の状態: core がまだ活動イベントを載せていない。
///
/// 何も返さない。**配送経路は動くが入力が空**というだけで、黙って壊れてはいない。
/// 起動時に 1 回だけその旨を記録し、core 側の対応が入ったら
/// [`ActivitySource`] の実装を差し替える。
pub struct UnavailableActivitySource;

impl ActivitySource for UnavailableActivitySource {
    fn drain(&self, _binding_id: &str) -> Vec<ActivityEvent> {
        Vec::new()
    }
}

/// テストと差し替えのための、外から積める供給元。
#[derive(Default)]
pub struct QueueActivitySource {
    events: std::sync::Mutex<Vec<(String, ActivityEvent)>>,
}

impl QueueActivitySource {
    pub fn push(&self, binding_id: &str, event: ActivityEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push((binding_id.to_string(), event));
        }
    }
}

impl ActivitySource for QueueActivitySource {
    fn drain(&self, binding_id: &str) -> Vec<ActivityEvent> {
        let Ok(mut events) = self.events.lock() else {
            return Vec::new();
        };
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *events)
            .into_iter()
            .partition(|(bound, _)| bound == binding_id);
        *events = kept;
        taken.into_iter().map(|(_, event)| event).collect()
    }
}

/// 供給元を共有ハンドルにする。
pub fn shared(source: impl ActivitySource + 'static) -> Arc<dyn ActivitySource> {
    Arc::new(source)
}

/// この gateway が今使う供給元。core 側の活動イベントが入ったら、ここだけを差し替える。
pub fn shared_shipped_source() -> Arc<dyn ActivitySource> {
    shared(UnavailableActivitySource)
}

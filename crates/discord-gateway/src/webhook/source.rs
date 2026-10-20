//! 活動イベントの供給口（D-1087）。
//!
//! core は活動を**中立のイベント**として流すだけ（`tool_activity` / `subtask_lifecycle`）。
//! この module はその中立形と、受信ループが積んで配送が取り出すキューだけを持つ。
//! 宛先・整形・配送はここを見ない（[`super::ActivityDelivery`] の責務）。
//!
//! core 側の対応:
//! - `tool_activity`: ツール実行 1 件（`started` / `completed` / `failed` / `rejected`）。
//! - `subtask_lifecycle`: サブタスク走行 1 件のライフサイクル。
//!
//! どちらも id を持たない fire-and-forget 通知で、core は宛先を知らない。購読しない
//! gateway では core の挙動も記録も変わらない。

/// ツール実行 1 件（core の `tool_activity` を写した、宛先を持たない中立形）。
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEvent {
    /// ターンの activity id（同じターンのイベントは同じ値）。per-turn 上限の単位。
    pub activity_id: String,
    pub call_id: String,
    pub tool: String,
    pub status: ActivityStatus,
    pub depth: u32,
    pub started_at: String,
    pub duration_ms: Option<u64>,
    /// core が持っている値そのまま（整形は配送側）。
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
    /// core の `status` 文字列から写す。未知の値は `None`（配送しない）。
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "started" => Some(ActivityStatus::Started),
            "completed" => Some(ActivityStatus::Completed),
            "failed" => Some(ActivityStatus::Failed),
            "rejected" => Some(ActivityStatus::Rejected),
            _ => None,
        }
    }

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

/// サブタスク走行 1 件のライフサイクル（core の `subtask_lifecycle`）。
///
/// `status` は `started` / `progress` / `completed` / `failed` / `timed_out` / `aborted`。
/// core はその時点で分かっている項目だけを載せるので、他は `None`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub run_id: String,
    pub status: String,
    pub label: Option<String>,
    /// started だけ（依頼本文）。
    pub task: Option<String>,
    /// progress だけ。
    pub detail: Option<String>,
    /// 終端だけ。
    pub duration_ms: Option<u64>,
    /// 終端だけ（結果本文）。
    pub result_text: Option<String>,
}

/// 配送待ちの活動 1 件。
#[derive(Clone, Debug, PartialEq)]
pub enum ActivityItem {
    Tool(ActivityEvent),
    Lifecycle(LifecycleEvent),
}

/// binding ごとの活動キュー。受信ループが積み、配送が binding 単位で全部取り出す。
///
/// 取り出しは「その binding の分だけ」なので、複数 binding を 1 gateway で扱っても
/// 混ざらない。溢れないよう上限は持たない: 配送はターン内で都度走るので、通常は
/// 数件で空になる（積み残しは「宛先が無い間に来た分」だけで、それも配送時に捨てる）。
#[derive(Default)]
pub struct ActivityQueue {
    items: std::sync::Mutex<Vec<(String, ActivityItem)>>,
}

impl ActivityQueue {
    pub fn push(&self, binding_id: &str, item: ActivityItem) {
        if let Ok(mut items) = self.items.lock() {
            items.push((binding_id.to_string(), item));
        }
    }

    /// その binding の未配送分を発生順に取り出す。
    pub fn drain(&self, binding_id: &str) -> Vec<ActivityItem> {
        let Ok(mut items) = self.items.lock() else {
            return Vec::new();
        };
        let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *items)
            .into_iter()
            .partition(|(bound, _)| bound == binding_id);
        *items = kept;
        taken.into_iter().map(|(_, item)| item).collect()
    }
}

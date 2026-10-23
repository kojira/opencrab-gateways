//! D-1092: watch の照会・設定・削除（`watch_list` / `watch_set` / `watch_delete`）。
//!
//! watch はゲートウェイ自身の状態なので、保存も適用もここで完結する（nostaro もリレーも使わない）。
//! 検証と保存は [`crate::watch_store`] が持ち、ここは invoke の三結果へ写すだけ。
//! 範囲外の値は**状態を変えずに** `Rejected` を返す。

use serde_json::{json, Value};

use opencrab_gate_client::InvokeOutcome;

use super::NostrInvokeHandler;

impl NostrInvokeHandler {
    /// D-1092: watch の照会・設定・削除。保存に失敗したら状態は変えずに拒否を返す。
    pub(super) fn watch_operation(&self, operation: &str, payload: &Value) -> InvokeOutcome {
        let Some(watches) = self.watches.as_ref() else {
            tracing::warn!(
                operation,
                "watch operation rejected; no watch store for this instance"
            );
            return InvokeOutcome::Rejected;
        };
        match operation {
            "watch_list" => InvokeOutcome::Ok(json!({
                "watches": watches.list().iter().map(|w| json!({
                    "id": w.id,
                    "interval_secs": w.interval_secs,
                    "authors": w.authors,
                    "keywords": w.keywords,
                    "kinds": w.kinds,
                })).collect::<Vec<_>>(),
            })),
            "watch_set" => match crate::watch_store::parse_set_request(payload) {
                Ok(request) => match watches.set(request) {
                    Ok(setting) => {
                        tracing::info!(
                            id = setting.id,
                            interval_secs = setting.interval_secs,
                            authors = setting.authors.len(),
                            keywords = setting.keywords.len(),
                            "watch applied"
                        );
                        InvokeOutcome::Ok(json!({
                            "id": setting.id,
                            "interval_secs": setting.interval_secs,
                            "authors": setting.authors,
                            "keywords": setting.keywords,
                            "kinds": setting.kinds,
                        }))
                    }
                    Err(reject) => {
                        tracing::warn!(error = %reject.0, "watch set rejected");
                        InvokeOutcome::Rejected
                    }
                },
                Err(reject) => {
                    tracing::warn!(error = %reject.0, "watch set rejected");
                    InvokeOutcome::Rejected
                }
            },
            "watch_delete" => {
                let Some(id) = payload.get("id").and_then(Value::as_i64) else {
                    return InvokeOutcome::Rejected;
                };
                match watches.delete(id) {
                    Ok(deleted) => InvokeOutcome::Ok(json!({"id": id, "deleted": deleted})),
                    Err(reject) => {
                        tracing::warn!(error = %reject.0, "watch delete rejected");
                        InvokeOutcome::Rejected
                    }
                }
            }
            _ => InvokeOutcome::Rejected,
        }
    }
}

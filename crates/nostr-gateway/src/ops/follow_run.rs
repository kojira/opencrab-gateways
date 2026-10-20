//! `follow` / `unfollow` の実行（D-1088 / D-1090）。
//!
//! 対象の検証と hex 化は [`crate::follow_targets`] が持つ。ここは実行だけを扱う:
//! - `user` だけ（1 件）: `nostaro <op> -- <hex>`。
//! - `users`（一括）: 0600 の一時ファイルへ書き、`nostaro <op> --from-file <file>` を 1 回だけ。
//!
//! 1 回の呼び出しで kind:3 は 1 つだけ公開される。人数分の連投（relay からの ban 対象）を
//! 作らないための経路分け。

use serde_json::{json, Value};
use std::process::Stdio;
use tokio::process::Command;

use opencrab_gate_client::InvokeOutcome;

use super::{run_to_outcome, NostrInvokeHandler, Run, OP_TIMEOUT};
use crate::secret::SECRET_ENV;

impl NostrInvokeHandler {
    /// nostaro の起動。`--config` を先頭に付け、stdin は閉じる（LLM の入力を待たせない）。
    fn command(&self, argv: &[String]) -> Command {
        let mut cmd = Command::new(&self.nostaro_bin);
        cmd.arg("--config")
            .arg(&self.config_path)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(s) = &self.secret {
            cmd.env(SECRET_ENV, s);
        }
        cmd
    }

    /// `publishes=true` は relay/外部へ書く op。nostaro の publish は「1 relay でも受理で Ok（exit 0）／
    /// 全 relay 未受理で非ゼロ」なので、非ゼロ=確定 publish なしとは限らない（どの relay も応答
    /// しなかった場合）。二重投稿防止を優先し、write op の非ゼロは Indeterminate にする（§5.3・
    /// 捏造しない）。read（resolve）は副作用が無いので非ゼロ=Rejected。spawn 失敗は外部 I/O 0 なので
    /// 双方 Rejected。
    pub(super) async fn run(&self, argv: &[String], publishes: bool) -> Run {
        self.run_capturing(argv, publishes).await.outcome
    }

    /// `follow` / `unfollow` の invoke（D-1088 / D-1090）。
    ///
    /// - `user` だけ（1 件）: `nostaro <op> -- <hex>`。従来どおり生の stdout を返す。
    /// - `users`（一括）: 解決した hex を 0600 の一時ファイルへ書き、`nostaro <op> --from-file
    ///   <file>` を **1 回だけ** 呼ぶ。人数分の kind:3 公開（relay からの ban 対象）にしない。
    ///
    /// 全件を先に検証し、1 件でも不正・上限超過・不明な mode なら nostaro を 1 回も起こさない。
    pub(super) async fn follow_or_unfollow(
        &self,
        operation: &str,
        payload: &Value,
    ) -> InvokeOutcome {
        let targets = match crate::follow_targets::parse_targets(payload) {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(
                    operation,
                    reason = error.reason(),
                    "follow target rejected before any nostaro invocation"
                );
                return InvokeOutcome::Rejected;
            }
        };
        let mode = match crate::follow_targets::FollowMode::parse(payload) {
            Ok(mode) => mode,
            Err(error) => {
                tracing::warn!(
                    operation,
                    reason = error.reason(),
                    "follow mode rejected before any nostaro invocation"
                );
                return InvokeOutcome::Rejected;
            }
        };
        if !crate::follow_targets::has_users_array(payload) {
            let argv = crate::follow_targets::invocation_argv(operation, &targets[0].hex);
            return run_to_outcome(self.run(&argv, true).await);
        }
        let file = match crate::follow_targets::TargetFile::write(&targets) {
            Ok(file) => file,
            Err(error) => {
                // 一時ファイルが作れない = 外部 I/O 0。何も投稿していない。
                tracing::warn!(operation, %error, "follow target file could not be written");
                return InvokeOutcome::Rejected;
            }
        };
        let argv = crate::follow_targets::bulk_argv(operation, file.path(), mode);
        let run = self.run_capturing(&argv, true).await;
        if !matches!(run.outcome, Run::Ok(_)) {
            tracing::warn!(
                operation,
                count = targets.len(),
                stderr = %run.stderr.trim(),
                "batch follow did not complete; nothing is reported as done"
            );
        }
        run_to_bulk_outcome(operation, mode, &targets, run)
    }

    /// [`Self::run`] と同じ実行だが、失敗時にも nostaro の出力を残す（何が起きたかを返せるように）。
    async fn run_capturing(&self, argv: &[String], publishes: bool) -> BatchRun {
        let mut cmd = self.command(argv);
        let (outcome, stderr) = match tokio::time::timeout(OP_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let outcome = if out.status.success() {
                    Run::Ok(stdout)
                } else {
                    tracing::warn!(code = ?out.status.code(), publishes, "nostaro op non-zero exit");
                    if publishes {
                        Run::Indeterminate
                    } else {
                        Run::Rejected
                    }
                };
                (outcome, stderr)
            }
            // spawn 失敗 = 外部 I/O 0（nostaro が起動していない=何も投稿していない）。
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "nostaro spawn failed");
                (Run::Rejected, e.to_string())
            }
            // timeout = 受理成否不明（kill 前に投稿済みかもしれない）。捏造しない。
            Err(_) => {
                tracing::warn!("nostaro op timeout");
                (Run::Indeterminate, String::new())
            }
        };
        BatchRun { outcome, stderr }
    }
}

/// 一括操作の実行結果。三結果に加えて nostaro の stderr を保持する（失敗の理由をログに残す）。
struct BatchRun {
    outcome: Run,
    stderr: String,
}

/// 一括操作の結果 JSON。公開は 1 回なので、対象ごとの status ではなく呼び出し全体の
/// status と nostaro の要約を返す（対象は照合できるよう input と hex を並べる）。
///
/// 確定拒否（何も投稿していない）と不確定（投稿の有無を決められない）は、単発と同じ
/// 三結果のまま返す。不確定に要約を載せる場所はプロトコルに無いので、呼び出し側が
/// ログへ残す。
fn run_to_bulk_outcome(
    operation: &str,
    mode: crate::follow_targets::FollowMode,
    targets: &[crate::follow_targets::FollowTarget],
    run: BatchRun,
) -> InvokeOutcome {
    match run.outcome {
        Run::Ok(stdout) => InvokeOutcome::Ok(json!({
            "operation": operation,
            "mode": mode.as_str(),
            "count": targets.len(),
            "status": "ok",
            "summary": stdout,
            "targets": targets
                .iter()
                .map(|target| json!({"input": target.input, "hex": target.hex}))
                .collect::<Vec<_>>(),
        })),
        Run::Rejected => InvokeOutcome::Rejected,
        Run::Indeterminate => InvokeOutcome::Indeterminate,
    }
}

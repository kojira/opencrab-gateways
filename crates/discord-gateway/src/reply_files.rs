//! reply の `files`（D-1079）。core が outbox に置いた写しを descriptor で受け取り、size と
//! sha256 を再検証して読む。gateway はワークスペースを直接読まない。

use std::path::Path;

use opencrab_gate_client::{open_verified, OutboundFile};
use serde_json::Value;

use crate::transport::ReplyFile;

/// Discord が 1 メッセージに付けられる添付の上限。
pub(crate) const MAX_REPLY_FILES: usize = 10;

/// payload の `files` を読み込む。`files` が無い・null なら空。不正な descriptor（旧 core の
/// 生パスを含む）、読めない写し、size/sha256 の不一致、上限超過、outbox 未設定は `None`
/// （呼び出し側は送信せず Rejected にする）。
pub(crate) async fn load_reply_files(
    payload: &Value,
    outbox_root: Option<&Path>,
) -> Option<Vec<ReplyFile>> {
    let value = match payload.get("files") {
        None | Some(Value::Null) => return Some(Vec::new()),
        Some(value) => value,
    };
    let outbox_root = outbox_root?.to_path_buf();
    let files = match OutboundFile::parse_list(value) {
        Ok(files) => files,
        Err(error) => {
            tracing::warn!(%error, "reply files rejected");
            return None;
        }
    };
    if files.len() > MAX_REPLY_FILES {
        tracing::warn!(count = files.len(), "reply files rejected: too many files");
        return None;
    }
    tokio::task::spawn_blocking(move || read_all(&outbox_root, files))
        .await
        .ok()?
}

fn read_all(outbox_root: &Path, files: Vec<OutboundFile>) -> Option<Vec<ReplyFile>> {
    files
        .into_iter()
        .map(|file| match open_verified(outbox_root, &file) {
            Ok(bytes) => Some(ReplyFile {
                name: file.name,
                bytes,
                sha256: file.sha256,
            }),
            Err(error) => {
                tracing::warn!(%error, name = %file.name, "reply file rejected");
                None
            }
        })
        .collect()
}

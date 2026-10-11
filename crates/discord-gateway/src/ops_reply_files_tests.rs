//! D-1079: reply の `files`（core の outbox にあるワークスペースファイルの写し）を添付する。

use std::path::Path;
use std::sync::Arc;

use opencrab_gate_client::{InvokeHandler, InvokeOutcome};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{operation_declarations, DiscordInvokeHandler};
use crate::transport::testfake::RecordingTransport;
use crate::transport::DiscordTransport;

const EVENT: &str = "discord:message:v1:100:200";

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// core が outbox に置く写しと、その descriptor を作る。
fn stage(root: &Path, name: &str, bytes: &[u8]) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(root.join("inst")).unwrap();
    std::fs::write(root.join(format!("inst/{id}.bin")), bytes).unwrap();
    json!({
        "id": id,
        "name": name,
        "media_type": "image/png",
        "size": bytes.len(),
        "sha256": sha_hex(bytes),
        "local_path": format!("inst/{id}.bin"),
    })
}

fn handler(rec: &Arc<RecordingTransport>, outbox: Option<&Path>) -> DiscordInvokeHandler {
    DiscordInvokeHandler::new(rec.clone() as Arc<dyn DiscordTransport>)
        .with_outbox_root(outbox.map(Path::to_path_buf))
}

fn reply_decl(decls: &Value) -> Value {
    decls
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "reply")
        .unwrap()
        .clone()
}

#[test]
fn reply_declares_files_only_when_outbox_is_configured() {
    let without = reply_decl(&operation_declarations(false));
    assert!(
        without["input_schema"]["properties"].get("files").is_none(),
        "{without}"
    );

    let decls = operation_declarations(true);
    let names: Vec<&str> = decls
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["join_voice", "leave_voice", "reaction", "reply", "resolve"]
    );
    let with = reply_decl(&decls);
    let files = &with["input_schema"]["properties"]["files"];
    assert_eq!(files["type"], "array");
    assert_eq!(files["items"]["type"], "string");
    assert_eq!(files["items"]["format"], "workspace-file");
    // files は任意。必須は従来どおり event と text。
    assert_eq!(with["input_schema"]["required"], json!(["event", "text"]));
    // 宣言は決定的（digest が起動ごとに変わらない）。
    assert_eq!(operation_declarations(true), decls);
}

#[tokio::test]
async fn reply_files_are_attached_to_the_first_chunk_only() {
    let outbox = tempfile::tempdir().unwrap();
    let a = stage(outbox.path(), "a.png", b"first-image");
    let b = stage(outbox.path(), "b.png", b"second-image");
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "あ".repeat(2500), "files": [a, b]}),
        )
        .await;
    assert!(matches!(out, InvokeOutcome::Ok(_)));
    assert_eq!(
        rec.kinds(),
        vec!["reply", "say"],
        "先頭 reply・続きは通常投稿"
    );
    assert_eq!(
        rec.file_names(),
        vec![vec!["a.png".to_string(), "b.png".to_string()], vec![]],
        "添付は先頭チャンク（返信）にだけ付く"
    );
}

#[tokio::test]
async fn short_reply_with_one_file_is_a_single_reply() {
    let outbox = tempfile::tempdir().unwrap();
    let a = stage(outbox.path(), "x.png", b"image");
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "どうぞ", "files": [a]}),
        )
        .await;
    assert!(matches!(out, InvokeOutcome::Ok(_)));
    assert_eq!(rec.kinds(), vec!["reply"]);
    assert_eq!(rec.file_names(), vec![vec!["x.png".to_string()]]);
}

async fn assert_rejected_without_transport(rec: &Arc<RecordingTransport>, out: InvokeOutcome) {
    assert!(matches!(out, InvokeOutcome::Rejected));
    assert!(rec.kinds().is_empty(), "送信 0 件: {:?}", rec.kinds());
}

#[tokio::test]
async fn raw_path_strings_from_an_old_core_are_rejected_without_sending() {
    let outbox = tempfile::tempdir().unwrap();
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi", "files": ["generated_images/x.png"]}),
        )
        .await;
    assert_rejected_without_transport(&rec, out).await;
}

#[tokio::test]
async fn sha_mismatch_is_rejected_without_sending() {
    let outbox = tempfile::tempdir().unwrap();
    let mut a = stage(outbox.path(), "x.png", b"image");
    a["sha256"] = json!(sha_hex(b"other"));
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi", "files": [a]}),
        )
        .await;
    assert_rejected_without_transport(&rec, out).await;
}

#[tokio::test]
async fn missing_outbox_copy_is_rejected_without_sending() {
    let outbox = tempfile::tempdir().unwrap();
    let a = stage(outbox.path(), "x.png", b"image");
    std::fs::remove_file(outbox.path().join(a["local_path"].as_str().unwrap())).unwrap();
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi", "files": [a]}),
        )
        .await;
    assert_rejected_without_transport(&rec, out).await;
}

#[tokio::test]
async fn more_than_ten_files_are_rejected_without_sending() {
    let outbox = tempfile::tempdir().unwrap();
    let files: Vec<Value> = (0..11)
        .map(|i| {
            stage(
                outbox.path(),
                &format!("{i}.png"),
                format!("img{i}").as_bytes(),
            )
        })
        .collect();
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, Some(outbox.path()))
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi", "files": files}),
        )
        .await;
    assert_rejected_without_transport(&rec, out).await;
}

#[tokio::test]
async fn files_without_a_configured_outbox_are_rejected_without_sending() {
    let outbox = tempfile::tempdir().unwrap();
    let a = stage(outbox.path(), "x.png", b"image");
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, None)
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi", "files": [a]}),
        )
        .await;
    assert_rejected_without_transport(&rec, out).await;
}

#[tokio::test]
async fn reply_without_files_is_unchanged() {
    let rec = Arc::new(RecordingTransport::default());
    let out = handler(&rec, None)
        .handle(
            "c1",
            "binding",
            "reply",
            &json!({"event": EVENT, "text": "hi"}),
        )
        .await;
    assert!(matches!(out, InvokeOutcome::Ok(_)));
    assert_eq!(rec.kinds(), vec!["reply"]);
    assert_eq!(rec.file_names(), vec![Vec::<String>::new()]);
}

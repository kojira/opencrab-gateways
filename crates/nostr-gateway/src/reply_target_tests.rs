//! D-1093: core no longer decides a reply destination. A plain say arrives without
//! `reply_target` and is published as a standalone note. A reply happens only when the model
//! calls the declared `reply` operation, whose `event` is a short-ref that core resolves to the
//! exact note the model chose; the published reply's e-tag is that note.

use super::*;
use crate::post::{deliver_say, SayDelivery};
use std::os::unix::fs::PermissionsExt as _;

fn fake_nostaro(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let args = dir.join("args");
    let bin = dir.join("fake-nostaro");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho ok\n",
            args.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, args)
}

fn argv(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A say with no reply target is a plain `post`, not a `reply`.
#[tokio::test]
async fn plain_say_without_target_is_a_standalone_post() {
    let _env = crate::ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (bin, args) = fake_nostaro(dir.path());
    let cfg = dir.path().join("cfg.toml");
    let out = deliver_say(&bin, &cfg, None, None, "タイムラインへのひとこと", false).await;
    assert_eq!(out, SayDelivery::PostedStandalone);
    let argv = argv(&args);
    assert_eq!(argv[2], "post", "{argv:?}");
    assert!(!argv.iter().any(|a| a == "reply"), "{argv:?}");
}

/// The declared `reply` takes `event` as a short-ref, so core resolves the model's eN to the
/// exact inbound note.
#[test]
fn reply_declares_event_as_a_short_ref() {
    let decl = operation_declarations();
    let reply = decl
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "reply")
        .expect("reply declared");
    assert_eq!(reply["input_schema"]["properties"]["event"]["format"], "short-ref");
    let required = reply["input_schema"]["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "event"));
}

/// An explicit reply to a chosen note publishes `reply -- <that note's id>`: the e-tag is the
/// note the model named (here a mention, which itself replies to a parent), not its parent and
/// not some other note of the turn.
#[tokio::test]
async fn explicit_reply_targets_exactly_the_chosen_note() {
    let _env = crate::ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let (bin, args) = fake_nostaro(dir.path());
    let handler = NostrInvokeHandler::new(bin, dir.path().join("cfg.toml"), None, false);
    let chosen = "cd".repeat(32);
    // The origin core resolves the model's short-ref to (same shape for either lane).
    for origin in [
        format!("nostr:event:v1:default:{chosen}"),
        format!("nostr:event:v1:watch:4:{chosen}"),
    ] {
        let out = handler
            .handle("c1", "b1", "reply", &json!({"event": origin, "text": "返信です"}))
            .await;
        assert!(matches!(out, InvokeOutcome::Ok(_)), "{origin}");
        let argv = argv(&args);
        assert_eq!(&argv[2..], ["reply", "--", chosen.as_str(), "返信です"], "{origin}");
    }
}

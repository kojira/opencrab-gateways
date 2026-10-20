//! D-1088: `follow` / `unfollow` の一括指定（`users`）と npub 入力の受け付け。

use super::*;
use nostr::{Keys, ToBech32};
use std::path::PathBuf;

/// 実 relay 不要のモック nostaro。1 呼び出し = 1 行（引数を空白区切り）で記録する。
struct Fixture {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    handler: NostrInvokeHandler,
    argv_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro");
        let argv_log = dir.path().join("argv.log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s ' \"$@\" >> '{}'\nprintf '\\n' >> '{}'\necho ok\n",
                argv_log.display(),
                argv_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let handler = NostrInvokeHandler::new(script, dir.path().join("cfg.toml"), None, false);
        Self {
            dir,
            handler,
            argv_log,
        }
    }

    /// 実行された invocation ごとの argv。
    fn invocations(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.argv_log)
            .unwrap_or_default()
            .lines()
            .map(|line| line.split_whitespace().map(str::to_string).collect())
            .collect()
    }
}

/// `--config <path> <operation> -- <hex>` の形か。hex は呼び出し側で照合する。
fn assert_invocation(call: &[String], operation: &str) {
    assert_eq!(call.first().map(String::as_str), Some("--config"));
    assert_eq!(call.get(2).map(String::as_str), Some(operation), "{call:?}");
    assert_eq!(call.get(3).map(String::as_str), Some("--"), "{call:?}");
}

fn hex_of(keys: &Keys) -> String {
    keys.public_key().to_hex()
}

#[tokio::test]
async fn batch_follow_runs_one_invocation_per_target_in_order() {
    // 子 spawn は environ を読むので env 書換テストと直列化する（#868）。
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let npub_keys = Keys::generate();
    let npub = npub_keys.public_key().to_bech32().unwrap();
    let hex_keys = Keys::generate();
    let hex = hex_of(&hex_keys);
    // core が短縮参照（u番号）を解決した結果は user に hex で入る。
    let short_ref_resolved = "11".repeat(32);

    let out = fixture
        .handler
        .handle(
            "c1",
            "b1",
            "follow",
            &json!({"user": short_ref_resolved, "users": [npub, hex.clone()]}),
        )
        .await;
    let InvokeOutcome::Ok(value) = out else {
        panic!("一括フォローは Ok を返すべき");
    };
    assert_eq!(value["count"], 3, "{value}");
    let results = value["results"].as_array().expect("per-entry results");
    assert_eq!(results.len(), 3, "{value}");
    assert!(results.iter().all(|r| r["status"] == "ok"), "{value}");

    let calls = fixture.invocations();
    assert_eq!(calls.len(), 3, "3 人なので 3 回: {calls:?}");
    assert_invocation(&calls[0], "follow");
    assert_invocation(&calls[1], "follow");
    assert_invocation(&calls[2], "follow");
    assert_eq!(calls[0][4], short_ref_resolved);
    assert_eq!(calls[1][4], hex_of(&npub_keys), "npub は hex へ変換する");
    assert_eq!(calls[2][4], hex);
}

#[tokio::test]
async fn batch_unfollow_runs_one_invocation_per_target() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();

    let out = fixture
        .handler
        .handle("c1", "b1", "unfollow", &json!({"users": [npub]}))
        .await;
    assert!(matches!(out, InvokeOutcome::Ok(_)));
    let calls = fixture.invocations();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_invocation(&calls[0], "unfollow");
    assert_eq!(calls[0][4], hex_of(&keys));
}

#[tokio::test]
async fn invalid_batch_entry_rejects_without_any_invocation() {
    // 会話の短縮参照（u2）は core が解決する single `user` でしか使えない。配列に混ざったら
    // 何も投稿せずに確定拒否する（部分実行を作らない）。
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();

    let out = fixture
        .handler
        .handle("c1", "b1", "follow", &json!({"users": [npub, "u2"]}))
        .await;
    assert!(matches!(out, InvokeOutcome::Rejected));
    assert!(
        fixture.invocations().is_empty(),
        "不正な要素があれば 1 件も実行しない"
    );
}

#[tokio::test]
async fn empty_users_list_rejects() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let out = fixture
        .handler
        .handle("c1", "b1", "follow", &json!({"users": []}))
        .await;
    assert!(matches!(out, InvokeOutcome::Rejected));
    assert!(fixture.invocations().is_empty());
}

#[tokio::test]
async fn single_user_follow_keeps_the_previous_result_shape() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let hex = "22".repeat(32);

    let out = fixture
        .handler
        .handle("c1", "b1", "follow", &json!({"user": hex}))
        .await;
    let InvokeOutcome::Ok(value) = out else {
        panic!("単発フォローは Ok を返すべき");
    };
    assert_eq!(value, json!("ok"), "単発は従来どおり生の stdout を返す");
    let calls = fixture.invocations();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_invocation(&calls[0], "follow");
    assert_eq!(calls[0][4], hex);
}

#[test]
fn declarations_advertise_batch_users_and_npub() {
    let declarations = operation_declarations();
    for name in ["follow", "unfollow"] {
        let declaration = declarations
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("{name} 宣言"));
        let schema = &declaration["input_schema"];
        let properties = &schema["properties"];
        assert_eq!(
            properties["user"]["format"], "short-ref",
            "{name}: user は core が解決する短縮参照フィールドのまま"
        );
        assert_eq!(
            properties["users"]["type"], "array",
            "{name}: users 配列が宣言されている: {schema}"
        );
        assert_eq!(properties["users"]["items"]["type"], "string", "{name}");
        let description = declaration["description"].as_str().unwrap();
        assert!(
            description.contains("npub"),
            "{name}: npub を受け付けることが説明文から読めない: {description}"
        );
        assert!(
            schema.get("required").is_none(),
            "{name}: user と users のどちらでも呼べるので必須指定にしない: {schema}"
        );
    }
}

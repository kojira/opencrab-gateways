//! D-1088: `follow` / `unfollow` の一括指定（`users`）と npub 入力の受け付け。

use super::*;
use nostr::{Keys, ToBech32};
use std::path::PathBuf;

/// 実 relay 不要のモック nostaro。1 呼び出し = 1 行（引数を空白区切り）で記録する。
/// `--from-file` が来たら、その時点のモードと中身も記録する（一時ファイルの作りを確かめるため）。
struct Fixture {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    handler: NostrInvokeHandler,
    argv_log: PathBuf,
    file_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-nostaro");
        let argv_log = dir.path().join("argv.log");
        let file_log = dir.path().join("file.log");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s ' "$@" >> '{argv}'
printf '\n' >> '{argv}'
previous=""
for argument in "$@"; do
  if [ "$previous" = "--from-file" ]; then
    printf 'mode=%s\n' "$(stat -f %Lp "$argument" 2>/dev/null || stat -c %a "$argument")" >> '{files}'
    printf 'present=%s\n' "$( [ -f "$argument" ] && echo yes || echo no )" >> '{files}'
    cat "$argument" >> '{files}'
    printf 'end\n' >> '{files}'
  fi
  previous="$argument"
done
echo 'Added the requested users; published 1 kind:3.'
"#,
                argv = argv_log.display(),
                files = file_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let handler = NostrInvokeHandler::new(script, dir.path().join("cfg.toml"), None, false);
        Self {
            dir,
            handler,
            argv_log,
            file_log,
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

    /// `--from-file` ごとに記録した (mode, 存在, 中身の行)。
    fn recorded_from_files(&self) -> Vec<(String, String, Vec<String>)> {
        let raw = std::fs::read_to_string(&self.file_log).unwrap_or_default();
        let mut records = Vec::new();
        for block in raw.split("end\n") {
            let mut mode = String::new();
            let mut present = String::new();
            let mut lines = Vec::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("mode=") {
                    mode = value.to_string();
                } else if let Some(value) = line.strip_prefix("present=") {
                    present = value.to_string();
                } else if !line.is_empty() {
                    lines.push(line.to_string());
                }
            }
            if !mode.is_empty() {
                records.push((mode, present, lines));
            }
        }
        records
    }
}

/// `--config <path> <operation> -- <hex>` の形か。hex は呼び出し側で照合する。
fn assert_invocation(call: &[String], operation: &str) {
    assert_eq!(call.first().map(String::as_str), Some("--config"));
    assert_eq!(call.get(2).map(String::as_str), Some(operation), "{call:?}");
    assert_eq!(call.get(3).map(String::as_str), Some("--"), "{call:?}");
}

/// `--config <path> <operation> --from-file <file>` の形か。
fn assert_bulk_invocation(call: &[String], operation: &str) {
    assert_eq!(call.first().map(String::as_str), Some("--config"));
    assert_eq!(call.get(2).map(String::as_str), Some(operation), "{call:?}");
    assert_eq!(
        call.get(3).map(String::as_str),
        Some("--from-file"),
        "{call:?}"
    );
}

fn hex_of(keys: &Keys) -> String {
    keys.public_key().to_hex()
}

#[tokio::test]
async fn batch_follow_is_one_invocation_that_reads_a_file() {
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

    // 1 人ずつではなく 1 回の from-file 呼び出しで済ませる（kind:3 の公開を 1 回に抑える）。
    let calls = fixture.invocations();
    assert_eq!(calls.len(), 1, "3 人でも 1 回: {calls:?}");
    assert_bulk_invocation(&calls[0], "follow");
    assert_eq!(calls[0][3], "--from-file", "{:?}", calls[0]);
    assert!(
        !calls[0].iter().any(|argument| argument == "--"),
        "positional は渡さない: {:?}",
        calls[0]
    );
    assert!(
        !calls[0].iter().any(|argument| argument == "--replace"),
        "{:?}",
        calls[0]
    );

    let records = fixture.recorded_from_files();
    assert_eq!(records.len(), 1, "{records:?}");
    let (mode, present, lines) = &records[0];
    assert_eq!(mode, "600", "一時ファイルは 0600");
    assert_eq!(present, "yes");
    assert_eq!(
        lines,
        &vec![short_ref_resolved.clone(), hex_of(&npub_keys), hex.clone()],
        "user → users の順の hex。npub も hex へ変換済み"
    );
    assert!(
        !std::path::Path::new(&calls[0][4]).exists(),
        "一時ファイルは呼び出し後に消えている"
    );
}

#[tokio::test]
async fn batch_unfollow_is_one_invocation_that_reads_a_file() {
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
    assert_bulk_invocation(&calls[0], "unfollow");
    assert_eq!(calls[0][3], "--from-file", "{:?}", calls[0]);
    assert!(
        !calls[0]
            .iter()
            .any(|argument| argument == "--only" || argument == "--all"),
        "既定は「ファイルの集合を外す」なのでフラグ不要: {:?}",
        calls[0]
    );
    assert_eq!(fixture.recorded_from_files()[0].2, vec![hex_of(&keys)]);
}

#[tokio::test]
async fn batch_follow_replace_passes_the_replace_flag() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let keys = Keys::generate();

    let out = fixture
        .handler
        .handle(
            "c1",
            "b1",
            "follow",
            &json!({"users": [hex_of(&keys)], "mode": "replace"}),
        )
        .await;
    let InvokeOutcome::Ok(value) = out else {
        panic!("Ok を返すべき");
    };
    assert_eq!(value["mode"], "replace", "{value}");
    let calls = fixture.invocations();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].iter().any(|argument| argument == "--replace"),
        "{:?}",
        calls[0]
    );
}

#[tokio::test]
async fn unknown_mode_rejects_without_any_invocation() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let keys = Keys::generate();
    let out = fixture
        .handler
        .handle(
            "c1",
            "b1",
            "follow",
            &json!({"users": [hex_of(&keys)], "mode": "destroy"}),
        )
        .await;
    assert!(matches!(out, InvokeOutcome::Rejected));
    assert!(fixture.invocations().is_empty());
}

#[tokio::test]
async fn batch_follow_reports_the_nostaro_summary() {
    let _env = crate::ENV_LOCK.lock().await;
    let fixture = Fixture::new();
    let keys = Keys::generate();
    let out = fixture
        .handler
        .handle("c1", "b1", "follow", &json!({"users": [hex_of(&keys)]}))
        .await;
    let InvokeOutcome::Ok(value) = out else {
        panic!("Ok を返すべき");
    };
    let summary = value["summary"].as_str().unwrap_or_default();
    assert!(
        summary.contains("published 1 kind:3"),
        "nostaro の出力をそのまま返す: {value}"
    );
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
    // 単発は従来どおり「生の stdout 文字列」をそのまま返す（要約 JSON に包まない）。
    assert_eq!(
        value,
        json!("Added the requested users; published 1 kind:3."),
        "単発は従来どおり生の stdout を返す"
    );
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
        // 公開は 1 回だと説明文から読める（人数分の連投をモデルに勧めない）。
        assert!(
            description.contains("公開は1回"),
            "{name}: 1 回の公開で済むことが説明文から読めない: {description}"
        );
    }
    // follow だけが mode を持つ（replace は集合の置き換え）。
    let follow = declarations
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "follow")
        .unwrap();
    assert_eq!(
        follow["input_schema"]["properties"]["mode"]["enum"],
        json!(["add", "replace"]),
        "follow は mode=add|replace を宣言する"
    );
    let unfollow = declarations
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "unfollow")
        .unwrap();
    assert!(
        unfollow["input_schema"]["properties"].get("mode").is_none(),
        "unfollow に mode は無い（既定はファイルの集合を外す）"
    );
}

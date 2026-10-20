//! `follow` / `unfollow` の対象指定（D-1088）。
//!
//! 受け付ける形は 2 つ:
//! - `user`: 1 件だけの短縮参照。core が宣言 schema の `"format":"short-ref"` を見て
//!   pubkey hex へ解決してから渡してくる（`u2` → hex）。未解決のまま来たら拒否する。
//! - `users`: 一括指定の配列。core が短縮参照を解決するのは top-level の文字列 field だけ
//!   なので、ここには hex / npub / nprofile を渡す（gateway が hex へ直す）。
//!
//! いずれも外部 I/O の前に全件を検証する（1 件でも不正なら 1 通も投稿しない）。

use serde_json::Value;

use nostr::nips::nip19::Nip19Profile;
use nostr::{FromBech32, PublicKey};

/// 1 回の invoke で受け付ける対象の上限。1 件ごとに nostaro を起こす（relay 往復あり）ので、
/// 無制限にはしない。超過は何も投稿せず確定拒否。
pub(crate) const MAX_TARGETS: usize = 100;

/// 解決済みの対象 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FollowTarget {
    /// 呼び出し元が渡した表記（結果にそのまま返して照合できるようにする）。
    pub input: String,
    /// nostaro に渡す 64 桁 hex（小文字）。
    pub hex: String,
}

/// 対象の解析エラー。理由はログに出す（外部 I/O 0 の確定拒否）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetError {
    /// `user` も `users` も実質空。
    Missing,
    /// 上限超過。
    TooMany(usize),
    /// 解決できない要素。
    Invalid { input: String, reason: &'static str },
}

impl TargetError {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "user か users のどちらかが必要です",
            Self::TooMany(_) => "一度に指定できる件数の上限を超えています",
            Self::Invalid { reason, .. } => reason,
        }
    }
}

/// hex / npub / nprofile / NIP-21 URI を 64 桁 hex にする。それ以外は `None`。
pub(crate) fn canonical_hex(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(value.to_ascii_lowercase());
    }
    if let Ok(public_key) = PublicKey::parse(value) {
        return Some(public_key.to_hex());
    }
    // nprofile（NIP-19 TLV）は PublicKey::parse では解けない。
    if let Ok(profile) = Nip19Profile::from_bech32(value) {
        return Some(profile.public_key.to_hex());
    }
    None
}

/// payload から対象一覧を組み立てる。`user` → `users` の順に並べ、同じ pubkey は 1 回だけ。
pub(crate) fn parse_targets(payload: &Value) -> Result<Vec<FollowTarget>, TargetError> {
    let mut inputs: Vec<String> = Vec::new();
    match payload.get("user") {
        None | Some(Value::Null) => {}
        Some(Value::String(user)) => {
            if !user.trim().is_empty() {
                inputs.push(user.clone());
            }
        }
        Some(_) => {
            return Err(TargetError::Invalid {
                input: String::new(),
                reason: "user は文字列で指定してください",
            })
        }
    }
    match payload.get("users") {
        None | Some(Value::Null) => {}
        Some(Value::Array(users)) => {
            for entry in users {
                match entry.as_str() {
                    Some(user) if !user.trim().is_empty() => inputs.push(user.to_string()),
                    _ => {
                        return Err(TargetError::Invalid {
                            input: entry.to_string(),
                            reason: "users の要素は空でない文字列で指定してください",
                        })
                    }
                }
            }
        }
        Some(_) => {
            return Err(TargetError::Invalid {
                input: String::new(),
                reason: "users は配列で指定してください",
            })
        }
    }
    if inputs.is_empty() {
        return Err(TargetError::Missing);
    }
    if inputs.len() > MAX_TARGETS {
        return Err(TargetError::TooMany(inputs.len()));
    }
    let mut targets: Vec<FollowTarget> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Some(hex) = canonical_hex(&input) else {
            return Err(TargetError::Invalid {
                input,
                reason: "hex でも npub でもありません（会話の u番号は user に渡してください）",
            });
        };
        if targets.iter().any(|target| target.hex == hex) {
            continue;
        }
        targets.push(FollowTarget { input, hex });
    }
    Ok(targets)
}

/// 1 件分の nostaro argv（`--` でオプション終端）。
pub(crate) fn invocation_argv(operation: &str, hex: &str) -> Vec<String> {
    vec![operation.to_string(), "--".to_string(), hex.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip19::ToBech32;
    use nostr::Keys;
    use serde_json::json;

    fn keys_hex() -> (Keys, String) {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex();
        (keys, hex)
    }

    #[test]
    fn hex_npub_and_nprofile_all_become_lowercase_hex() {
        let (npub_keys, npub_hex) = keys_hex();
        let npub = npub_keys.public_key().to_bech32().unwrap();
        let (profile_keys, profile_hex) = keys_hex();
        let nprofile = Nip19Profile::new(profile_keys.public_key(), Vec::<&str>::new())
            .unwrap()
            .to_bech32()
            .unwrap();
        let plain = Keys::generate().public_key().to_hex().to_uppercase();

        let targets = parse_targets(&json!({"users": [npub, nprofile, plain.clone()]})).unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].hex, npub_hex);
        assert_eq!(targets[1].hex, profile_hex);
        assert_eq!(targets[2].hex, plain.to_ascii_lowercase());
    }

    #[test]
    fn user_comes_first_then_users_and_duplicates_collapse() {
        let (_, hex) = keys_hex();
        let targets =
            parse_targets(&json!({"user": hex.clone(), "users": [hex.clone(), "33".repeat(32)]}))
                .unwrap();
        assert_eq!(
            targets.iter().map(|t| t.hex.clone()).collect::<Vec<_>>(),
            vec![hex, "33".repeat(32)]
        );
        assert_eq!(targets[0].input, targets[0].hex);
    }

    #[test]
    fn short_ref_inside_users_is_rejected_with_a_hint() {
        let error = parse_targets(&json!({"users": ["u2"]})).unwrap_err();
        assert!(matches!(error, TargetError::Invalid { .. }));
        assert!(error.reason().contains("user に渡してください"));
    }

    #[test]
    fn empty_payload_and_empty_array_are_missing() {
        assert_eq!(parse_targets(&json!({})).unwrap_err(), TargetError::Missing);
        assert_eq!(
            parse_targets(&json!({"users": []})).unwrap_err(),
            TargetError::Missing
        );
        assert_eq!(
            parse_targets(&json!({"user": "  "})).unwrap_err(),
            TargetError::Missing
        );
    }

    #[test]
    fn non_string_entries_are_rejected() {
        assert!(matches!(
            parse_targets(&json!({"users": [1]})).unwrap_err(),
            TargetError::Invalid { .. }
        ));
        assert!(matches!(
            parse_targets(&json!({"users": "u2"})).unwrap_err(),
            TargetError::Invalid { .. }
        ));
        assert!(matches!(
            parse_targets(&json!({"user": 5})).unwrap_err(),
            TargetError::Invalid { .. }
        ));
    }

    #[test]
    fn batch_over_the_cap_is_rejected() {
        let users: Vec<String> = (0..=MAX_TARGETS).map(|_| "44".repeat(32)).collect();
        assert_eq!(
            parse_targets(&json!({"users": users})).unwrap_err(),
            TargetError::TooMany(MAX_TARGETS + 1)
        );
    }

    #[test]
    fn argv_terminates_options_before_the_pubkey() {
        assert_eq!(
            invocation_argv("follow", &"55".repeat(32)),
            vec!["follow".to_string(), "--".to_string(), "55".repeat(32)]
        );
    }
}

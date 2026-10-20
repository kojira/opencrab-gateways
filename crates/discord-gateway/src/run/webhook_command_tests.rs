//! `/webhook` の認可テスト（D-1087）。オーナー以外は宛先を変更できない。

use super::webhook_command::{deferred_ephemeral_response, is_owner};
use crate::config::AccessConfig;

fn access() -> AccessConfig {
    AccessConfig {
        owners: vec!["owner-1".into()],
        co_agents: [("co-1".to_string(), "agent-x".to_string())]
            .into_iter()
            .collect(),
        trusted_users: vec!["trusted-1".into()],
    }
}

/// 応答が ephemeral でないと、生 URL を出すことが「チャンネル全員に見せる」になる。
/// 送信 payload の flags で ephemeral bit（64）を確かめる。
#[test]
fn the_reply_is_ephemeral_so_only_the_invoker_sees_the_url() {
    let payload = serde_json::to_value(deferred_ephemeral_response()).unwrap();
    let flags = payload["data"]["flags"].as_u64().unwrap_or_default();
    assert_eq!(flags & 64, 64, "{payload}");
}

#[test]
fn only_the_owner_may_configure_the_activity_webhook() {
    assert!(is_owner(&access(), "owner-1"));
    assert!(!is_owner(&access(), "co-1"));
    assert!(!is_owner(&access(), "trusted-1"));
    assert!(!is_owner(&access(), "stranger"));
}

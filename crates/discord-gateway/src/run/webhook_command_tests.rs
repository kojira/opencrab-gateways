//! `/webhook` の認可テスト（D-1087）。オーナー以外は宛先を変更できない。

use super::webhook_command::is_owner;
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

#[test]
fn only_the_owner_may_configure_the_activity_webhook() {
    assert!(is_owner(&access(), "owner-1"));
    assert!(!is_owner(&access(), "co-1"));
    assert!(!is_owner(&access(), "trusted-1"));
    assert!(!is_owner(&access(), "stranger"));
}

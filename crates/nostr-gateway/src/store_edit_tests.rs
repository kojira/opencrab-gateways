//! D-1097 N1: settings edits on an existing instance. Each edit bumps the desired generation
//! and returns the row to `pending`, so the daemon rebuilds the child with the new settings.

use super::*;
use crate::config::AccessConfig;
use base64::Engine as _;

const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const KEY: [u8; 32] = [3; 32];

fn config_b64(relays: &[&str]) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        serde_json::to_vec(&serde_json::json!({
            "relays": relays,
            "self_pubkey": "a".repeat(64),
            "name": "bot",
        }))
        .unwrap(),
    )
}

fn seeded() -> (tempfile::TempDir, NostrStore, u64) {
    let temp = tempfile::tempdir().unwrap();
    let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
    let generation = store
        .upsert_desired(
            ID,
            "agent",
            &config_b64(&["wss://old.example"]),
            &["addr".into()],
            "nsec-old",
            true,
            &KEY,
        )
        .unwrap();
    store.mark_provisioning(ID, generation).unwrap();
    store.mark_verified(ID, generation, true).unwrap();
    (temp, store, generation)
}

fn assert_pending_after(store: &NostrStore, before: u64) -> InstanceRow {
    let row = store.get(ID).unwrap().unwrap();
    assert_eq!(
        row.desired_generation,
        before + 1,
        "edit must bump generation"
    );
    assert_eq!(
        row.lifecycle_state,
        LifecycleState::Pending,
        "edit must hand the instance back to the daemon"
    );
    row
}

#[test]
fn update_config_replaces_runtime_config_and_keeps_credential() {
    let (_temp, store, generation) = seeded();
    let next = config_b64(&["wss://new.example", "wss://two.example"]);
    store.update_config(ID, &next).unwrap();
    let row = assert_pending_after(&store, generation);
    assert_eq!(row.config_b64, next);
    assert_eq!(&*store.decrypt_credential(ID, &KEY).unwrap(), b"nsec-old");
}

#[test]
fn set_access_replaces_identity_projections() {
    let (_temp, store, generation) = seeded();
    let access = AccessConfig {
        owner: vec!["11".repeat(32)],
        trusted_users: vec!["22".repeat(32)],
        followees: vec!["33".repeat(32)],
        co_agents: [("44".repeat(32), "peer".to_string())]
            .into_iter()
            .collect(),
    };
    store.set_access(ID, &access).unwrap();
    let row = assert_pending_after(&store, generation);
    assert_eq!(row.access, access);

    let smaller = AccessConfig {
        owner: vec!["55".repeat(32)],
        ..AccessConfig::default()
    };
    store.set_access(ID, &smaller).unwrap();
    assert_eq!(store.get(ID).unwrap().unwrap().access, smaller);
}

#[test]
fn set_credential_replaces_the_encrypted_key_only() {
    let (_temp, store, generation) = seeded();
    let before = store.get(ID).unwrap().unwrap();
    store.set_credential(ID, "nsec-new", &KEY).unwrap();
    let row = assert_pending_after(&store, generation);
    assert_eq!(row.config_b64, before.config_b64);
    assert!(!row.credential_envelope.contains("nsec-new"));
    assert_eq!(&*store.decrypt_credential(ID, &KEY).unwrap(), b"nsec-new");
    assert!(store.set_credential(ID, "  ", &KEY).is_err());
}

#[test]
fn set_enabled_toggles_and_hands_back_to_the_daemon() {
    let (_temp, store, generation) = seeded();
    store.set_enabled(ID, false).unwrap();
    let row = assert_pending_after(&store, generation);
    assert!(!row.enabled);
}

#[test]
fn edits_on_an_unknown_instance_fail_and_create_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
    assert!(store.update_config(ID, &config_b64(&["wss://x"])).is_err());
    assert!(store.set_access(ID, &AccessConfig::default()).is_err());
    assert!(store.set_credential(ID, "nsec", &KEY).is_err());
    assert!(store.set_enabled(ID, true).is_err());
    assert!(store.list().unwrap().is_empty());
}

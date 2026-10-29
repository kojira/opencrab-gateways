//! D-1097 D3: the one-time import of the instance-mode placements and token files.

use super::*;
use base64::Engine as _;
use std::os::unix::fs::PermissionsExt as _;

const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: [u8; 32] = [3; 32];

struct Core(std::collections::BTreeMap<String, (String, i64)>);

#[async_trait::async_trait]
impl SubjectLookup for Core {
    async fn core_instance(&self, instance_id: &str) -> Result<Option<(String, i64)>> {
        Ok(self.0.get(instance_id).cloned())
    }
}

fn core() -> Core {
    Core(
        [
            (A.to_string(), ("discord".to_string(), 1)),
            (B.to_string(), ("discord".to_string(), 2)),
        ]
        .into(),
    )
}

fn write_secret(path: &Path, value: &str) {
    std::fs::write(path, value).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn placement(dir: &Path, instance_id: &str, agent: &str, extra: serde_json::Value) -> PathBuf {
    let config = serde_json::json!({"agent_id":agent,"self_bot_id":"111111111111111111",
        "access":{"owners":["222222222222222222"]}});
    let mut value = serde_json::json!({
        "core_socket":"/tmp/core.sock",
        "instances":[{"instance_id":instance_id,"revision":3,
            "addresses":[format!("discord-{agent}-1-2")],
            "config_b64":base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&config).unwrap())}]
    });
    for (k, v) in extra.as_object().unwrap() {
        value[k] = v.clone();
    }
    let path = dir.join(format!("{agent}.json"));
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

fn setup() -> (tempfile::TempDir, DiscordStore, LegacyImport) {
    let temp = tempfile::tempdir().unwrap();
    let tokens = temp.path().join("tokens");
    std::fs::create_dir(&tokens).unwrap();
    write_secret(&tokens.join("agent-a.token"), "token-a\n");
    write_secret(&tokens.join("agent-b.token"), "token-b");
    let pa = placement(temp.path(), A, "agent-a", serde_json::json!({}));
    let pb = placement(temp.path(), B, "agent-b", serde_json::json!({}));
    let store = DiscordStore::open(&temp.path().join("owner.db")).unwrap();
    let import = LegacyImport {
        placements: vec![pa, pb],
        token_dir: tokens,
    };
    (temp, store, import)
}

#[tokio::test]
async fn d1097_import_copies_placements_and_tokens_into_an_empty_store() {
    let (_temp, store, import) = setup();
    let outcome = import_legacy(&store, &import, &core(), &KEY).await.unwrap();
    assert_eq!(outcome, ImportOutcome::Imported(2));
    let a = store.get(A).unwrap().unwrap();
    assert_eq!(a.agent_id, "agent-a");
    assert_eq!(a.subject_id, 1);
    assert!(a.enabled);
    assert_eq!(a.addresses, vec!["discord-agent-a-1-2"]);
    let cfg = crate::config::parse_instance_config(
        &crate::config::decode_config_b64(&a.config_b64).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg.access.owners, vec!["222222222222222222"]);
    assert_eq!(&*store.decrypt_credential(A, &KEY).unwrap(), b"token-a");
    assert_eq!(store.get(B).unwrap().unwrap().subject_id, 2);
    assert_eq!(&*store.decrypt_credential(B, &KEY).unwrap(), b"token-b");
}

#[tokio::test]
async fn d1097_import_runs_once_and_never_rereads_the_placements() {
    let (_temp, store, import) = setup();
    import_legacy(&store, &import, &core(), &KEY).await.unwrap();
    let generation = store.get(A).unwrap().unwrap().desired_generation;
    for path in &import.placements {
        std::fs::remove_file(path).unwrap();
    }
    let outcome = import_legacy(&store, &import, &core(), &KEY).await.unwrap();
    assert_eq!(outcome, ImportOutcome::StoreNotEmpty);
    assert_eq!(
        store.get(A).unwrap().unwrap().desired_generation,
        generation
    );
}

#[tokio::test]
async fn d1097_import_is_all_or_nothing() {
    let failures: Vec<Box<dyn Fn(&tempfile::TempDir, &mut LegacyImport)>> = vec![
        Box::new(|temp, _| std::fs::remove_file(temp.path().join("tokens/agent-b.token")).unwrap()),
        Box::new(|temp, _| {
            std::fs::set_permissions(
                temp.path().join("tokens/agent-b.token"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap()
        }),
        Box::new(|temp, _| write_secret(&temp.path().join("tokens/agent-b.token"), " \n")),
        Box::new(|temp, import| {
            import.placements[1] = placement(
                temp.path(),
                B,
                "agent-b",
                serde_json::json!({"run_env":{"ACCOUNT":"b"}}),
            )
        }),
        Box::new(|temp, import| {
            import.placements[1] = placement(temp.path(), A, "agent-b", serde_json::json!({}))
        }),
    ];
    for (index, break_it) in failures.into_iter().enumerate() {
        let (temp, store, mut import) = setup();
        break_it(&temp, &mut import);
        assert!(
            import_legacy(&store, &import, &core(), &KEY).await.is_err(),
            "case {index}"
        );
        assert!(store.list().unwrap().is_empty(), "case {index} wrote rows");
    }

    let (_temp, store, import) = setup();
    let missing = Core([(A.to_string(), ("discord".to_string(), 1))].into());
    assert!(import_legacy(&store, &import, &missing, &KEY)
        .await
        .is_err());
    let wrong_kind = Core(
        [
            (A.to_string(), ("discord".to_string(), 1)),
            (B.to_string(), ("nostr".to_string(), 2)),
        ]
        .into(),
    );
    assert!(import_legacy(&store, &import, &wrong_kind, &KEY)
        .await
        .is_err());
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn d1097_daemon_config_accepts_an_absolute_legacy_import() {
    let config = |placements: serde_json::Value, token_dir: &str| {
        serde_json::from_value::<crate::daemon::DaemonConfig>(serde_json::json!({
            "database_path":"/tmp/discord.db","admin_socket":"/tmp/admin.sock",
            "gate_admin_socket":"/tmp/gate-admin.sock","gate_admin_credential":"/tmp/token",
            "core_socket":"/tmp/runtime.sock","child_binary":"/bin/true","placement_dir":"/tmp/place",
            "legacy_import":{"placements":placements,"token_dir":token_dir}
        }))
        .unwrap()
    };
    config(serde_json::json!(["/tmp/a.json"]), "/tmp/tokens")
        .validate()
        .unwrap();
    assert!(config(serde_json::json!(["a.json"]), "/tmp/tokens")
        .validate()
        .is_err());
    assert!(config(serde_json::json!(["/tmp/a.json"]), "tokens")
        .validate()
        .is_err());
}

struct DownCore;

#[async_trait::async_trait]
impl SubjectLookup for DownCore {
    async fn core_instance(&self, _: &str) -> Result<Option<(String, i64)>> {
        anyhow::bail!("connect gate admin: refused")
    }
}

/// A core that cannot be reached yet is a retryable condition, distinct from a bad setup.
#[tokio::test]
async fn d1097_unreachable_core_is_retryable_and_writes_nothing() {
    let (_temp, store, import) = setup();
    let error = import_legacy(&store, &import, &DownCore, &KEY)
        .await
        .unwrap_err();
    assert!(error.is::<CoreUnavailable>(), "{error:#}");
    assert!(store.list().unwrap().is_empty());
    let (temp, store, import) = setup();
    std::fs::remove_file(temp.path().join("tokens/agent-a.token")).unwrap();
    let error = import_legacy(&store, &import, &DownCore, &KEY)
        .await
        .unwrap_err();
    assert!(
        !error.is::<CoreUnavailable>(),
        "a missing token is not retryable"
    );
}

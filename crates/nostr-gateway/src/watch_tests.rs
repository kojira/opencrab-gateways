//! D-1092: watch の宣言・invoke・実行中の車線への反映。
//!
//! 「エージェントが自分で watch を設定できる」ことの受け入れ条件を、宣言 → handler →
//! 車線の作り直しの順に、実際の関数を通して確かめる。

use std::sync::Arc;

use serde_json::json;

use crate::ops::NostrInvokeHandler;
use crate::watch_store::WatchRegistry;
use opencrab_gate_client::{InvokeHandler, InvokeOutcome};

fn handler_with(registry: Arc<WatchRegistry>) -> NostrInvokeHandler {
    NostrInvokeHandler::new(
        std::path::PathBuf::from("/nonexistent-nostaro"),
        std::path::PathBuf::from("/nonexistent-config.toml"),
        None,
        false,
    )
    .with_watches(registry)
}

#[test]
fn watch_declarations_are_owner_only_and_declare_their_effect() {
    let decls = crate::ops::operation_declarations();
    let arr = decls.as_array().unwrap();
    for (name, effect) in [
        ("watch_list", "read_only"),
        ("watch_set", "state_change"),
        ("watch_delete", "state_change"),
    ] {
        let decl = arr
            .iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("{name} must be declared"));
        assert_eq!(
            decl["authorization"]["allowed_callers"],
            json!(["owner"]),
            "{name} must be owner-only"
        );
        assert_eq!(decl["effect"], json!(effect), "{name} effect");
        // 投稿系ではないので sub-engine へは出さない。
        assert_eq!(decl["sub_engine"], json!("not_exposed"));
    }
    // 設定系は引数の形を宣言する（core は解釈しない）。
    let set = arr.iter().find(|d| d["name"] == "watch_set").unwrap();
    assert_eq!(set["input_schema"]["required"], json!(["interval_secs"]));
    for field in ["id", "interval_secs", "authors", "keywords", "kinds"] {
        assert!(
            set["input_schema"]["properties"][field].is_object(),
            "watch_set must declare {field}"
        );
    }
    let delete = arr.iter().find(|d| d["name"] == "watch_delete").unwrap();
    assert_eq!(delete["input_schema"]["required"], json!(["id"]));
}

#[tokio::test]
async fn set_list_delete_roundtrip_through_the_invoke_handler() {
    let registry = WatchRegistry::in_memory();
    let handler = handler_with(registry.clone());

    let set = handler
        .handle(
            "call-1",
            "binding-1",
            "watch_set",
            &json!({"interval_secs": 1800, "authors": ["a"], "keywords": ["k"], "kinds": [1, 7]}),
        )
        .await;
    let InvokeOutcome::Ok(created) = set else {
        panic!("watch_set must succeed");
    };
    let id = created["id"].as_i64().expect("id");

    let listed = handler
        .handle("call-2", "binding-1", "watch_list", &json!({}))
        .await;
    let InvokeOutcome::Ok(listed) = listed else {
        panic!("watch_list must succeed");
    };
    let watches = listed["watches"].as_array().expect("watches array");
    assert_eq!(watches.len(), 1);
    assert_eq!(watches[0]["id"], json!(id));
    assert_eq!(watches[0]["interval_secs"], json!(1800));
    assert_eq!(watches[0]["kinds"], json!([1, 7]));

    let deleted = handler
        .handle("call-3", "binding-1", "watch_delete", &json!({"id": id}))
        .await;
    assert!(matches!(deleted, InvokeOutcome::Ok(_)));
    assert!(registry.list().is_empty(), "delete must clear the store");
}

#[tokio::test]
async fn invalid_watch_requests_are_rejected_and_change_nothing() {
    let registry = WatchRegistry::in_memory();
    let handler = handler_with(registry.clone());
    for payload in [
        json!({"interval_secs": 10}),
        json!({"interval_secs": 90_000}),
        json!({"interval_secs": 600, "kinds": [4]}),
        json!({"interval_secs": 600, "kinds": []}),
        json!({"authors": ["a"]}),
    ] {
        let outcome = handler
            .handle("call-bad", "binding-1", "watch_set", &payload)
            .await;
        assert!(
            matches!(outcome, InvokeOutcome::Rejected),
            "must reject {payload}"
        );
        assert!(
            registry.list().is_empty(),
            "state must not change for {payload}"
        );
    }
}

#[tokio::test]
async fn watch_operations_are_rejected_without_a_store() {
    let handler = NostrInvokeHandler::new(
        std::path::PathBuf::from("/nonexistent-nostaro"),
        std::path::PathBuf::from("/nonexistent-config.toml"),
        None,
        false,
    );
    for (operation, payload) in [
        ("watch_list", json!({})),
        ("watch_set", json!({"interval_secs": 600})),
        ("watch_delete", json!({"id": 1})),
    ] {
        let outcome = handler
            .handle("call-x", "binding-1", operation, &payload)
            .await;
        assert!(
            matches!(outcome, InvokeOutcome::Rejected),
            "{operation} without a store must be rejected"
        );
    }
}

#[tokio::test]
async fn a_set_notifies_the_running_lanes() {
    let registry = WatchRegistry::in_memory();
    let waiter = registry.clone();
    let notified = tokio::spawn(async move {
        // 変更が来るまで待つ（来なければテストがタイムアウトして落ちる）。
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter.changed())
            .await
            .is_ok()
    });
    // 待ち手が登録されるのを待ってから変更する。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let handler = handler_with(registry.clone());
    let outcome = handler
        .handle(
            "call-live",
            "binding-1",
            "watch_set",
            &json!({"interval_secs": 900, "authors": ["live"]}),
        )
        .await;
    assert!(matches!(outcome, InvokeOutcome::Ok(_)));
    assert!(notified.await.unwrap(), "watch_set must wake the lanes");
}

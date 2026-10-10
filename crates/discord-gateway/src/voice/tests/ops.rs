use std::sync::Arc;

use opencrab_gate_client::{InvokeHandler, InvokeOutcome};
use serde_json::json;

use crate::ops::{operation_declarations, DiscordInvokeHandler};
use crate::transport::DryRunTransport;

#[test]
fn voice_operations_are_declared_for_owner_co_agent_trusted_only() {
    let decls = operation_declarations();
    let arr = decls.as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["join_voice", "leave_voice", "reaction", "reply", "resolve"]
    );
    for name in ["join_voice", "leave_voice"] {
        let d = arr.iter().find(|d| d["name"] == name).unwrap();
        assert_eq!(
            d["authorization"]["allowed_callers"],
            json!(["co_agent", "owner", "trusted"]),
            "{name}: guest は不可"
        );
        assert_eq!(d["dispatch"], "background");
        assert_eq!(d["effect"], "state_change");
        assert_eq!(d["sub_engine"], "not_exposed");
        assert_eq!(d["sharing"], "conversation_bound");
        assert!(d["callback_schema"].is_null());
    }
    let join = arr.iter().find(|d| d["name"] == "join_voice").unwrap();
    assert_eq!(join["input_schema"]["required"], json!(["channel_id"]));
}

#[tokio::test]
async fn reply_and_reaction_on_a_voice_origin_are_rejected() {
    let h = DiscordInvokeHandler::new(Arc::new(DryRunTransport));
    let origin = crate::map::voice_origin_for("10", "20", "30", 40);
    assert!(matches!(
        h.handle("c1", "b", "reply", &json!({"event": origin, "text": "hi"}))
            .await,
        InvokeOutcome::Rejected
    ));
    assert!(matches!(
        h.handle(
            "c2",
            "b",
            "reaction",
            &json!({"event": origin, "emoji": "👍"})
        )
        .await,
        InvokeOutcome::Rejected
    ));
    assert!(matches!(
        h.handle("c3", "b", "resolve", &json!({"ref": origin}))
            .await,
        InvokeOutcome::Rejected
    ));
}

#[tokio::test]
async fn voice_operations_without_a_voice_runtime_report_an_error_result() {
    let h = DiscordInvokeHandler::new(Arc::new(DryRunTransport));
    match h
        .handle("c1", "b", "join_voice", &json!({"channel_id": "123"}))
        .await
    {
        InvokeOutcome::Ok(v) => {
            assert_eq!(v["ok"], false);
            assert!(v["error"].as_str().unwrap().contains("voice"));
        }
        _ => panic!("expected an error result"),
    }
    // channel_id の欠落・非数字は入力不正。
    for payload in [json!({}), json!({"channel_id": "abc"})] {
        assert!(matches!(
            h.handle("c2", "b", "join_voice", &payload).await,
            InvokeOutcome::Rejected
        ));
    }
}

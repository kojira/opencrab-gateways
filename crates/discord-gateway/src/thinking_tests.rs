use super::*;

fn command(name: &str, args: Value) -> Option<InteractionAction> {
    Some(InteractionAction::Command {
        name: name.to_string(),
        args,
    })
}

#[test]
fn slash_command_definition_has_show_set_reset_with_levels_and_profile() {
    let value = serde_json::to_value(thinking_command()).unwrap();
    assert_eq!(value["name"], "thinking");
    let options = value["options"].as_array().unwrap();
    assert_eq!(
        options
            .iter()
            .map(|option| option["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["show", "set", "reset"]
    );
    let level = &options[1]["options"][0];
    assert_eq!(level["name"], "level");
    assert_eq!(level["required"], true);
    assert_eq!(
        level["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["none", "low", "medium", "high"]
    );
    for profile in [&options[1]["options"][1], &options[2]["options"][0]] {
        assert_eq!(profile["name"], "profile");
        assert_ne!(profile["required"], true);
        assert_eq!(profile["choices"].as_array().unwrap().len(), 2);
    }
}

#[test]
fn submissions_map_to_the_neutral_session_effort_commands() {
    assert_eq!(
        map_thinking_submission("show", None, None),
        command("list_session_reasoning_efforts", json!({}))
    );
    assert_eq!(
        map_thinking_submission("set", Some("high"), None),
        command("set_session_reasoning_effort", json!({"effort": "high"}))
    );
    assert_eq!(
        map_thinking_submission("set", Some("none"), Some("default")),
        command("set_session_reasoning_effort", json!({"effort": "none"}))
    );
    assert_eq!(
        map_thinking_submission("set", Some("low"), Some("realtime")),
        command(
            "set_session_reasoning_effort",
            json!({"effort": "low", "profile": "realtime"})
        )
    );
    assert_eq!(
        map_thinking_submission("reset", None, None),
        command("reset_session_reasoning_effort", json!({}))
    );
    assert_eq!(
        map_thinking_submission("reset", None, Some("realtime")),
        command(
            "reset_session_reasoning_effort",
            json!({"profile": "realtime"})
        )
    );
}

#[test]
fn invalid_levels_and_profiles_are_rejected_before_core() {
    assert_eq!(map_thinking_submission("set", Some("minimal"), None), None);
    assert_eq!(map_thinking_submission("set", Some("HIGH"), None), None);
    assert_eq!(map_thinking_submission("set", None, None), None);
    assert_eq!(
        map_thinking_submission("set", Some("low"), Some("voice")),
        None
    );
    assert_eq!(map_thinking_submission("reset", None, Some("bogus")), None);
    assert_eq!(map_thinking_submission("bogus", None, None), None);
}

#[test]
fn show_renders_both_profiles_with_their_source() {
    let text = thinking_success(
        "list_session_reasoning_efforts",
        json!({
            "scope": "session",
            "efforts": ["none", "low", "medium", "high"],
            "configured_effort": null,
            "current_effort": "high",
            "source": "agent",
            "realtime_configured_effort": null,
            "realtime_current_effort": null,
            "realtime_source": "provider"
        }),
    );
    assert_eq!(
        text.as_deref(),
        Some("This session: `high` (agent setting)\nVoice turns: provider default")
    );
    let text = thinking_success(
        "list_session_reasoning_efforts",
        json!({
            "configured_effort": "low",
            "current_effort": "low",
            "source": "session",
            "realtime_configured_effort": "none",
            "realtime_current_effort": "none",
            "realtime_source": "session"
        }),
    );
    assert_eq!(
        text.as_deref(),
        Some("This session: `low` (set for this session)\nVoice turns: `none` (set for this session)")
    );
}

#[test]
fn set_and_reset_results_use_the_echoed_profile() {
    assert_eq!(
        thinking_success(
            "set_session_reasoning_effort",
            json!({"scope": "session", "configured_effort": "medium", "current_effort": "medium",
                   "source": "session", "applies": "next_turn"})
        )
        .as_deref(),
        Some("Thinking level for this session set to `medium`. It will be used from the next turn of this session.")
    );
    assert_eq!(
        thinking_success(
            "set_session_reasoning_effort",
            json!({"scope": "session", "profile": "realtime", "configured_effort": "none",
                   "current_effort": "none", "source": "session", "applies": "next_turn"})
        )
        .as_deref(),
        Some("Thinking level for voice turns in this session set to `none`. It will be used from the next voice turn.")
    );
    assert_eq!(
        thinking_success(
            "reset_session_reasoning_effort",
            json!({"scope": "session", "configured_effort": null, "current_effort": "high",
                   "source": "agent", "applies": "next_turn"})
        )
        .as_deref(),
        Some("Removed the thinking level for this session. Now: `high` (agent setting). It will be used from the next turn of this session.")
    );
    assert_eq!(
        thinking_success(
            "reset_session_reasoning_effort",
            json!({"scope": "session", "profile": "realtime", "configured_effort": null,
                   "current_effort": null, "source": "provider", "applies": "next_turn"})
        )
        .as_deref(),
        Some("Removed the thinking level for voice turns in this session. Now: provider default. It will be used from the next voice turn.")
    );
}

#[test]
fn inconsistent_or_unknown_results_are_not_reported_as_success() {
    // A reset that still has a session value.
    assert_eq!(
        thinking_success(
            "reset_session_reasoning_effort",
            json!({"configured_effort": "low", "current_effort": "low", "source": "session",
                   "applies": "next_turn"})
        ),
        None
    );
    // Unknown profile echo.
    assert_eq!(
        thinking_success(
            "set_session_reasoning_effort",
            json!({"profile": "bogus", "configured_effort": "low", "current_effort": "low",
                   "source": "session", "applies": "next_turn"})
        ),
        None
    );
    // Untrusted level text never reaches the Discord message.
    assert_eq!(
        thinking_success(
            "set_session_reasoning_effort",
            json!({"configured_effort": "low`\n# x", "current_effort": "low`\n# x",
                   "source": "session", "applies": "next_turn"})
        ),
        None
    );
    assert_eq!(
        thinking_success(
            "set_session_model",
            json!({"configured_effort": "low", "current_effort": "low", "source": "session",
                   "applies": "next_turn"})
        ),
        None
    );
}

#[test]
fn non_owners_are_refused_like_model() {
    let refused = unbound_error(&SaidCaller::TrustedUser);
    assert_eq!(
        thinking_error_text(&refused),
        "Only an owner can use /thinking."
    );
    assert_eq!(
        thinking_error_text(&unbound_error(&SaidCaller::Owner)),
        "The thinking command failed: The binding is not available."
    );
    assert_eq!(
        thinking_error_text(&CommandError::Rejected {
            code: "forbidden".to_string(),
            message: None,
        }),
        "Only an owner can use /thinking."
    );
    assert_eq!(
        thinking_error_text(&CommandError::Rejected {
            code: "invalid_args".to_string(),
            message: None,
        }),
        "Invalid thinking level. Use none, low, medium or high."
    );
}

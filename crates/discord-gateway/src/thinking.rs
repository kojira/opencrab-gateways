//! `/thinking` (D-1086): show or change this session's reasoning effort through the neutral
//! session commands of core. Mirrors `/model`: same channel gating, owner-only, ephemeral.

use opencrab_gate_client::client::CommandError;
use opencrab_gate_client::wire::SaidCaller;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use serenity::all::{CommandOptionType, CreateCommand, CreateCommandOption};

use crate::model::InteractionAction;

pub(crate) const THINKING_EVENT_KIND: &str = "thinking_interaction";
pub(crate) const THINKING_LEVELS: [&str; 4] = ["none", "low", "medium", "high"];
const LIST_COMMAND: &str = "list_session_reasoning_efforts";
const SET_COMMAND: &str = "set_session_reasoning_effort";
const RESET_COMMAND: &str = "reset_session_reasoning_effort";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct ThinkingInteractionEvent {
    pub event_kind: String,
    pub interaction_id: String,
    pub application_id: String,
    pub token: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub user_id: String,
    pub subcommand: String,
    pub level: Option<String>,
    pub profile: Option<String>,
}

fn profile_option() -> CreateCommandOption {
    CreateCommandOption::new(
        CommandOptionType::String,
        "profile",
        "Which turns: default (typed messages) or realtime (voice). Default: default",
    )
    .add_string_choice("default", "default")
    .add_string_choice("realtime", "realtime")
}

pub(crate) fn thinking_command() -> CreateCommand {
    let show = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "show",
        "Show the thinking level of this session",
    );
    let level = THINKING_LEVELS.iter().fold(
        CreateCommandOption::new(CommandOptionType::String, "level", "Thinking level")
            .required(true),
        |option, level| option.add_string_choice(*level, *level),
    );
    let set = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "set",
        "Set the thinking level for this session",
    )
    .add_sub_option(level)
    .add_sub_option(profile_option());
    let reset = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "reset",
        "Remove this session's thinking level",
    )
    .add_sub_option(profile_option());
    CreateCommand::new("thinking")
        .description("Inspect or change the thinking level for this session")
        .set_options(vec![show, set, reset])
}

/// Maps a submission to the neutral core command. `None` = invalid arguments.
pub(crate) fn map_thinking_submission(
    subcommand: &str,
    level: Option<&str>,
    profile: Option<&str>,
) -> Option<InteractionAction> {
    let realtime = match profile {
        None | Some("default") => false,
        Some("realtime") => true,
        Some(_) => return None,
    };
    let (name, mut args) = match subcommand {
        "show" if level.is_none() && profile.is_none() => (LIST_COMMAND, json!({})),
        "set" => {
            let level = level.filter(|level| THINKING_LEVELS.contains(level))?;
            (SET_COMMAND, json!({"effort": level}))
        }
        "reset" if level.is_none() => (RESET_COMMAND, json!({})),
        _ => return None,
    };
    if realtime {
        args["profile"] = json!("realtime");
    }
    Some(InteractionAction::Command {
        name: name.to_string(),
        args,
    })
}

#[derive(Debug, Deserialize)]
struct EffortList {
    configured_effort: Option<String>,
    current_effort: Option<String>,
    source: String,
    realtime_configured_effort: Option<String>,
    realtime_current_effort: Option<String>,
    realtime_source: String,
}

#[derive(Debug, Deserialize)]
struct EffortMutation {
    profile: Option<String>,
    configured_effort: Option<String>,
    current_effort: Option<String>,
    source: String,
    applies: String,
}

/// Renders a value plus where it comes from; `None` when the result is inconsistent.
fn describe(current: Option<&str>, source: &str) -> Option<String> {
    match (source, current) {
        ("session", Some(level)) => Some(format!("`{level}` (set for this session)")),
        ("agent", Some(level)) => Some(format!("`{level}` (agent setting)")),
        ("provider", None) => Some("provider default".to_string()),
        _ => None,
    }
}

fn known_level(level: Option<&str>) -> bool {
    level.is_none_or(|level| THINKING_LEVELS.contains(&level))
}

pub(crate) fn thinking_success(name: &str, value: Value) -> Option<String> {
    if name == LIST_COMMAND {
        let list: EffortList = serde_json::from_value(value).ok()?;
        let all = [
            &list.configured_effort,
            &list.current_effort,
            &list.realtime_configured_effort,
            &list.realtime_current_effort,
        ];
        if !all.iter().all(|level| known_level(level.as_deref())) {
            return None;
        }
        return Some(format!(
            "This session: {}\nVoice turns: {}",
            describe(list.current_effort.as_deref(), &list.source)?,
            describe(
                list.realtime_current_effort.as_deref(),
                &list.realtime_source
            )?
        ));
    }
    let result: EffortMutation = serde_json::from_value(value).ok()?;
    if result.applies != "next_turn"
        || !known_level(result.configured_effort.as_deref())
        || !known_level(result.current_effort.as_deref())
    {
        return None;
    }
    let voice = match result.profile.as_deref() {
        None => false,
        Some("realtime") => true,
        Some(_) => return None,
    };
    let (turns, next) = if voice {
        ("voice turns in this session", "the next voice turn")
    } else {
        ("this session", "the next turn of this session")
    };
    match name {
        SET_COMMAND => {
            let level = result.configured_effort.as_deref()?;
            if result.source != "session" || result.current_effort.as_deref() != Some(level) {
                return None;
            }
            Some(format!(
                "Thinking level for {turns} set to `{level}`. It will be used from {next}."
            ))
        }
        RESET_COMMAND => {
            if result.configured_effort.is_some() {
                return None;
            }
            Some(format!(
                "Removed the thinking level for {turns}. Now: {}. It will be used from {next}.",
                describe(result.current_effort.as_deref(), &result.source)?
            ))
        }
        _ => None,
    }
}

pub(crate) fn thinking_error_text(error: &CommandError) -> String {
    match error {
        CommandError::Rejected { code, .. } if code == "forbidden" => {
            "Only an owner can use /thinking.".to_string()
        }
        CommandError::Rejected { code, .. } if code == "invalid_args" => {
            "Invalid thinking level. Use none, low, medium or high.".to_string()
        }
        CommandError::Rejected { code, .. } if code == "unknown_message" => {
            "This server does not support /thinking yet (unknown_message).".to_string()
        }
        CommandError::Timeout => "The thinking command timed out. Try again.".to_string(),
        CommandError::Rejected { code, .. } => format!("The thinking command failed ({code})."),
        CommandError::NotReady => {
            "The thinking command failed: The binding is not available.".to_string()
        }
        CommandError::Disconnected => "The thinking command failed: disconnect".to_string(),
    }
}

/// The error when this channel has no live binding: owners learn it is not ready, everyone
/// else is refused exactly as core would refuse them.
pub(crate) fn unbound_error(caller: &SaidCaller) -> CommandError {
    if matches!(caller, SaidCaller::Owner) {
        CommandError::NotReady
    } else {
        CommandError::Rejected {
            code: "forbidden".to_string(),
            message: Some("Only an owner may use thinking commands.".to_string()),
        }
    }
}

#[cfg(test)]
#[path = "thinking_tests.rs"]
mod tests;

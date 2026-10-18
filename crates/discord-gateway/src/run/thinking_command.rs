//! `/thinking` (D-1086) handling. Same channel gating, owner check, ephemeral deferral and
//! response editing as `/model`.

use super::*;
use crate::thinking::{
    map_thinking_submission, thinking_error_text, thinking_success, unbound_error,
    ThinkingInteractionEvent,
};

/// Handles `line` when it is a `/thinking` interaction; `false` = not ours, keep routing.
/// `gate` = (client, agent id, access, configured addresses), as for `/model`.
pub(super) async fn dispatch(
    gate: (&InstanceClient, &str, &AccessConfig, &[String]),
    http: Option<&serenity::http::Http>,
    line: &str,
) -> bool {
    let (client, agent_id, access, configured_addresses) = gate;
    let Ok(event) = serde_json::from_str::<ThinkingInteractionEvent>(line) else {
        return false;
    };
    if event.event_kind != crate::thinking::THINKING_EVENT_KIND {
        return false;
    }
    if let Some(http) = http {
        handle_thinking_interaction(client, agent_id, access, configured_addresses, http, event)
            .await;
    }
    true
}

async fn handle_thinking_interaction(
    client: &InstanceClient,
    agent_id: &str,
    access: &AccessConfig,
    configured_addresses: &[String],
    http: &serenity::http::Http,
    event: ThinkingInteractionEvent,
) {
    let (Ok(interaction_id), Ok(application_id)) = (
        event.interaction_id.parse::<u64>(),
        event.application_id.parse::<u64>(),
    ) else {
        return;
    };
    http.set_application_id(serenity::all::ApplicationId::new(application_id));
    let interaction_id = serenity::all::InteractionId::new(interaction_id);
    let guild = event.guild_id.as_deref().unwrap_or("");
    let requested_address = address_for(agent_id, guild, &event.channel_id);
    // Not our channel: stay silent so the gateway that owns it answers the interaction.
    let Some(address) = model_command_address(&requested_address, configured_addresses) else {
        return;
    };
    let caller = caller_for(access, &event.user_id);
    let transport = resolve_model_command_transport(client, address).await;

    let deferred = serenity::all::CreateInteractionResponse::Defer(
        serenity::all::CreateInteractionResponseMessage::new().ephemeral(true),
    );
    if let Err(error) = http
        .create_interaction_response(interaction_id, &event.token, &deferred, Vec::new())
        .await
    {
        tracing::warn!(error = %crate::secret::redact_token(&error.to_string()), "failed to defer /thinking response");
        return;
    }

    let Some(InteractionAction::Command { name, args }) = map_thinking_submission(
        &event.subcommand,
        event.level.as_deref(),
        event.profile.as_deref(),
    ) else {
        edit_model_response(
            http,
            &event.token,
            "Invalid thinking level. Use none, low, medium or high.",
        )
        .await;
        return;
    };
    let outcome = match transport {
        Some((address, _)) => {
            client
                .command(&address, &caller, &name, &args, COMMAND_TIMEOUT)
                .await
        }
        None => Err(unbound_error(&caller)),
    };
    let text = match outcome {
        Ok(value) => thinking_success(&name, value)
            .unwrap_or_else(|| "The thinking command failed: invalid server response".to_string()),
        Err(error) => thinking_error_text(&error),
    };
    edit_model_response(http, &event.token, &text).await;
}

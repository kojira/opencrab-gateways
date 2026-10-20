//! `/model` のインタラクション処理（D-1087 で run.rs から分割、挙動は変えない）。
//!
//! `/model` は global 登録だが、gateway は自分が設定されたチャンネルでだけ答える
//! （同じ Discord application を複数 gateway が共有しうるため）。`/thinking` `/webhook` と
//! 同じ流儀（ephemeral defer → edit）。

use super::*;
use crate::model::{
    autocomplete_choices, list_result, map_model_submission, ModelInteractionEvent,
    ModelListResult, AUTOCOMPLETE_TIMEOUT, LIST_COMMAND, MODEL_CACHE_TTL,
};
use crate::model::{command_error_text, submission_success};
use opencrab_gate_client::client::CommandError;

#[derive(Clone)]
pub(super) struct CachedModels {
    fetched_at: std::time::Instant,
    result: ModelListResult,
}

pub(super) type ModelCache = Arc<tokio::sync::Mutex<HashMap<String, CachedModels>>>;

// `/model` is registered globally, but a gateway answers it only in the channels it is configured
// for. Several gateways may share one Discord application (e.g. production and an isolated QC
// cohort); answering elsewhere would race another gateway's interaction and act on the wrong core.
pub(super) fn model_command_address<'a>(
    requested_address: &'a str,
    configured_addresses: &[String],
) -> Option<&'a str> {
    configured_addresses
        .iter()
        .any(|address| address == requested_address)
        .then_some(requested_address)
}

pub(super) async fn resolve_model_command_transport(
    client: &InstanceClient,
    address: &str,
) -> Option<(String, String)> {
    let binding_id = client.binding_for_address(address).await?;
    Some((address.to_string(), binding_id))
}

pub(super) async fn handle_model_interaction(
    client: &InstanceClient,
    agent_id: &str,
    access: &AccessConfig,
    configured_addresses: &[String],
    http: &serenity::http::Http,
    cache: &ModelCache,
    event: ModelInteractionEvent,
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

    if event.autocomplete {
        let models = match transport.as_ref() {
            Some((address, binding_id)) => {
                autocomplete_models(client, address, binding_id, &caller, cache).await
            }
            None => Vec::new(),
        };
        let choices = autocomplete_choices(&models, event.model.as_deref().unwrap_or_default());
        let response = choices.into_iter().fold(
            serenity::all::CreateAutocompleteResponse::new(),
            |response, choice| response.add_string_choice(choice.name, choice.value),
        );
        let response = serenity::all::CreateInteractionResponse::Autocomplete(response);
        if let Err(error) = http
            .create_interaction_response(interaction_id, &event.token, &response, Vec::new())
            .await
        {
            tracing::debug!(error = %crate::secret::redact_token(&error.to_string()), "model autocomplete response failed");
        }
        return;
    }

    let deferred = serenity::all::CreateInteractionResponse::Defer(
        serenity::all::CreateInteractionResponseMessage::new().ephemeral(true),
    );
    if let Err(error) = http
        .create_interaction_response(interaction_id, &event.token, &deferred, Vec::new())
        .await
    {
        tracing::warn!(error = %crate::secret::redact_token(&error.to_string()), "failed to defer /model response");
        return;
    }

    let Some(InteractionAction::Command { name, args }) =
        map_model_submission(&event.subcommand, event.model.as_deref())
    else {
        edit_model_response(
            http,
            &event.token,
            "The model command failed: Invalid command arguments.",
        )
        .await;
        return;
    };
    let binding_id = transport.as_ref().map(|(_, binding_id)| binding_id.clone());
    let outcome = match transport {
        Some((address, _)) => {
            client
                .command(&address, &caller, &name, &args, COMMAND_TIMEOUT)
                .await
        }
        None if matches!(caller, SaidCaller::Owner) => Err(CommandError::NotReady),
        None => Err(CommandError::Rejected {
            code: "forbidden".to_string(),
            message: Some("Only an owner may use model commands.".to_string()),
        }),
    };
    let text = match outcome {
        Ok(value) => {
            if name == LIST_COMMAND {
                if let (Some(binding_id), Some(result)) = (binding_id, list_result(value.clone())) {
                    cache.lock().await.insert(
                        binding_id,
                        CachedModels {
                            fetched_at: std::time::Instant::now(),
                            result,
                        },
                    );
                }
            }
            submission_success(&name, value)
                .unwrap_or_else(|| "The model command failed: invalid server response".to_string())
        }
        Err(error) => command_error_text(event.model.as_deref(), &error),
    };
    edit_model_response(http, &event.token, &text).await;
}

async fn autocomplete_models(
    client: &InstanceClient,
    address: &str,
    binding_id: &str,
    caller: &SaidCaller,
    cache: &ModelCache,
) -> Vec<String> {
    let now = std::time::Instant::now();
    if let Some(entry) = cache.lock().await.get(binding_id).cloned() {
        if now.duration_since(entry.fetched_at) < MODEL_CACHE_TTL {
            return entry.result.models;
        }
    }
    let Ok(value) = client
        .command(
            address,
            caller,
            LIST_COMMAND,
            &serde_json::json!({}),
            AUTOCOMPLETE_TIMEOUT,
        )
        .await
    else {
        return Vec::new();
    };
    let Some(result) = list_result(value) else {
        return Vec::new();
    };
    let models = result.models.clone();
    cache.lock().await.insert(
        binding_id.to_string(),
        CachedModels {
            fetched_at: std::time::Instant::now(),
            result,
        },
    );
    models
}

pub(super) async fn edit_model_response(http: &serenity::http::Http, token: &str, text: &str) {
    let allowed_mentions = serenity::all::CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false);
    let edit = serenity::all::EditInteractionResponse::new()
        .content(text)
        .allowed_mentions(allowed_mentions);
    if let Err(error) = http
        .edit_original_interaction_response(token, &edit, Vec::new())
        .await
    {
        tracing::warn!(error = %crate::secret::redact_token(&error.to_string()), "failed to edit /model response");
    }
}

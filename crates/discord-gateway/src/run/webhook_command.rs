//! `/webhook` (D-1087) の処理。`/thinking` `/model` と同じ流儀（チャンネル判定・オーナー
//! 限定・ephemeral・defer → edit）。
//!
//! 宛先は gateway の DB にだけ書く（core へはコマンドを送らない）。`set` は対象チャンネルに
//! webhook を作る（既にこの gateway のものがあれば再利用する）。

use super::*;
use crate::webhook::route::RouteStore;
use crate::webhook_command::{
    clear_text, error_text, parse_subcommand, set_text, show_text, WebhookInteractionEvent,
    WebhookSubcommand, WEBHOOK_EVENT_KIND, WEBHOOK_NAME,
};
use secrecy::ExposeSecret as _;

/// イベント行が `/webhook` なら処理して true。
pub(super) async fn dispatch(
    gate: (&InstanceClient, &str, &AccessConfig, &[String]),
    store: Option<&RouteStore>,
    http: Option<&serenity::http::Http>,
    line: &str,
) -> bool {
    let Ok(event) = serde_json::from_str::<WebhookInteractionEvent>(line) else {
        return false;
    };
    if event.event_kind != WEBHOOK_EVENT_KIND {
        return false;
    }
    let (client, agent_id, access, configured_addresses) = gate;
    let _ = client;
    if let Some(http) = http {
        handle(agent_id, access, configured_addresses, store, http, event).await;
    }
    true
}

async fn handle(
    agent_id: &str,
    access: &AccessConfig,
    configured_addresses: &[String],
    store: Option<&RouteStore>,
    http: &serenity::http::Http,
    event: WebhookInteractionEvent,
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
    // 自分の管轄チャンネルでなければ黙る（そのチャンネルを持つ gateway が答える）。
    let Some(_address) = model_command_address(&requested_address, configured_addresses) else {
        return;
    };
    let deferred = serenity::all::CreateInteractionResponse::Defer(
        serenity::all::CreateInteractionResponseMessage::new().ephemeral(true),
    );
    if let Err(error) = http
        .create_interaction_response(interaction_id, &event.token, &deferred, Vec::new())
        .await
    {
        tracing::warn!(error = %crate::secret::redact_token(&error.to_string()), "failed to defer /webhook response");
        return;
    }

    let text = run_subcommand(agent_id, access, store, http, &event).await;
    edit_model_response(http, &event.token, &text).await;
}

async fn run_subcommand(
    agent_id: &str,
    access: &AccessConfig,
    store: Option<&RouteStore>,
    http: &serenity::http::Http,
    event: &WebhookInteractionEvent,
) -> String {
    // 宛先の変更はオーナーだけ（他の管理コマンドと同じ）。
    if !is_owner(access, &event.user_id) {
        return error_text("only the owner can configure the activity webhook");
    }
    let Some(store) = store else {
        return error_text("this gateway has no activity store configured");
    };
    let Some(subcommand) = parse_subcommand(&event.subcommand) else {
        return error_text("unknown subcommand");
    };
    match subcommand {
        WebhookSubcommand::Show => match store.get(agent_id, &event.channel_id) {
            Ok(Some(route)) => show_text(Some((
                &crate::webhook::redact_webhook_url(&route.url),
                route.enabled,
            ))),
            Ok(None) => show_text(None),
            Err(error) => error_text(&error.to_string()),
        },
        WebhookSubcommand::Set => {
            let destination = event
                .destination_channel_id
                .as_deref()
                .unwrap_or(&event.channel_id);
            let Ok(channel) = destination.parse::<u64>() else {
                return error_text("the destination channel is not a valid id");
            };
            match ensure_webhook_url(http, channel).await {
                Ok(url) => {
                    let route = crate::webhook::SessionRoute {
                        agent_id: agent_id.to_string(),
                        channel_id: event.channel_id.clone(),
                        url,
                        events: None,
                        enabled: true,
                    };
                    match store.set(&route, now_ms()) {
                        Ok(()) => {
                            set_text(&crate::webhook::redact_webhook_url(&route.url), destination)
                        }
                        Err(error) => error_text(&error.to_string()),
                    }
                }
                Err(message) => error_text(&message),
            }
        }
        WebhookSubcommand::Clear => match store.clear(agent_id, &event.channel_id) {
            Ok(removed) => clear_text(removed),
            Err(error) => error_text(&error.to_string()),
        },
    }
}

/// 対象チャンネルの webhook URL を返す（この gateway のものがあれば再利用、無ければ作る）。
///
/// 失敗理由に URL / token を含めない。
async fn ensure_webhook_url(
    http: &serenity::http::Http,
    channel_id: u64,
) -> Result<String, String> {
    let channel = serenity::all::ChannelId::new(channel_id);
    let existing = http
        .get_channel_webhooks(channel)
        .await
        .map_err(|error| {
            format!(
                "could not read the channel's webhooks: {}",
                redacted(&error)
            )
        })?
        .into_iter()
        .find(|webhook| webhook.name.as_deref() == Some(WEBHOOK_NAME) && webhook.token.is_some());
    let (id, token) = match existing {
        Some(webhook) => (
            webhook.id.get(),
            webhook
                .token
                .map(|token| token.expose_secret().to_string())
                .unwrap_or_default(),
        ),
        None => {
            let created = http
                .create_webhook(
                    channel,
                    &serenity::all::CreateWebhook::new(WEBHOOK_NAME),
                    None,
                )
                .await
                .map_err(|error| {
                    format!(
                        "could not create a webhook in that channel: {}",
                        redacted(&error)
                    )
                })?;
            (
                created.id.get(),
                created
                    .token
                    .map(|token| token.expose_secret().to_string())
                    .unwrap_or_default(),
            )
        }
    };
    if token.is_empty() {
        return Err("the channel webhook has no token; cannot deliver".to_string());
    }
    let url = format!("https://discord.com/api/webhooks/{id}/{token}");
    // 保存前に必ず検証する（https + host + path）。
    crate::webhook::validate_webhook_url(&url)?;
    Ok(url)
}

/// 宛先を変更してよいか。オーナーだけ。
pub(super) fn is_owner(access: &AccessConfig, user_id: &str) -> bool {
    matches!(caller_for(access, user_id), SaidCaller::Owner)
}

fn redacted(error: &serenity::Error) -> String {
    crate::secret::redact_token(&error.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

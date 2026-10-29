//! D-1097: an owner @mentioning the bot in a channel it does not have yet enables that channel:
//! the child creates the binding on its core connection (`create_binding`), waits for the bind,
//! and the message then goes on as a normal said. The daemon records the address.

use crate::config::AccessConfig;
use crate::map::IncomingMessage;

/// Starts the say consumer for a newly bound address (so answers, typing, reactions and
/// activity reach the new channel like any configured one).
pub(crate) type SayConsumers = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// Whether `msg` should enable the message's channel.
pub(crate) fn wants_channel(
    access: &AccessConfig,
    self_bot_id: &str,
    msg: &IncomingMessage,
    already_bound: bool,
) -> bool {
    !already_bound
        && msg.mentions.iter().any(|id| id == self_bot_id)
        && access.owners.iter().any(|id| id == &msg.author.id)
}

/// The binding id for `address` of `instance_id`: the same rule as the daemon's gate-admin
/// reconcile, so both sides name the same binding.
pub(crate) fn binding_id_for(instance_id: &str, address: &str) -> Option<String> {
    let namespace = uuid::Uuid::parse_str(instance_id).ok()?;
    Some(uuid::Uuid::new_v5(&namespace, address.as_bytes()).to_string())
}

/// Creates the binding on this child's own core connection, waits for core's bind, then tells
/// the daemon (when there is one) so the store keeps it. True = the channel is bound.
pub(crate) async fn enable_channel(
    client: &opencrab_gate_client::client::InstanceClient,
    channel_socket: Option<&std::path::Path>,
    address: &str,
) -> bool {
    let Some(binding_id) = binding_id_for(&client.instance_id, address) else {
        return false;
    };
    if let Err(error) = client
        .create_binding(&binding_id, address, "Discord channel")
        .await
    {
        tracing::warn!(%address, error = ?error, "owner mention: core refused the channel");
        return false;
    }
    let mut bound = false;
    for _ in 0..300 {
        if client.binding_for_address(address).await.is_some() {
            bound = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if !bound {
        tracing::warn!(%address, "owner mention: core did not bind the channel in time");
        return false;
    }
    if let Some(socket) = channel_socket {
        if let Err(error) = crate::channel_request::request(socket, address, &binding_id).await {
            tracing::warn!(%address, error = %error, "owner mention: daemon did not record the channel");
        }
    }
    tracing::info!(%address, "owner mention: channel enabled");
    true
}

/// For one received line: if an owner @mentioned the bot in a channel it does not have, enable
/// that channel and wait for its bind, so the normal handling that follows delivers the message.
pub(crate) async fn maybe_enable(
    (client, agent_id, self_bot_id, access): (
        &opencrab_gate_client::client::InstanceClient,
        &str,
        &str,
        &AccessConfig,
    ),
    (channel_socket, say_consumers): (Option<&std::path::Path>, Option<&SayConsumers>),
    line: &str,
) {
    let Some(msg) = crate::map::parse_event_line(line) else {
        return;
    };
    let address = crate::map::address_of(agent_id, &msg);
    let bound = client.binding_for_address(&address).await.is_some();
    if wants_channel(access, self_bot_id, &msg, bound)
        && enable_channel(client, channel_socket, &address).await
    {
        if let Some(spawn) = say_consumers {
            spawn(address);
        }
    }
}

/// The [`SayConsumers`] of a running instance: each call starts the say consumer for one more
/// address with the same transport, reactions, delivery targets, voice and activity.
pub(crate) fn say_consumers(
    client: &std::sync::Arc<opencrab_gate_client::client::InstanceClient>,
    transport: &std::sync::Arc<dyn crate::transport::DiscordTransport>,
    cfg: &crate::config::InstanceConfig,
    delivery_targets: &crate::ops::BindingDeliveryTargets,
    (voice, activity): (
        &Option<std::sync::Arc<crate::voice::session::VoiceManager>>,
        &Option<std::sync::Arc<crate::webhook::ActivityDelivery>>,
    ),
) -> SayConsumers {
    let client = client.clone();
    let transport = transport.clone();
    let agent_id = cfg.agent_id.clone();
    let reactions = cfg.system_reactions.clone();
    let delivery_targets = delivery_targets.clone();
    let voice = voice.clone();
    let activity = activity.clone();
    std::sync::Arc::new(move |address: String| {
        super::spawn_say_consumer(
            client.clone(),
            address,
            transport.clone(),
            agent_id.clone(),
            reactions.clone(),
            std::sync::Arc::clone(&delivery_targets),
            voice.clone(),
            activity.clone(),
        );
    })
}

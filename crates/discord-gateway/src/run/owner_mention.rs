//! D-1097: an owner @mentioning the bot in a channel it does not listen to yet enables that
//! channel: the bot adds it to its own address set, starts the channel's say consumer, and the
//! message then goes on as a normal said. In daemon mode the daemon also records the address so
//! it survives a restart.
//!
//! D-1099: core is not asked. Which channels a bot listens to is gateway state; the first said of
//! the channel's session key opens the conversation in core.

use crate::config::AccessConfig;
use crate::map::IncomingMessage;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// The channel addresses (= core session keys) this bot listens to.
pub(crate) type Listening = Arc<RwLock<HashSet<String>>>;

pub(crate) fn listening(addresses: &[String]) -> Listening {
    Arc::new(RwLock::new(addresses.iter().cloned().collect()))
}

pub(crate) fn listens(listening: &Listening, address: &str) -> bool {
    listening
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(address)
}

/// Starts the say consumer for a newly enabled address (so answers, typing, reactions and
/// activity reach the new channel like any configured one).
pub(crate) type SayConsumers = Arc<dyn Fn(String) + Send + Sync>;

/// Whether `msg` should enable the message's channel.
pub(crate) fn wants_channel(
    access: &AccessConfig,
    self_bot_id: &str,
    msg: &IncomingMessage,
    already_listening: bool,
) -> bool {
    !already_listening
        && msg.mentions.iter().any(|id| id == self_bot_id)
        && access.owners.iter().any(|id| id == &msg.author.id)
}

/// Adds `address` to the bot's set and tells the daemon (when there is one) so the store keeps it.
/// True = the bot listens to the channel now (it did not before).
pub(crate) async fn enable_channel(
    listening: &Listening,
    channel_socket: Option<&std::path::Path>,
    address: &str,
) -> bool {
    let added = listening
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(address.to_string());
    if !added {
        return false;
    }
    if let Some(socket) = channel_socket {
        if let Err(error) = crate::channel_request::request(socket, address).await {
            tracing::warn!(%address, error = %error, "owner mention: daemon did not record the channel");
        }
    }
    tracing::info!(%address, "owner mention: channel enabled");
    true
}

/// For one received line: if an owner @mentioned the bot in a channel it does not listen to,
/// enable that channel, so the normal handling that follows delivers the message.
pub(crate) async fn maybe_enable(
    (agent_id, self_bot_id, access): (&str, &str, &AccessConfig),
    (listening, channel_socket, say_consumers): (
        &Listening,
        Option<&std::path::Path>,
        Option<&SayConsumers>,
    ),
    line: &str,
) {
    let Some(msg) = crate::map::parse_event_line(line) else {
        return;
    };
    let address = crate::map::address_of(agent_id, &msg);
    if wants_channel(access, self_bot_id, &msg, listens(listening, &address))
        && enable_channel(listening, channel_socket, &address).await
    {
        if let Some(spawn) = say_consumers {
            spawn(address);
        }
    }
}

/// The [`SayConsumers`] of a running instance: each call starts the say consumer for one more
/// address with the same transport, reactions, delivery targets, voice and activity.
pub(crate) fn say_consumers(
    client: &Arc<opencrab_gate_client::client::InstanceClient>,
    transport: &Arc<dyn crate::transport::DiscordTransport>,
    cfg: &crate::config::InstanceConfig,
    delivery_targets: &crate::ops::SessionDeliveryTargets,
    (voice, activity): (
        &Option<Arc<crate::voice::session::VoiceManager>>,
        &Option<Arc<crate::webhook::ActivityDelivery>>,
    ),
) -> SayConsumers {
    let client = client.clone();
    let transport = transport.clone();
    let agent_id = cfg.agent_id.clone();
    let reactions = cfg.system_reactions.clone();
    let delivery_targets = delivery_targets.clone();
    let voice = voice.clone();
    let activity = activity.clone();
    Arc::new(move |address: String| {
        super::spawn_say_consumer(
            client.clone(),
            address,
            transport.clone(),
            agent_id.clone(),
            reactions.clone(),
            Arc::clone(&delivery_targets),
            voice.clone(),
            activity.clone(),
        );
    })
}

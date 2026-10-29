//! D-1097: an owner @mentioning the bot in a channel it does not have yet enables that channel.

use crate::config::AccessConfig;
use crate::map::IncomingMessage;

/// Whether `msg` should make the bot ask its daemon for the message's channel.
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

/// Asks the daemon (through this instance's channel socket) for `address`, then waits for
/// core's bind to reach the client. True = the channel is bound and the message can go on.
pub(crate) async fn enable_channel(
    client: &opencrab_gate_client::client::InstanceClient,
    channel_socket: &std::path::Path,
    address: &str,
) -> bool {
    if let Err(error) = crate::channel_request::request(channel_socket, address).await {
        tracing::warn!(%address, error = %error, "owner mention: channel not enabled");
        return false;
    }
    for _ in 0..300 {
        if client.binding_for_address(address).await.is_some() {
            tracing::info!(%address, "owner mention: channel enabled");
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tracing::warn!(%address, "owner mention: core did not bind the channel in time");
    false
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
    channel_socket: Option<&std::path::Path>,
    line: &str,
) {
    let Some(channel_socket) = channel_socket else {
        return;
    };
    let Some(msg) = crate::map::parse_event_line(line) else {
        return;
    };
    let address = crate::map::address_of(agent_id, &msg);
    let bound = client.binding_for_address(&address).await.is_some();
    if wants_channel(access, self_bot_id, &msg, bound) {
        enable_channel(client, channel_socket, &address).await;
    }
}

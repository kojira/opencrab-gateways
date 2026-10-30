//! D-1097: channel requests from the daemon's children (an owner @mentioned a bot in a
//! channel it does not have yet). See [`crate::channel_request`].
//!
//! D-1099: which channels a bot listens to is gateway state only. The daemon records the address
//! in its store; core is not told anything (the first said of that channel opens its session).

use super::{DiscordDaemon, SpawnerFactory};
use crate::store::InstanceRow;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use std::sync::Arc;

impl<F> DiscordDaemon<F>
where
    F: SpawnerFactory + 'static,
{
    /// D-1097: from now on every started child gets a channel-request socket served by `binder`
    /// (normally the daemon itself, through [`DaemonChannels`]).
    pub fn serve_channel_requests(&self, binder: Arc<dyn crate::channel_request::ChannelBinder>) {
        *self.channel_binder.lock().unwrap() = Some(binder);
    }

    pub(super) fn start_channel_listener(&self, row: &InstanceRow) {
        let Some(binder) = self.channel_binder.lock().unwrap().clone() else {
            return;
        };
        let Some(path) = self.factory.channel_socket(row) else {
            return;
        };
        let task = tokio::spawn(crate::channel_request::serve(
            path,
            row.instance_id.clone(),
            binder,
        ));
        if let Some(old) = self
            .channel_tasks
            .lock()
            .unwrap()
            .insert(row.instance_id.clone(), task)
        {
            old.abort();
        }
    }

    /// Records a channel address the child started listening to, so it survives a restart.
    /// The address must be a channel of this instance's agent.
    pub fn record_address(&self, instance_id: &str, address: &str) -> Result<()> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?;
        let row = store.get(instance_id)?.context("instance unknown")?;
        anyhow::ensure!(
            crate::config::parse_address_owner(&row.agent_id, address),
            "address does not belong to this agent"
        );
        store.add_address(instance_id, address)?;
        Ok(())
    }

    pub(super) fn stop_channel_listener(&self, instance_id: &str) {
        if let Some(task) = self.channel_tasks.lock().unwrap().remove(instance_id) {
            task.abort();
        }
    }
}

/// The daemon as the channel binder of its own children (held weakly, so it never keeps the
/// daemon alive).
pub struct DaemonChannels<F>(pub std::sync::Weak<DiscordDaemon<F>>);

#[async_trait]
impl<F> crate::channel_request::ChannelBinder for DaemonChannels<F>
where
    F: SpawnerFactory + 'static,
{
    async fn record_address(&self, instance_id: &str, address: &str) -> Result<()> {
        let daemon = self.0.upgrade().context("daemon stopped")?;
        daemon.record_address(instance_id, address)
    }
}

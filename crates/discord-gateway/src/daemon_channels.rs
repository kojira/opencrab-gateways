//! D-1097: channel requests from the daemon's children (an owner @mentioned a bot in a
//! channel it does not have yet). See [`crate::channel_request`].

use super::{DiscordDaemon, GateReconciler, SpawnerFactory};
use crate::store::InstanceRow;
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use std::sync::Arc;

impl<R, F> DiscordDaemon<R, F>
where
    R: GateReconciler + 'static,
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

    /// D-1097: adds `address` to a running instance without rebuilding it. Core sends the new
    /// bind to the live child, so the child can deliver the message that asked for it.
    pub async fn bind_address(&self, instance_id: &str, address: &str) -> Result<()> {
        let row = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .get(instance_id)?
            .context("instance unknown")?;
        anyhow::ensure!(
            crate::config::parse_address_owner(&row.agent_id, address),
            "address does not belong to this agent"
        );
        if row.addresses.iter().any(|a| a == address) {
            return Ok(());
        }
        let binding_id = self.reconciler.bind(&row, address).await?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("store poisoned"))?
            .add_bound_address(instance_id, address, &binding_id)?;
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
pub struct DaemonChannels<R, F>(pub std::sync::Weak<DiscordDaemon<R, F>>);

#[async_trait]
impl<R, F> crate::channel_request::ChannelBinder for DaemonChannels<R, F>
where
    R: GateReconciler + 'static,
    F: SpawnerFactory + 'static,
{
    async fn bind_address(&self, instance_id: &str, address: &str) -> Result<()> {
        let daemon = self.0.upgrade().context("daemon stopped")?;
        daemon.bind_address(instance_id, address).await
    }
}

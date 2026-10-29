//! D-1098: the one module that tells core about this gateway's instances.
//!
//! Core keeps a generic instance (kind, agent, enabled, an opaque config) and its bindings. The
//! daemon declares them with one credential-less `provision` frame on the ordinary gate socket,
//! before the child says `hello` (core refuses `provision` while the instance is connected, so
//! the daemon stops the child first). What core gets is only the fixed neutral config
//! ([`crate::daemon::DEFAULT_CORE_CONFIG_B64`]) and the instance's channel addresses; Discord
//! settings stay in the gateway.

use crate::daemon::{core_config_b64_for_runtime_config, GateReconciler, VerifiedInstance};
use crate::store::InstanceRow;
use anyhow::Result;
use async_trait::async_trait;
use opencrab_gate_client::provision::{ProvisionClient, ProvisionDesired};

/// Provisions instances on core's gate socket.
pub struct ProvisionReconciler {
    client: ProvisionClient,
    kind_id: String,
}

impl ProvisionReconciler {
    pub fn new(core_socket: std::path::PathBuf, kind_id: impl Into<String>) -> Self {
        Self {
            client: ProvisionClient::new(core_socket),
            kind_id: kind_id.into(),
        }
    }
}

#[async_trait]
impl GateReconciler for ProvisionReconciler {
    /// Core has no read side for gateways; startup decides from the process alone and the next
    /// provision converges core.
    async fn observe(
        &self,
        _desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        Ok(opencrab_process_supervisor::lifecycle::CoreObservation::Unavailable)
    }

    async fn reconcile(&self, desired: &InstanceRow) -> Result<VerifiedInstance> {
        let config_b64 = core_config_b64_for_runtime_config(&desired.config_b64)?;
        let provisioned = self
            .client
            .provision(&ProvisionDesired {
                instance_id: &desired.instance_id,
                kind_id: &self.kind_id,
                agent_id: &desired.agent_id,
                enabled: desired.enabled,
                config_b64: &config_b64,
                addresses: &desired.addresses,
            })
            .await?;
        Ok(VerifiedInstance {
            revision: provisioned.revision,
            digest: provisioned.config_digest,
            bindings: provisioned
                .bindings
                .into_iter()
                .map(|binding| binding.binding_id)
                .collect(),
            enabled: provisioned.enabled,
        })
    }
}

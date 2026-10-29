//! D-1097: the one module that talks to core about instances and bindings (today: core's
//! gate-admin API over its Unix socket, with the daemon's credential).
//!
//! Everything else in the gateway uses only the traits it implements:
//! [`crate::daemon::GateReconciler`] (create / revise an instance, converge its bindings),
//! [`crate::subject_grant::SubjectGrantIssuer`] (the grant a new instance needs) and
//! [`crate::legacy_import::SubjectLookup`] (an existing instance's kind and subject). Replacing
//! gate-admin with another core interface means changing this module and nothing else.

use crate::daemon::{
    core_config_b64_for_runtime_config, DaemonConfig, GateReconciler, VerifiedInstance,
};
use crate::legacy_import::SubjectLookup;
use crate::store::InstanceRow;
use crate::subject_grant::{SubjectGrant, SubjectGrantIssuer};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use zeroize::Zeroizing;

/// The core-facing pieces a running daemon uses, built from its config.
pub struct CoreProvisioning {
    pub reconciler: Arc<UdsGateReconciler>,
    pub lookup: Arc<dyn SubjectLookup>,
    /// Present when the daemon may create bots (`creation_namespace` is set).
    pub grants: Option<Arc<dyn SubjectGrantIssuer>>,
}

impl CoreProvisioning {
    pub fn from_config(config: &DaemonConfig) -> Result<Self> {
        let client = opencrab_gate_client::admin::GateAdminClient::from_credential_file(
            config.gate_admin_socket.clone(),
            &config.gate_admin_credential,
        )?;
        let grants = config
            .creation_namespace
            .map(|_| {
                GateAdminGrants::from_credential_file(
                    config.gate_admin_socket.clone(),
                    &config.gate_admin_credential,
                )
                .map(|issuer| Arc::new(issuer) as Arc<dyn SubjectGrantIssuer>)
            })
            .transpose()?;
        Ok(Self {
            lookup: Arc::new(client.clone()),
            reconciler: Arc::new(UdsGateReconciler::new(client, "discord")),
            grants,
        })
    }
}

pub struct UdsGateReconciler {
    client: opencrab_gate_client::admin::GateAdminClient,
    kind_id: String,
}

impl UdsGateReconciler {
    pub fn new(
        client: opencrab_gate_client::admin::GateAdminClient,
        kind_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            kind_id: kind_id.into(),
        }
    }
}

#[async_trait]
impl GateReconciler for UdsGateReconciler {
    async fn observe(
        &self,
        desired: &InstanceRow,
    ) -> Result<opencrab_process_supervisor::lifecycle::CoreObservation> {
        use opencrab_process_supervisor::lifecycle::CoreObservation;
        let Some(observed) = self.client.get_instance(&desired.instance_id).await? else {
            return Ok(CoreObservation::Mismatch);
        };
        let mut bindings: Vec<_> = observed
            .bindings
            .into_iter()
            .map(|value| value.binding_id)
            .collect();
        bindings.sort();
        let mut expected = desired.core_bindings.clone();
        expected.sort();
        if observed.revision == desired.core_revision.unwrap_or(0)
            && observed.config_digest == desired.core_digest.as_deref().unwrap_or_default()
            && bindings == expected
        {
            Ok(if observed.enabled {
                CoreObservation::ExactEnabled
            } else {
                CoreObservation::ExactDisabled
            })
        } else {
            Ok(CoreObservation::Mismatch)
        }
    }

    async fn reconcile(
        &self,
        desired: &InstanceRow,
        subject_grant: Option<&str>,
    ) -> Result<VerifiedInstance> {
        let observed = self
            .client
            .reconcile(opencrab_gate_client::admin::ReconcileDesired {
                instance_id: &desired.instance_id,
                kind_id: &self.kind_id,
                subject_id: desired.subject_id,
                enabled: desired.enabled,
                config_b64: &core_config_b64_for_runtime_config(&desired.config_b64)?,
                subject_grant,
                addresses: &desired.addresses,
            })
            .await?;
        Ok(VerifiedInstance {
            revision: observed.revision,
            digest: observed.config_digest,
            bindings: observed
                .bindings
                .into_iter()
                .map(|binding| binding.binding_id)
                .collect(),
            enabled: observed.enabled,
        })
    }
}

/// Core's gate-admin over its Unix socket, with the daemon's gate-admin credential.
pub struct GateAdminGrants {
    socket: std::path::PathBuf,
    token: std::sync::Arc<Zeroizing<String>>,
}

impl GateAdminGrants {
    /// Reads the same credential file [`opencrab_gate_client::admin::GateAdminClient`] uses.
    pub fn from_credential_file(
        socket: std::path::PathBuf,
        credential: &std::path::Path,
    ) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(credential)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "credential must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            anyhow::ensure!(
                metadata.permissions().mode() & 0o777 == 0o600,
                "credential must be mode 0600"
            );
        }
        let token = Zeroizing::new(std::fs::read_to_string(credential)?.trim().to_string());
        anyhow::ensure!(!token.is_empty(), "credential must be nonempty");
        Ok(Self {
            socket,
            token: std::sync::Arc::new(token),
        })
    }

    pub fn new_for_test(socket: std::path::PathBuf, token: String) -> Self {
        Self {
            socket,
            token: std::sync::Arc::new(Zeroizing::new(token)),
        }
    }
}

#[async_trait::async_trait]
impl SubjectGrantIssuer for GateAdminGrants {
    async fn issue(&self, agent_id: &str) -> Result<SubjectGrant> {
        use anyhow::Context as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let body = serde_json::to_vec(&serde_json::json!({ "agent_id": agent_id }))?;
        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect gate admin {}", self.socket.display()))?;
        let head = Zeroizing::new(format!(
            "POST /api/gate-subject-grants HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.token.as_str(),
            body.len()
        ));
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&body).await?;
        let mut response = Zeroizing::new(Vec::new());
        stream.read_to_end(&mut response).await?;
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("invalid gate admin response")?;
        let status: u16 = std::str::from_utf8(&response[..split])?
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .context("missing HTTP status")?
            .parse()?;
        let value: serde_json::Value =
            serde_json::from_slice(&response[split + 4..]).unwrap_or_default();
        if !(200..300).contains(&status) {
            let code = value["code"].as_str().unwrap_or("rejected");
            anyhow::bail!("core refused the subject grant: {code} (http {status})");
        }
        let subject_id = value["subject_id"].as_i64().context("subject_id missing")?;
        anyhow::ensure!(subject_id > 0, "subject_id must be positive");
        let grant = Zeroizing::new(
            value["grant"]
                .as_str()
                .filter(|grant| !grant.is_empty())
                .context("grant missing")?
                .to_string(),
        );
        Ok(SubjectGrant { subject_id, grant })
    }
}

#[async_trait::async_trait]
impl SubjectLookup for opencrab_gate_client::admin::GateAdminClient {
    async fn core_instance(&self, instance_id: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .get_instance(instance_id)
            .await?
            .map(|observed| (observed.kind_id, observed.subject_id)))
    }
}

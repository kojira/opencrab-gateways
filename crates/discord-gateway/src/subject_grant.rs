//! D-1097: the core side of creating a new Discord instance.
//!
//! Core creates an instance only with a single-use subject grant for the agent. The daemon asks
//! core's gate-admin for one (`POST /api/gate-subject-grants`, body `{"agent_id"}`, answer
//! `{"subject_id","grant"}`); this module is the one seam for that call.

use anyhow::Result;
use zeroize::Zeroizing;

pub struct SubjectGrant {
    pub subject_id: i64,
    pub grant: Zeroizing<String>,
}

impl std::fmt::Debug for SubjectGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubjectGrant")
            .field("subject_id", &self.subject_id)
            .field("grant", &"<redacted>")
            .finish()
    }
}

#[async_trait::async_trait]
pub trait SubjectGrantIssuer: Send + Sync {
    async fn issue(&self, agent_id: &str) -> Result<SubjectGrant>;
}

/// The instance id a creation-namespace principal may create for `agent_id` (core's rule).
pub fn instance_id_for(namespace: &uuid::Uuid, agent_id: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(namespace, format!("instance\0{agent_id}").as_bytes())
}

/// Core's gate-admin over its Unix socket, with the creation-namespace principal's credential.
#[derive(Clone)]
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

#[cfg(test)]
#[path = "subject_grant_tests.rs"]
mod tests;

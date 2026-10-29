//! D-1097: what creating a new Discord instance needs from core: a subject and a single-use grant
//! for the agent ([`SubjectGrantIssuer`]). The implementation that talks to core lives in
//! [`crate::core_provisioning`].

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

pub use crate::core_provisioning::GateAdminGrants;

#[cfg(test)]
#[path = "subject_grant_tests.rs"]
mod tests;

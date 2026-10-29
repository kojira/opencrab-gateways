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

#[async_trait::async_trait]
pub trait SubjectGrantIssuer: Send + Sync {
    async fn issue(&self, agent_id: &str) -> Result<SubjectGrant>;
}

/// The instance id a creation-namespace principal may create for `agent_id` (core's rule).
pub fn instance_id_for(namespace: &uuid::Uuid, agent_id: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(namespace, format!("instance\0{agent_id}").as_bytes())
}

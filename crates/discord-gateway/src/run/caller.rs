//! The gateway-owned caller classification of a Discord author.

use crate::config::AccessConfig;
use opencrab_gate_client::wire::SaidCaller;

pub(crate) fn caller_for(access: &AccessConfig, author_id: &str) -> SaidCaller {
    if access.owners.iter().any(|id| id == author_id) {
        SaidCaller::Owner
    } else if let Some(agent_id) = access.co_agents.get(author_id) {
        SaidCaller::CoAgent {
            agent_id: agent_id.clone(),
        }
    } else if access.trusted_users.iter().any(|id| id == author_id) {
        SaidCaller::TrustedUser
    } else {
        SaidCaller::Agent
    }
}

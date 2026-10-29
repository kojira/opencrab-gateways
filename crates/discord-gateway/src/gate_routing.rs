//! D-1097: which gate-admin credential covers an instance.
//!
//! Core issues a principal for either exact instance ids or a creation namespace, never both. The
//! daemon therefore holds two credentials: the exact one covers the instances imported from the
//! instance-mode setup (random ids), the creation one covers instances it creates itself
//! (`uuid5(namespace, "instance\0" + agent_id)`). Every gate-admin call for an instance goes
//! through [`GateRouting::client_for`].

use crate::store::InstanceRow;
use opencrab_gate_client::admin::GateAdminClient;

pub struct GateRouting {
    pub exact: GateAdminClient,
    pub creation: Option<(uuid::Uuid, GateAdminClient)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    Exact,
    Creation,
}

impl GateRouting {
    /// Which principal covers `instance_id` for `agent_id`.
    pub fn principal_for(&self, instance_id: &str, agent_id: &str) -> Principal {
        match &self.creation {
            Some((namespace, _))
                if crate::subject_grant::instance_id_for(namespace, agent_id).to_string()
                    == instance_id =>
            {
                Principal::Creation
            }
            _ => Principal::Exact,
        }
    }

    /// The client whose principal covers `instance_id` for `agent_id`.
    pub fn client_for(&self, instance_id: &str, agent_id: &str) -> &GateAdminClient {
        match (self.principal_for(instance_id, agent_id), &self.creation) {
            (Principal::Creation, Some((_, client))) => client,
            _ => &self.exact,
        }
    }

    pub fn client_for_row(&self, row: &InstanceRow) -> &GateAdminClient {
        self.client_for(&row.instance_id, &row.agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(name: &str) -> GateAdminClient {
        GateAdminClient::new_for_test(format!("/tmp/{name}.sock").into(), name.into())
    }

    #[test]
    fn d1097_a_namespace_derived_id_uses_the_creation_credential_and_others_the_exact_one() {
        let ns = uuid::Uuid::parse_str("0c9a7e51-2b3d-4f6a-8e1c-5d7b9f2a4c63").unwrap();
        let routing = GateRouting {
            exact: client("exact"),
            creation: Some((ns, client("creation"))),
        };
        let derived = crate::subject_grant::instance_id_for(&ns, "agent-a").to_string();
        let legacy = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        assert_eq!(
            routing.principal_for(&derived, "agent-a"),
            Principal::Creation
        );
        assert_eq!(routing.principal_for(legacy, "agent-a"), Principal::Exact);
        // The namespace id of another agent is not this agent's: exact.
        assert_eq!(routing.principal_for(&derived, "agent-b"), Principal::Exact);
        let only_exact = GateRouting {
            exact: client("exact"),
            creation: None,
        };
        assert_eq!(
            only_exact.principal_for(&derived, "agent-a"),
            Principal::Exact
        );
    }
}

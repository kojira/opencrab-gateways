//! operator と gateway が共有する配置ファイル。Bearer は載せない。

use std::net::SocketAddr;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    pub http_bind: String,
    pub core_socket: String,
    /// D-1099: where this gateway keeps its conversations (session key → instance). Absent = in
    /// memory, so conversations do not survive a restart.
    #[serde(default)]
    pub conversations_path: Option<std::path::PathBuf>,
    /// D-1099: conversations this gateway already owns (session key → instance), merged into its
    /// registry on start. Used to hand over conversations created before the registry existed.
    #[serde(default)]
    pub conversations: Vec<ConversationSeed>,
    pub instances: Vec<InstancePlacement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationSeed {
    pub session: String,
    pub instance_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub agent_id: String,
    pub author_id: String,
}

impl Placement {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let place: Placement = serde_json::from_str(&text)?;
        place.validate()?;
        Ok(place)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let addr: SocketAddr = self
            .http_bind
            .parse()
            .map_err(|_| anyhow::anyhow!("http_bind must be a socket address"))?;
        if !addr.ip().is_loopback() {
            anyhow::bail!("http_bind must be loopback");
        }
        if self.core_socket.is_empty() || !self.core_socket.starts_with('/') {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if self.instances.is_empty() {
            anyhow::bail!("instances must be nonempty");
        }
        if let Some(path) = &self.conversations_path {
            if !path.is_absolute() {
                anyhow::bail!("conversations_path must be an absolute path");
            }
        }
        for seed in &self.conversations {
            if seed.session.is_empty() {
                anyhow::bail!("conversation session must be nonempty");
            }
            if !self
                .instances
                .iter()
                .any(|i| i.instance_id == seed.instance_id)
            {
                anyhow::bail!("conversation instance_id is not an instance of this placement");
            }
        }
        let mut seen_instances = std::collections::BTreeSet::new();
        let mut seen_agents = std::collections::BTreeSet::new();
        for inst in &self.instances {
            if inst.agent_id.is_empty() {
                anyhow::bail!("agent_id must be nonempty");
            }
            if inst.author_id.is_empty() {
                anyhow::bail!("author_id must be nonempty");
            }
            crate::v3::wire::parse_uuid(&inst.instance_id)
                .map_err(|_| anyhow::anyhow!("instance_id must be canonical lowercase UUID"))?;
            if !seen_instances.insert(inst.instance_id.clone()) {
                anyhow::bail!("duplicate instance_id is a double live; refuse startup");
            }
            if !seen_agents.insert(inst.agent_id.clone()) {
                anyhow::bail!("duplicate agent_id is ambiguous; refuse startup");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback() {
        let p = Placement {
            http_bind: "0.0.0.0:80".into(),
            core_socket: "/tmp/g.sock".into(),
            conversations_path: None,
            conversations: Vec::new(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                agent_id: "agent-a".into(),
                author_id: "owner".into(),
            }],
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn accepts_loopback() {
        let p = Placement {
            http_bind: "127.0.0.1:18700".into(),
            core_socket: "/tmp/g.sock".into(),
            conversations_path: None,
            conversations: Vec::new(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                agent_id: "agent-a".into(),
                author_id: "owner".into(),
            }],
        };
        p.validate().unwrap();
    }
}

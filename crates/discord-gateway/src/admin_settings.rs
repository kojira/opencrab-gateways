//! D-1097 D1: settings operations behind the Discord admin socket. Each change is written as a
//! new desired generation, so the daemon's reconcile rebuilds only that instance's child.

use crate::config::{decode_config_b64, parse_address_owner, parse_instance_config};
use crate::store::{DiscordStore, InstanceRow};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde_json::{json, Value};

/// Every instance in `scope`, without any secret.
pub fn list(store: &DiscordStore, scope: &std::collections::BTreeSet<String>) -> Result<Value> {
    let mut instances = Vec::new();
    for row in store.list()? {
        if !scope.contains(&row.instance_id) {
            continue;
        }
        let config = parse_instance_config(&decode_config_b64(&row.config_b64)?)?;
        instances.push(json!({
            "instance_id": row.instance_id,
            "agent_id": row.agent_id,
            "enabled": row.enabled,
            "lifecycle_state": row.lifecycle_state,
            "last_exit": store.last_exit(&row.instance_id)?,
            "credential_configured": !row.credential_envelope.is_empty(),
            "addresses": row.addresses,
            "config": config,
        }));
    }
    Ok(json!({ "instances": instances }))
}

pub fn set_config(
    store: &DiscordStore,
    instance_id: &str,
    config: &Value,
    key: &[u8; 32],
) -> Result<Value> {
    let row = existing(store, instance_id)?;
    let bytes = serde_json::to_vec(config)?;
    let parsed = parse_instance_config(&bytes)?;
    anyhow::ensure!(parsed.agent_id == row.agent_id, "agent_id cannot change");
    let canonical = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&parsed)?);
    write(
        store,
        &row,
        &canonical,
        &row.addresses,
        "",
        row.enabled,
        key,
    )
}

pub fn set_addresses(
    store: &DiscordStore,
    instance_id: &str,
    addresses: &[String],
    key: &[u8; 32],
) -> Result<Value> {
    let row = existing(store, instance_id)?;
    anyhow::ensure!(!addresses.is_empty(), "addresses must be nonempty");
    for address in addresses {
        anyhow::ensure!(
            parse_address_owner(&row.agent_id, address),
            "address does not belong to this agent: {address}"
        );
    }
    let mut unique = addresses.to_vec();
    unique.sort();
    unique.dedup();
    write(store, &row, &row.config_b64, &unique, "", row.enabled, key)
}

/// Replaces the bot token. `self_bot_id`, when given, is the bot the token belongs to and
/// replaces the config's `self_bot_id` in the same write.
pub fn set_credential(
    store: &DiscordStore,
    instance_id: &str,
    credential: &str,
    self_bot_id: Option<&str>,
    key: &[u8; 32],
) -> Result<Value> {
    let row = existing(store, instance_id)?;
    anyhow::ensure!(!credential.trim().is_empty(), "credential must be nonempty");
    let config_b64 = match self_bot_id {
        Some(id) => {
            let mut config = parse_instance_config(&decode_config_b64(&row.config_b64)?)?;
            config.self_bot_id = id.to_string();
            let bytes = serde_json::to_vec(&config)?;
            parse_instance_config(&bytes)?;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
        None => row.config_b64.clone(),
    };
    write(
        store,
        &row,
        &config_b64,
        &row.addresses,
        credential.trim(),
        row.enabled,
        key,
    )
}

pub fn set_enabled(
    store: &DiscordStore,
    instance_id: &str,
    enabled: bool,
    key: &[u8; 32],
) -> Result<Value> {
    let row = existing(store, instance_id)?;
    write(
        store,
        &row,
        &row.config_b64,
        &row.addresses,
        "",
        enabled,
        key,
    )
}

fn existing(store: &DiscordStore, instance_id: &str) -> Result<InstanceRow> {
    store.get(instance_id)?.context("instance_unknown")
}

/// `credential` empty = keep the stored one.
fn write(
    store: &DiscordStore,
    row: &InstanceRow,
    config_b64: &str,
    addresses: &[String],
    credential: &str,
    enabled: bool,
    key: &[u8; 32],
) -> Result<Value> {
    let generation = store.upsert_desired(
        &row.instance_id,
        &row.agent_id,
        row.subject_id,
        config_b64,
        addresses,
        credential,
        None,
        enabled,
        key,
    )?;
    Ok(json!({"desired_generation": generation, "state": "pending"}))
}

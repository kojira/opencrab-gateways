//! D-1097 N1/N3: settings operations on the protected admin socket.
//!
//! | op | input | effect |
//! |---|---|---|
//! | `list` | – | scoped instances (id, agent, enabled, state, credential_configured) |
//! | `get` | instance | adds `settings` (relays, name, filter, self_pubkey) and `access` |
//! | `set_config` | `settings{relays,name,filter}` | replaces runtime config; self_pubkey kept |
//! | `set_access` | `settings{owner,trusted_users,followees,co_agents}` | replaces access |
//! | `set_key` | `credential` (nsec or hex) | replaces the key and self_pubkey |
//! | `set_enabled` | `enabled` | starts or stops the instance |
//! | `generate_key` | `prefix`, `overwrite=true` | nostaro vanity → key; instance disabled |
//!
//! Each edit bumps the desired generation, so the daemon rebuilds the child within one
//! reconcile tick. Keys are write-only: no response carries a secret.

use crate::admin::Request;
use crate::config::{decode_config_b64, AccessConfig, WatchFilter};
use crate::follow_targets::canonical_hex;
use crate::secret_store::MASTER_KEY_LEN;
use crate::store::{InstanceRow, NostrStore};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::Path;

/// The longest a vanity search may run (same ceiling as before the gateway separation).
const VANITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
const BECH32_CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";

/// One vanity search at a time: each holds a nostaro process for up to [`VANITY_TIMEOUT`].
static VANITY_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(crate) fn summary(row: &InstanceRow) -> Value {
    json!({
        "instance_id": row.instance_id,
        "agent_id": row.agent_id,
        "subject_id": row.subject_id,
        "enabled": row.enabled,
        "desired_generation": row.desired_generation,
        "lifecycle_state": row.lifecycle_state,
        "credential_configured": !row.credential_envelope.is_empty(),
    })
}

pub(crate) fn list(
    store: &NostrStore,
    scope: &std::collections::BTreeSet<String>,
) -> Result<Value> {
    let instances: Vec<Value> = store
        .list()?
        .iter()
        .filter(|row| scope.contains(&row.instance_id))
        .map(summary)
        .collect();
    Ok(json!({ "instances": instances }))
}

pub(crate) fn get(row: &InstanceRow) -> Result<Value> {
    let config = stored_config(row)?;
    let pick = |key: &str| config.get(key).cloned().unwrap_or(Value::Null);
    let mut value = summary(row);
    value["settings"] = json!({
        "relays": pick("relays"),
        "name": pick("name"),
        "self_pubkey": pick("self_pubkey"),
        "filter": config.get("filter").cloned().unwrap_or_else(|| json!({})),
    });
    value["access"] = serde_json::to_value(&row.access)?;
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigEdit {
    relays: Vec<String>,
    name: String,
    #[serde(default)]
    filter: WatchFilter,
}

pub(crate) fn set_config(
    store: &NostrStore,
    row: &InstanceRow,
    request: &Request,
) -> Result<Value> {
    let edit: ConfigEdit =
        serde_json::from_value(request.settings.clone().context("settings required")?)
            .context("settings must be {relays, name, filter}")?;
    let relays: Vec<String> = edit
        .relays
        .iter()
        .map(|relay| relay.trim().to_string())
        .filter(|relay| !relay.is_empty())
        .collect();
    for relay in &relays {
        let url = url::Url::parse(relay).with_context(|| format!("relay is not a URL: {relay}"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "wss" | "ws"),
            "relay must be a ws:// or wss:// URL: {relay}"
        );
    }
    let mut config = stored_config(row)?;
    config.insert("relays".into(), json!(relays));
    config.insert("name".into(), json!(edit.name.trim()));
    config.insert("filter".into(), serde_json::to_value(&edit.filter)?);
    let encoded = encode(&config)?;
    // Validate exactly as the daemon will run it: stored config plus the gateway's access.
    crate::config::canonicalize_config_b64_with_gateway_access(&encoded, row.access.clone())?;
    let generation = store.update_config(&row.instance_id, &encoded)?;
    Ok(json!({ "desired_generation": generation, "state": "pending" }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessEdit {
    #[serde(default)]
    owner: Vec<String>,
    #[serde(default)]
    trusted_users: Vec<String>,
    #[serde(default)]
    followees: Vec<String>,
    #[serde(default)]
    co_agents: std::collections::BTreeMap<String, String>,
}

pub(crate) fn set_access(
    store: &NostrStore,
    row: &InstanceRow,
    request: &Request,
) -> Result<Value> {
    let edit: AccessEdit =
        serde_json::from_value(request.settings.clone().context("settings required")?)
            .context("settings must be {owner, trusted_users, followees, co_agents}")?;
    let keys = |field: &str, values: &[String]| -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
            let hex = canonical_hex(value)
                .with_context(|| format!("{field}: not a Nostr public key: {value}"))?;
            if !out.contains(&hex) {
                out.push(hex);
            }
        }
        Ok(out)
    };
    let mut access = AccessConfig {
        owner: keys("owner", &edit.owner)?,
        trusted_users: keys("trusted_users", &edit.trusted_users)?,
        followees: keys("followees", &edit.followees)?,
        co_agents: Default::default(),
    };
    for (pubkey, agent_id) in &edit.co_agents {
        let hex = canonical_hex(pubkey)
            .with_context(|| format!("co_agents: not a Nostr public key: {pubkey}"))?;
        let agent_id = agent_id.trim();
        anyhow::ensure!(
            !agent_id.is_empty(),
            "co_agents: agent id is required for {pubkey}"
        );
        access.co_agents.insert(hex, agent_id.to_string());
    }
    anyhow::ensure!(
        !access.is_empty(),
        "access must contain at least one owner, trusted_user, followee, or co_agent"
    );
    let generation = store.set_access(&row.instance_id, &access)?;
    Ok(json!({ "desired_generation": generation, "state": "pending" }))
}

pub(crate) fn set_key(
    store: &NostrStore,
    row: &InstanceRow,
    request: &Request,
    master_key: &[u8; MASTER_KEY_LEN],
) -> Result<Value> {
    let secret = request.credential.as_deref().unwrap_or_default().trim();
    store_identity(store, row, secret, None, master_key)
}

pub(crate) fn set_enabled(
    store: &NostrStore,
    row: &InstanceRow,
    request: &Request,
) -> Result<Value> {
    let enabled = request.enabled.context("enabled required")?;
    let generation = store.set_enabled(&row.instance_id, enabled)?;
    Ok(json!({ "desired_generation": generation, "state": "pending", "enabled": enabled }))
}

/// Validate the prefix and check the overwrite guard before any process is spawned.
pub(crate) fn check_generate(request: &Request) -> Result<String> {
    let prefix = request
        .prefix
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if let Some(bad) = prefix.chars().find(|c| !BECH32_CHARSET.contains(*c)) {
        anyhow::bail!("vanity prefix has a character outside bech32: '{bad}'");
    }
    anyhow::ensure!(
        request.overwrite == Some(true),
        "a key is already set; pass overwrite=true to replace it"
    );
    Ok(prefix)
}

/// Run `nostaro vanity --json [--prefix=…]` and return the generated nsec.
pub(crate) async fn vanity(nostaro_bin: &Path, prefix: &str) -> Result<zeroize::Zeroizing<String>> {
    let _permit = VANITY_GATE.acquire().await.context("vanity gate closed")?;
    let mut command = tokio::process::Command::new(nostaro_bin);
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .arg("vanity")
        .arg("--json");
    if !prefix.is_empty() {
        command.arg(format!("--prefix={prefix}"));
    }
    let output = tokio::time::timeout(VANITY_TIMEOUT, command.output())
        .await
        .context("nostaro vanity timed out")?
        .context("nostaro vanity could not start")?;
    anyhow::ensure!(output.status.success(), "nostaro vanity failed");
    // stdout holds the secret: parse it without ever putting it in an error or a log line.
    let stdout = zeroize::Zeroizing::new(String::from_utf8(output.stdout).unwrap_or_default());
    let line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .context("nostaro vanity printed no JSON result")?;
    let value: Value = serde_json::from_str(line)
        .map_err(|_| anyhow::anyhow!("nostaro vanity JSON unreadable"))?;
    let nsec = value
        .get("nsec")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|nsec| !nsec.is_empty())
        .context("nostaro vanity returned no key")?;
    Ok(zeroize::Zeroizing::new(nsec.to_string()))
}

/// Store a key with its derived public key. `enabled` is set when given.
pub(crate) fn store_identity(
    store: &NostrStore,
    row: &InstanceRow,
    secret: &str,
    enabled: Option<bool>,
    master_key: &[u8; MASTER_KEY_LEN],
) -> Result<Value> {
    let keys = nostr::Keys::parse(secret)
        .map_err(|_| anyhow::anyhow!("credential is not a Nostr secret key (nsec or hex)"))?;
    let public = keys.public_key();
    let mut config = stored_config(row)?;
    config.insert("self_pubkey".into(), json!(public.to_hex()));
    let generation = store.set_identity(
        &row.instance_id,
        secret,
        &encode(&config)?,
        enabled,
        master_key,
    )?;
    use nostr::nips::nip19::ToBech32 as _;
    Ok(json!({
        "desired_generation": generation,
        "state": "pending",
        "self_pubkey": public.to_hex(),
        "npub": public.to_bech32().unwrap_or_default(),
    }))
}

fn stored_config(row: &InstanceRow) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_slice(&decode_config_b64(&row.config_b64)?)
        .context("stored config is not JSON")?;
    match value {
        Value::Object(map) => Ok(map),
        _ => anyhow::bail!("stored config is not a JSON object"),
    }
}

fn encode(config: &Map<String, Value>) -> Result<String> {
    Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(config)?))
}

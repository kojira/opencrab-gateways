//! Protected Discord-local administration over a mode-0600 Unix socket.

use crate::store::DiscordStore;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    op: String,
    /// Absent only for `list` and `create`.
    #[serde(default)]
    instance_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    config_b64: Option<String>,
    #[serde(default)]
    addresses: Option<Vec<String>>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    /// D-1097: the whole Discord instance config as JSON (`set_config`).
    #[serde(default)]
    config: Option<Value>,
    /// D-1097: the bot id the new credential belongs to (`set_credential`), as checked against
    /// Discord by the caller. Stored together with the token so they never disagree.
    #[serde(default)]
    self_bot_id: Option<String>,
    /// D-1097: the owners of a new bot (`create`).
    #[serde(default)]
    owners: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn spawn(
    socket: PathBuf,
    store: Arc<Mutex<DiscordStore>>,
    master_key: Arc<[u8; 32]>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move { serve(&socket, store, master_key).await })
}

/// Every instance in this gateway's store is this gateway's own; the socket (mode 0600) is the
/// boundary, so there is no further per-instance scope.
pub async fn serve(
    socket: &Path,
    store: Arc<Mutex<DiscordStore>>,
    master_key: Arc<[u8; 32]>,
) -> Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    set_mode(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let store = store.clone();
        let key = master_key.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, store, key).await {
                tracing::warn!(error = %error, "discord local admin request failed");
            }
        });
    }
}

async fn handle(
    stream: UnixStream,
    store: Arc<Mutex<DiscordStore>>,
    key: Arc<[u8; 32]>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => apply(request, &store, &key),
            Err(_) => Response {
                id: String::new(),
                ok: false,
                result: None,
                error: Some("bad_request".into()),
            },
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
    }
    Ok(())
}

/// `create`: a bot for an agent that has none, under a fresh instance id. The daemon's next
/// reconcile provisions it in core and starts the bot.
fn create(request: &Request, store: &DiscordStore, key: &[u8; 32]) -> Result<Value> {
    let agent_id = request.agent_id.as_deref().context("agent_id required")?;
    let plan = crate::admin_settings::plan_create(
        agent_id,
        request
            .self_bot_id
            .as_deref()
            .context("self_bot_id required")?,
        request.owners.as_deref().unwrap_or_default(),
        request.addresses.as_deref().unwrap_or_default(),
        request.credential.as_deref().unwrap_or_default(),
    )?;
    anyhow::ensure!(
        store.list()?.iter().all(|row| row.agent_id != agent_id),
        "this agent already has a bot"
    );
    let instance_id = uuid::Uuid::new_v4().to_string();
    crate::admin_settings::store_create(store, &instance_id, &plan, key)
}

fn respond(id: String, result: Result<Value>) -> Response {
    match result {
        Ok(value) => Response {
            id,
            ok: true,
            result: Some(value),
            error: None,
        },
        Err(error) => Response {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
        },
    }
}

fn apply(request: Request, store: &Mutex<DiscordStore>, key: &[u8; 32]) -> Response {
    let id = request.id.clone();
    let result = (|| -> Result<Value> {
        let store = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
        match request.op.as_str() {
            "list" => return crate::admin_settings::list(&store, |_| Ok(true)),
            "create" => return create(&request, &store, key),
            _ => {}
        }
        match request.op.as_str() {
            "get" => Ok(store.get(&request.instance_id)?.map_or(Value::Null, |row| {
                json!({
                    "instance_id": row.instance_id,
                    "agent_id": row.agent_id,
                    "enabled": row.enabled,
                    "desired_generation": row.desired_generation,
                    "lifecycle_state": row.lifecycle_state,
                    "credential_configured": !row.credential_envelope.is_empty(),
                })
            })),
            "upsert" => {
                let canonical_config = crate::config::canonicalize_config_b64(
                    request
                        .config_b64
                        .as_deref()
                        .context("config_b64 required")?,
                )?;
                let generation = store.upsert_desired(
                    &request.instance_id,
                    request.agent_id.as_deref().context("agent_id required")?,
                    &canonical_config,
                    request.addresses.as_deref().unwrap_or_default(),
                    request.credential.as_deref().unwrap_or_default(),
                    request.enabled.context("enabled required")?,
                    key,
                )?;
                Ok(json!({"desired_generation": generation, "state": "pending"}))
            }
            "set_config" => crate::admin_settings::set_config(
                &store,
                &request.instance_id,
                request.config.as_ref().context("config required")?,
                key,
            ),
            "set_addresses" => crate::admin_settings::set_addresses(
                &store,
                &request.instance_id,
                request.addresses.as_deref().context("addresses required")?,
                key,
            ),
            "set_credential" => crate::admin_settings::set_credential(
                &store,
                &request.instance_id,
                request.credential.as_deref().unwrap_or_default(),
                request.self_bot_id.as_deref(),
                key,
            ),
            "set_enabled" => crate::admin_settings::set_enabled(
                &store,
                &request.instance_id,
                request.enabled.context("enabled required")?,
                key,
            ),
            _ => anyhow::bail!("unknown_operation"),
        }
    })();
    respond(id, result)
}

fn prepare_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => anyhow::bail!("admin socket path conflict"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
    anyhow::ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_socket(),
        "not a socket"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn s5_discord_admin_is_mode_0600_and_redacts_secret() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("admin.sock");
        let store = Arc::new(Mutex::new(
            DiscordStore::open(&temp.path().join("owner.db")).unwrap(),
        ));
        let inspect = store.clone();
        let task = spawn(socket.clone(), store, Arc::new([4; 32]));
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let secret_config = base64::engine::general_purpose::STANDARD.encode(
            br#"{"agent_id":"a","self_bot_id":"123456789012345678","token":"config-leak"}"#,
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"secret-config","op":"upsert","instance_id":instance_id,
            "agent_id":"a","config_b64":secret_config,"addresses":[],"credential":"super-secret",
            "enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":false"));
        assert!(inspect.lock().unwrap().get(instance_id).unwrap().is_none());

        let config = base64::engine::general_purpose::STANDARD
            .encode(br#"{"agent_id":"a","self_bot_id":"123456789012345678"}"#);
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"2","op":"upsert","instance_id":instance_id,"agent_id":"a",
            "config_b64":config,"addresses":[],"credential":"super-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":true"));
        assert!(!line.contains("super-secret"));
        let row = inspect.lock().unwrap().get(instance_id).unwrap().unwrap();
        assert!(!row.credential_envelope.contains("super-secret"));
        assert_eq!(
            &*inspect
                .lock()
                .unwrap()
                .decrypt_credential(instance_id, &[4; 32])
                .unwrap(),
            b"super-secret"
        );
        task.abort();
    }
}

//! Protected Nostr-local administration over a mode-0600 Unix socket.

use crate::admin_settings as settings;
use crate::store::NostrStore;
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
pub(crate) struct Request {
    pub(crate) id: String,
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) instance_id: String,
    #[serde(default)]
    pub(crate) agent_id: Option<String>,
    #[serde(default)]
    pub(crate) config_b64: Option<String>,
    #[serde(default)]
    pub(crate) addresses: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) credential: Option<String>,
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    /// D-1097: settings payloads (`set_config` / `set_access`).
    #[serde(default)]
    pub(crate) settings: Option<Value>,
    /// D-1097 N3: vanity prefix for `generate_key`.
    #[serde(default)]
    pub(crate) prefix: Option<String>,
    #[serde(default)]
    pub(crate) overwrite: Option<bool>,
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

/// Everything an admin request may touch. Every instance in the store is this gateway's own;
/// the socket (mode 0600) is the boundary.
pub struct AdminContext {
    pub store: Arc<Mutex<NostrStore>>,
    pub master_key: Arc<[u8; 32]>,
    /// nostaro binary for `generate_key` (D-1097 N3).
    pub nostaro_bin: PathBuf,
}

pub fn spawn(socket: PathBuf, context: AdminContext) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move { serve(&socket, Arc::new(context)).await })
}

pub async fn serve(socket: &Path, context: Arc<AdminContext>) -> Result<()> {
    prepare_socket(socket)?;
    let listener = UnixListener::bind(socket)?;
    set_mode(socket)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let context = context.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, context).await {
                tracing::warn!(error = %error, "nostr local admin request failed");
            }
        });
    }
}

async fn handle(stream: UnixStream, context: Arc<AdminContext>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.op == "generate_key" => generate_key(request, &context).await,
            Ok(request) => apply(request, &context.store, &context.master_key),
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

fn apply(request: Request, store: &Mutex<NostrStore>, key: &[u8; 32]) -> Response {
    let id = request.id.clone();
    let result = (|| -> Result<Value> {
        if request.op == "list" {
            let store = store
                .lock()
                .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
            return settings::list(&store);
        }
        let store = store
            .lock()
            .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
        let existing = || -> Result<crate::store::InstanceRow> {
            store.get(&request.instance_id)?.context("instance unknown")
        };
        match request.op.as_str() {
            "get" => match store.get(&request.instance_id)? {
                Some(row) => settings::get(&row),
                None => Ok(Value::Null),
            },
            "set_config" => settings::set_config(&store, &existing()?, &request),
            "set_access" => settings::set_access(&store, &existing()?, &request),
            "set_key" => settings::set_key(&store, &existing()?, &request, key),
            "set_enabled" => settings::set_enabled(&store, &existing()?, &request),
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
            _ => anyhow::bail!("unknown_operation"),
        }
    })();
    respond(id, result)
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

/// `generate_key` runs nostaro, which may take long, so the store lock is held only around
/// the reads and the final write, never across the search.
async fn generate_key(request: Request, context: &AdminContext) -> Response {
    let id = request.id.clone();
    let result = async {
        let prefix = settings::check_generate(&request)?;
        {
            let store = context
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
            store
                .get(&request.instance_id)?
                .context("instance unknown")?;
        }
        let nsec = settings::vanity(&context.nostaro_bin, &prefix).await?;
        let store = context
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("store_unavailable"))?;
        let row = store
            .get(&request.instance_id)?
            .context("instance unknown")?;
        // A new identity starts disabled: the operator checks the settings, then enables it.
        settings::store_identity(&store, &row, &nsec, Some(false), &context.master_key)
    }
    .await;
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
#[path = "admin_settings_tests.rs"]
mod settings_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn s5_nostr_admin_is_mode_0600_and_redacts_secret() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("admin.sock");
        let store = Arc::new(Mutex::new(
            NostrStore::open(&temp.path().join("owner.db")).unwrap(),
        ));
        let inspect = store.clone();
        let task = spawn(
            socket.clone(),
            AdminContext {
                store,
                master_key: Arc::new([4; 32]),
                nostaro_bin: PathBuf::from("/nonexistent-nostaro"),
            },
        );
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let instance_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let secret_config = base64::engine::general_purpose::STANDARD.encode(
            br#"{"relays":["wss://relay.example"],"self_pubkey":"0000000000000000000000000000000000000000000000000000000000000000","name":"bot","secret":"config-leak"}"#,
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let request = json!({"id":"secret-config","op":"upsert","instance_id":instance_id,"agent_id":"a",
            "config_b64":secret_config,"addresses":[],"credential":"super-secret","enabled":true});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":false"));
        assert!(inspect.lock().unwrap().get(instance_id).unwrap().is_none());

        let config = base64::engine::general_purpose::STANDARD.encode(
            br#"{"relays":["wss://relay.example"],"self_pubkey":"0000000000000000000000000000000000000000000000000000000000000000","name":"bot","access":{"owner":["1111111111111111111111111111111111111111111111111111111111111111"]}}"#,
        );
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

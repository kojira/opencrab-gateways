//! D-1097: the one thing a bot child may tell its daemon — it started listening to one more
//! channel address of its own instance (an owner @mentioned the bot there), so the daemon
//! records it across restarts. Core is not involved (D-1099).
//!
//! The daemon creates one Unix socket per instance (mode 0600) and gives its path only to that
//! child. A request names an address; the instance is fixed by which socket it came in on, so
//! a child can never touch another instance or any other setting.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// What the daemon does for a request on an instance's socket.
#[async_trait::async_trait]
pub trait ChannelBinder: Send + Sync {
    async fn record_address(&self, instance_id: &str, address: &str) -> Result<()>;
}

/// The socket path for `instance_id` under `dir`. Kept short: a Unix socket path must fit in
/// `sun_path` (104 bytes on macOS), and `dir` is already a per-release directory.
pub fn socket_path(dir: &Path, instance_id: &str) -> PathBuf {
    let short: String = instance_id.chars().filter(|c| *c != '-').take(12).collect();
    dir.join(format!("{short}.ch"))
}

/// Serves `instance_id`'s socket at `path`. Each request is one JSON line
/// `{"address": ...}`,
/// answered with one line `{"ok": bool, "error"?: ...}`.
pub async fn serve(
    path: PathBuf,
    instance_id: String,
    binder: std::sync::Arc<dyn ChannelBinder>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let instance_id = instance_id.clone();
        let binder = binder.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, &instance_id, binder.as_ref()).await {
                tracing::warn!(error = %error, "channel request failed");
            }
        });
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    address: String,
}

async fn answer(
    stream: tokio::net::UnixStream,
    instance_id: &str,
    binder: &dyn ChannelBinder,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::io::BufReader::new(read).read_line(&mut line).await?;
    let result = match serde_json::from_str::<Request>(&line) {
        Ok(request) => binder.record_address(instance_id, &request.address).await,
        Err(_) => Err(anyhow::anyhow!("bad_request")),
    };
    let reply = match result {
        Ok(()) => serde_json::json!({"ok": true}),
        Err(error) => serde_json::json!({"ok": false, "error": error.to_string()}),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// The child's side: tells the daemon it listens to `address` now.
pub async fn request(path: &Path, address: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let call = async {
        let mut stream = tokio::net::UnixStream::connect(path).await?;
        let line = serde_json::json!({ "address": address });
        stream.write_all(format!("{line}\n").as_bytes()).await?;
        let mut reply = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut reply)
            .await?;
        Ok::<serde_json::Value, anyhow::Error>(serde_json::from_str(&reply)?)
    };
    let reply = tokio::time::timeout(std::time::Duration::from_secs(30), call)
        .await
        .map_err(|_| anyhow::anyhow!("the daemon did not answer the channel request"))??;
    anyhow::ensure!(
        reply["ok"] == true,
        "the daemon refused the channel: {}",
        reply["error"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

#[cfg(test)]
#[path = "channel_request_tests.rs"]
mod tests;

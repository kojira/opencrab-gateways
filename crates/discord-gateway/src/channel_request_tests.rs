use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder(Mutex<Vec<(String, String)>>);

#[async_trait::async_trait]
impl ChannelBinder for Recorder {
    async fn record_address(&self, instance_id: &str, address: &str) -> Result<()> {
        anyhow::ensure!(address.starts_with("discord-agent-"), "not this agent's");
        self.0
            .lock()
            .unwrap()
            .push((instance_id.to_string(), address.to_string()));
        Ok(())
    }
}

#[tokio::test]
async fn d1097_a_child_records_an_address_of_its_own_instance_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let path = socket_path(temp.path(), "instance-a");
    let binder = Arc::new(Recorder::default());
    let server = tokio::spawn(serve(path.clone(), "instance-a".into(), binder.clone()));
    for _ in 0..200 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(path.exists(), "the daemon creates the socket");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    request(&path, "discord-agent-1-3").await.unwrap();
    assert!(request(&path, "discord-other-1-3").await.is_err());
    assert_eq!(
        binder.0.lock().unwrap().clone(),
        vec![("instance-a".to_string(), "discord-agent-1-3".to_string())],
        "the instance comes from the socket, not from the request"
    );

    // Anything else on the line is refused; there is no other operation.
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"{\"address\":\"discord-agent-1-4\",\"instance_id\":\"instance-b\"}\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(stream)
        .read_line(&mut line)
        .await
        .unwrap();
    assert!(line.contains("\"ok\":false"), "{line}");
    assert_eq!(binder.0.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn d1097_a_missing_daemon_socket_is_an_error_not_a_hang() {
    let temp = tempfile::tempdir().unwrap();
    let path = socket_path(temp.path(), "nobody");
    assert!(request(&path, "discord-agent-1-3").await.is_err());
}

/// The production path fits a Unix socket address (`sun_path`, 104 bytes on macOS).
#[test]
fn d1097_the_production_socket_path_fits_a_unix_socket_address() {
    let dir = std::path::Path::new("/tmp/opencrab-prod-discord-0123456-0123456");
    let path = socket_path(dir, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    assert!(path.as_os_str().len() < 104, "{}", path.display());
    assert_ne!(
        socket_path(dir, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        socket_path(dir, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
    );
}

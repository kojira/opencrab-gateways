use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// One-request fake gate-admin: records the request line, headers and body, answers `reply`.
async fn fake(
    socket: std::path::PathBuf,
    status: u16,
    reply: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) {
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0_u8; 2048];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..split]).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while raw.len() < split + 4 + length {
                    let n = stream.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..n]);
                }
                break;
            }
        }
        seen.lock()
            .unwrap()
            .push(String::from_utf8_lossy(&raw).to_string());
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}

#[tokio::test]
async fn d1097_issuer_posts_the_agent_and_reads_subject_and_grant() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("gate-admin.sock");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(fake(
        socket.clone(),
        201,
        r#"{"subject_id":11,"grant":"grant-secret"}"#,
        seen.clone(),
    ));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let issuer = GateAdminGrants::new_for_test(socket, "admin-token".into());
    let grant = issuer.issue("agent-new").await.unwrap();
    assert_eq!(grant.subject_id, 11);
    assert_eq!(grant.grant.as_str(), "grant-secret");
    let request = seen.lock().unwrap()[0].clone();
    assert!(
        request.starts_with("POST /api/gate-subject-grants HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(request.contains("Authorization: Bearer admin-token\r\n"));
    let body = &request[request.find("\r\n\r\n").unwrap() + 4..];
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!({"agent_id":"agent-new"})
    );
    server.abort();
}

#[tokio::test]
async fn d1097_issuer_reports_a_refusal_without_the_secret() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("gate-admin.sock");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(fake(
        socket.clone(),
        403,
        r#"{"code":"unauthorized"}"#,
        seen,
    ));
    while !socket.exists() {
        tokio::task::yield_now().await;
    }
    let issuer = GateAdminGrants::new_for_test(socket, "admin-token".into());
    let error = issuer.issue("agent-new").await.unwrap_err().to_string();
    assert!(error.contains("unauthorized"), "{error}");
    assert!(!error.contains("admin-token"));
    server.abort();
}

#[test]
fn d1097_issuer_requires_a_0600_credential_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cred");
    std::fs::write(&path, "admin-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(GateAdminGrants::from_credential_file("/tmp/x.sock".into(), &path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    GateAdminGrants::from_credential_file("/tmp/x.sock".into(), &path).unwrap();
}

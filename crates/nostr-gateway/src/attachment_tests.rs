use super::*;
use base64::Engine as _;
use nostr::{JsonUtil, Keys};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Fixture {
    dir: tempfile::TempDir,
    handler: NostrInvokeHandler,
    files: Value,
    keys: Keys,
}

impl Fixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("publisher");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'; printf 'post\\n' >> '{}'; exit 0\n",
                dir.path().join("argv").display(),
                dir.path().join("effects").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let keys = Keys::generate();
        let files: Vec<Value> = [b"first".as_slice(), b"second".as_slice()].into_iter().enumerate().map(|(i, bytes)| {
            let path = format!("copy-{i}");
            std::fs::write(dir.path().join(&path), bytes).unwrap();
            json!({"id":format!("file-{i}"), "name":format!("file-{i}.txt"), "size":bytes.len(),
                "sha256":format!("{:x}", Sha256::digest(bytes)), "media_type":"text/plain", "local_path":path})
        }).collect();
        let handler = NostrInvokeHandler::new(
            script,
            dir.path().join("relays.toml"),
            Some(keys.secret_key().to_secret_hex()),
            false,
        )
        .with_attachments(Some(dir.path().to_owned()), keys.public_key().to_hex());
        Self {
            dir,
            handler,
            files: json!(files),
            keys,
        }
    }

    fn payload(&self) -> Value {
        json!({"text":"hello", "files":self.files})
    }
    fn posted(&self) -> bool {
        self.dir.path().join("argv").exists()
    }
    fn endpoint(&mut self, server: &Server) {
        self.handler.upload_policy = crate::share_yabu::UploadPolicy::local(
            server.endpoint.clone(),
            Duration::from_secs(2),
            Duration::from_secs(3),
        );
    }
}

#[derive(Clone)]
enum Response {
    Valid,
    Status(u16),
    Body(String),
    Stall,
    Held(Arc<tokio::sync::Notify>),
    Redirect(String),
}
struct Request {
    headers: String,
    body: Vec<u8>,
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(responses: Vec<Response>, effects: PathBuf) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/upload", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let mut index = 0;
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(socket.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(head).unwrap();
                let length = header(&headers, "content-length").parse::<usize>().unwrap();
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let response = responses
                    .get(index)
                    .unwrap_or(&Response::Status(500))
                    .clone();
                let hash = format!("{:x}", Sha256::digest(&body));
                captured.lock().unwrap().push(Request { headers, body });
                use std::io::Write as _;
                writeln!(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&effects)
                        .unwrap(),
                    "upload-{index}"
                )
                .unwrap();
                let (status, body, extra) = match response {
                    Response::Valid => (201, json!({"url":format!("https://cdn.example.test/file-{index}?v=1"), "sha256":hash,"size":length}).to_string(), String::new()),
                    Response::Held(release) => {
                        release.notified().await;
                        (201, json!({"url":format!("https://cdn.example.test/file-{index}?v=1"), "sha256":hash,"size":length}).to_string(), String::new())
                    }
                    Response::Status(status) => (status, String::new(), String::new()),
                    Response::Body(body) => (200, body, String::new()),
                    Response::Redirect(url) => (307, String::new(), format!("Location: {url}\r\n")),
                    Response::Stall => {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").await.unwrap();
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                };
                let reply = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
                index += 1;
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
fn header<'a>(headers: &'a str, name: &str) -> &'a str {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .unwrap()
        .1
        .trim()
}

#[tokio::test]
async fn reply_and_say_upload_verified_bytes_then_publish_once_with_ordered_urls() {
    let _env = crate::ENV_LOCK.lock().await;
    for operation in ["reply", "say"] {
        let mut f = Fixture::new();
        let server = Server::start(
            vec![Response::Valid, Response::Valid],
            f.dir.path().join("effects"),
        )
        .await;
        f.endpoint(&server);
        let mut payload = f.payload();
        let id = "ab".repeat(32);
        if operation == "reply" {
            payload["event"] = json!(format!("nostr:event:v1:default:{id}"));
        } else {
            payload["user"] = json!(id.to_uppercase());
        }
        let result = f
            .handler
            .handle("call", "binding", operation, &payload)
            .await;
        assert!(
            matches!(result, InvokeOutcome::Ok(_)),
            "publisher did not succeed"
        );
        let argv = std::fs::read_to_string(f.dir.path().join("argv")).unwrap();
        let args = if operation == "reply" {
            format!("reply\n--\n{id}\n")
        } else {
            format!("post\n--mention\n{id}\n--\n")
        };
        assert_eq!(argv, format!("--config\n{}\n{args}hello\n\nhttps://cdn.example.test/file-0?v=1\nhttps://cdn.example.test/file-1?v=1\n", f.handler.config_path.display()));
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("effects")).unwrap(),
            "upload-0\nupload-1\npost\n"
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (i, request) in requests.iter().enumerate() {
            assert!(request.headers.starts_with("PUT /upload HTTP/1.1\r\n"));
            assert_eq!(
                request.body,
                if i == 0 {
                    b"first".as_slice()
                } else {
                    b"second".as_slice()
                }
            );
            assert_eq!(header(&request.headers, "content-type"), "text/plain");
            let hash = format!("{:x}", Sha256::digest(&request.body));
            assert_eq!(header(&request.headers, "x-sha-256"), hash);
            let encoded = header(&request.headers, "authorization")
                .strip_prefix("Nostr ")
                .unwrap();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD.encode(&decoded),
                encoded
            );
            let event = nostr::Event::from_json(decoded).unwrap();
            // Verify ID and Schnorr signature by decoding the actual wire event,
            // independently of the uploader's builder path.
            event.verify().unwrap();
            assert_eq!(event.pubkey, f.keys.public_key());
            assert_eq!(event.kind, nostr::Kind::Custom(24242));
            assert_eq!(event.content, "Upload");
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(
                value["tags"],
                json!([
                    ["t", "upload"],
                    ["x", hash],
                    ["expiration", (event.created_at.as_u64() + 300).to_string()]
                ])
            );
            assert!(
                nostr::Timestamp::now()
                    .as_u64()
                    .abs_diff(event.created_at.as_u64())
                    < 5
            );
        }
    }
}

#[tokio::test]
async fn whole_batch_preflight_rejects_without_http_or_publication() {
    let _env = crate::ENV_LOCK.lock().await;
    for case in [
        "raw",
        "hash",
        "mime",
        "count",
        "total",
        "oversize",
        "mention",
        "target",
        "text",
        "key",
        "mismatch",
        "root",
        "relative-root",
    ] {
        let mut f = Fixture::new();
        let server = Server::start(vec![], f.dir.path().join("effects")).await;
        f.endpoint(&server);
        let mut payload = f.payload();
        let mut operation = "say";
        match case {
            "raw" => payload["files"] = json!(["workspace/file.txt"]),
            "hash" => payload["files"][1]["sha256"] = json!("00".repeat(32)),
            "mime" => payload["files"][1]["media_type"] = json!("application/zip"),
            "count" => payload["files"] = json!(vec![f.files[0].clone(); 11]),
            "total" => {
                payload["files"][0]["size"] = json!(crate::attachments::MAX_BYTES);
            }
            "oversize" => payload["files"][0]["size"] = json!(u64::MAX),
            "mention" => payload["user"] = json!(42),
            "target" => {
                operation = "reply";
                payload["event"] = json!("invalid");
            }
            "text" => payload["text"] = json!(" "),
            "key" => f.handler.secret = None,
            "mismatch" => f.handler.self_pubkey = "00".repeat(32),
            "root" => f.handler.attachment_outbox_root = None,
            "relative-root" => f.handler.attachment_outbox_root = Some("relative".into()),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                f.handler
                    .handle("call", "binding", operation, &payload)
                    .await,
                InvokeOutcome::Rejected
            ),
            "{case}"
        );
        assert_eq!(server.count(), 0, "{case}");
        assert!(!f.posted(), "{case}");
    }
}

#[tokio::test]
async fn partial_effect_outcomes_never_retry_or_publish_incomplete_batch() {
    let _env = crate::ENV_LOCK.lock().await;
    for (responses, rejected, count) in [
        (vec![Response::Status(402)], true, 1),
        (vec![Response::Status(413)], true, 1),
        (vec![Response::Status(500)], false, 1),
        (vec![Response::Body("not json".into())], false, 1),
        (vec![Response::Body("x".repeat(65537))], false, 1),
        (vec![Response::Valid, Response::Status(413)], false, 2),
    ] {
        let mut f = Fixture::new();
        let server = Server::start(responses, f.dir.path().join("effects")).await;
        f.endpoint(&server);
        let outcome = f
            .handler
            .handle("call", "binding", "say", &f.payload())
            .await;
        assert_eq!(
            matches!(outcome, InvokeOutcome::Rejected),
            rejected,
            "wrong rejection classification"
        );
        if !rejected {
            assert!(matches!(outcome, InvokeOutcome::Indeterminate));
        }
        assert_eq!(server.count(), count);
        assert!(!f.posted());
    }
}

#[tokio::test]
async fn invalid_success_descriptors_are_indeterminate() {
    let _env = crate::ENV_LOCK.lock().await;
    for (url, hash, size) in [
        ("https://cdn.example.test/x", "00".repeat(32), 5),
        (
            "https://cdn.example.test/x",
            format!("{:x}", Sha256::digest(b"first")),
            6,
        ),
        (
            "https://@cdn.example.test/x",
            format!("{:x}", Sha256::digest(b"first")),
            5,
        ),
        (
            "http://cdn.example.test/x",
            format!("{:x}", Sha256::digest(b"first")),
            5,
        ),
        (
            "https://user@cdn.example.test/x",
            format!("{:x}", Sha256::digest(b"first")),
            5,
        ),
        (
            "https://cdn.example.test/x\ny",
            format!("{:x}", Sha256::digest(b"first")),
            5,
        ),
    ] {
        let mut f = Fixture::new();
        let server = Server::start(
            vec![Response::Body(
                json!({"url":url,"sha256":hash,"size":size}).to_string(),
            )],
            f.dir.path().join("effects"),
        )
        .await;
        f.endpoint(&server);
        assert!(matches!(
            f.handler
                .handle("call", "binding", "say", &f.payload())
                .await,
            InvokeOutcome::Indeterminate
        ));
        assert_eq!(server.count(), 1);
        assert!(!f.posted());
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let mut f = Fixture::new();
    let destination =
        Server::start(vec![Response::Valid], f.dir.path().join("other-effects")).await;
    let server = Server::start(
        vec![Response::Redirect(destination.endpoint.clone())],
        f.dir.path().join("effects"),
    )
    .await;
    f.endpoint(&server);
    assert!(matches!(
        f.handler
            .handle("call", "binding", "say", &f.payload())
            .await,
        InvokeOutcome::Indeterminate
    ));
    assert_eq!(server.count(), 1);
    assert_eq!(destination.count(), 0);
    assert!(!f.posted());
}

#[tokio::test]
async fn request_and_phase_deadlines_interrupt_active_response_and_cancel_next_stage() {
    for phase in [false, true] {
        let mut f = Fixture::new();
        let server = Server::start(vec![Response::Stall], f.dir.path().join("effects")).await;
        f.handler.upload_policy = crate::share_yabu::UploadPolicy::local(
            server.endpoint.clone(),
            if phase {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(150)
            },
            if phase {
                Duration::from_millis(150)
            } else {
                Duration::from_secs(5)
            },
        );
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            f.handler.handle("call", "binding", "say", &f.payload()),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, InvokeOutcome::Indeterminate));
        assert_eq!(server.count(), 1);
        assert!(!f.posted());
    }
    let mut f = Fixture::new();
    let release = Arc::new(tokio::sync::Notify::new());
    let server = Server::start(
        vec![Response::Held(release.clone()), Response::Valid],
        f.dir.path().join("effects"),
    )
    .await;
    f.endpoint(&server);
    let payload = f.payload();
    let mut future = Box::pin(f.handler.handle("call", "binding", "say", &payload));
    tokio::select! {
        _ = &mut future => panic!("stalled handler completed"),
        _ = async { while server.count() == 0 { tokio::time::sleep(Duration::from_millis(5)).await; } } => {}
    }
    // Dropping the actual handler future is the transport's cancellation seam.
    drop(future);
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
    assert!(!f.posted());
}

#[tokio::test]
async fn publisher_failure_after_upload_is_indeterminate_even_if_spawn_fails() {
    let _env = crate::ENV_LOCK.lock().await;
    for spawn_failure in [false, true] {
        let mut f = Fixture::new();
        let server = Server::start(
            vec![Response::Valid, Response::Valid],
            f.dir.path().join("effects"),
        )
        .await;
        f.endpoint(&server);
        if spawn_failure {
            f.handler.nostaro_bin = f.dir.path().join("missing");
        } else {
            std::fs::write(&f.handler.nostaro_bin, "#!/bin/sh\nexit 1\n").unwrap();
        }
        assert!(matches!(
            f.handler
                .handle("call", "binding", "say", &f.payload())
                .await,
            InvokeOutcome::Indeterminate
        ));
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn dry_run_and_empty_files_preserve_text_path_without_http() {
    let _env = crate::ENV_LOCK.lock().await;
    let mut f = Fixture::new();
    let server = Server::start(vec![], f.dir.path().join("effects")).await;
    f.endpoint(&server);
    f.handler.dry_run = true;
    f.handler.secret = None; // cannot sign; still no external work in dry-run
    assert!(matches!(
        f.handler
            .handle("call", "binding", "say", &f.payload())
            .await,
        InvokeOutcome::Ok(_)
    ));
    assert!(!f.posted());
    f.handler.dry_run = false;
    f.handler.attachment_outbox_root = None;
    for payload in [
        json!({"text":"hello"}),
        json!({"text":"hello","files":null}),
        json!({"text":"hello","files":[]}),
    ] {
        assert!(matches!(
            f.handler.handle("call", "binding", "say", &payload).await,
            InvokeOutcome::Ok(_)
        ));
    }
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn files_without_root_and_malformed_mention_reject_before_publisher() {
    let _env = crate::ENV_LOCK.lock().await;
    let handler = NostrInvokeHandler::new("/bin/echo".into(), "/unused".into(), None, false);
    for payload in [
        json!({"text":"hello", "files":["raw/path.png"]}),
        json!({"text":"hello", "user":42}),
    ] {
        assert!(
            matches!(
                handler.handle("call", "binding", "say", &payload).await,
                InvokeOutcome::Rejected
            ),
            "invalid payload reached publisher: {payload}"
        );
    }
}

#[test]
fn attachment_root_survives_placement_serialization() {
    let placement: crate::config::Placement = serde_json::from_value(json!({
        "core_socket":"/tmp/core.sock", "nostaro_bin":"nostaro", "instances":[],
        "attachment_outbox_root":"/tmp/outbox"
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(placement).unwrap()["attachment_outbox_root"],
        "/tmp/outbox"
    );
    for enabled in [false, true] {
        let declarations = operation_declarations_with_files(enabled);
        for op in declarations.as_array().unwrap() {
            let expected = enabled && matches!(op["name"].as_str(), Some("reply" | "say"));
            assert_eq!(
                op["input_schema"]["properties"]["files"]["items"]["format"] == "workspace-file",
                expected
            );
        }
    }
}

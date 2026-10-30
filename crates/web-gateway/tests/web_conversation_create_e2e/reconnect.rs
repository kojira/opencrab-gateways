/// QC 症状の再現: core 再起動後も gateway が自動再接続し、作成が 201 ready → message → say。
#[test]
fn core_restart_gateway_reconnects_then_create_201_message_say() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-reconn-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);
    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");
    // A conversation made before the restart: it must stay this gateway's conversation.
    let before = create(gw_port, "Before");
    let before_session = before["session_id"].as_str().unwrap().to_string();

    drop(core);
    let down_start = Instant::now();
    while down_start.elapsed() < Duration::from_secs(5) {
        if http(
            core_port,
            "GET",
            "/health",
            None,
            None,
            Duration::from_secs(1),
        )
        .is_none()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let cut_start = Instant::now();
    let mut cut = None;
    while cut_start.elapsed() < Duration::from_secs(8) {
        match http(
            gw_port,
            "POST",
            &format!("/api/web-conversations/{before_session}/messages"),
            None,
            Some(
                r#"{"client_message_id":"dddddddd-dddd-4ddd-8ddd-dddddddddddd","text":"probe","attachments":[]}"#,
            ),
            Duration::from_secs(2),
        ) {
            Some((st, body)) => {
                assert_eq!(st, 503, "cut {body}");
                if body.contains("disconnect") {
                    cut = Some(body);
                    break;
                }
            }
            None => panic!("gateway HTTP died with core"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(cut.is_some(), "expected disconnect while core is down");

    let core = spawn_core(root.path());
    assert!(
        wait_http(core_port, "/health", Duration::from_secs(30)),
        "core did not restart: {}",
        std::fs::read_to_string(root.path().join("core.log"))
            .unwrap_or_else(|error| format!("<core log unavailable: {error}>"))
    );
    let v = create(gw_port, "Reconnect");
    assert_eq!(v["state"], "ready", "{v}");
    let session = v["session_id"].as_str().unwrap().to_string();
    // The conversation made before the restart is still this gateway's.
    let (st, body) = http(
        gw_port,
        "GET",
        &format!("/api/web-conversations/{before_session}"),
        None,
        None,
        Duration::from_secs(5),
    )
    .expect("get");
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("\"ready\""), "{body}");

    let sse_rx = spawn_sse(gw_port, &session);
    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let (st, accepted) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("message");
    assert_eq!(st, 202, "{accepted}");
    let a: serde_json::Value = serde_json::from_str(accepted.trim()).unwrap();
    assert_eq!(a["state"], "accepted", "{accepted}");
    mock.release();
    let sse = sse_rx.recv_timeout(Duration::from_secs(30)).expect("sse");
    assert!(
        sse.contains("event: message") && sse.contains(REPLY),
        "{sse}"
    );

    drop(gw);
    drop(core);
    let conn = Connection::open(&db).unwrap();
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = 'hello from e2e'",
            [session.clone()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, 1);
    let reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = ?2",
            [session.clone(), REPLY.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply, 1);
    let _ = std::fs::remove_file(&sock);
}

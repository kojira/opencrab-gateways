#[test]
fn creation_is_owned_by_gateway_and_requires_its_live_instance() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-disc-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);

    let (st, _) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some("{}"),
        Duration::from_secs(5),
    )
    .expect("server route response");
    assert_eq!(st, 404, "server must not proxy gateway creation");

    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");
    let value = create(gw_port, "x");
    assert_eq!(value["state"], "ready");
    assert_eq!(counts(&db), (1, 1));

    drop(gw);
    drop(core);
    let _ = std::fs::remove_file(&sock);
}

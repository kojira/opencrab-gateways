//! D-1092: watch の正本はゲートウェイの保存ファイル。実 binary を mock core につないで確かめる。
//!
//! 固定する不変条件（production 経路: main → spawn_instance → 車線）:
//! - 初回起動で、配置の `watches`（旧来の 1 件）が保存ファイルへ 1 度だけ移り、挙動は変わらない
//!   （同じ authors で watch 車線が立つ）。保存ファイルは mode 0600。
//! - 保存ファイルが外から書き換えられたら（設定画面）、再起動なしで watch 車線が新しい値で作り直される。
//! - 再起動後は保存ファイルが正本で、配置の `watches` を再び種にしない。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

const INSTANCE: &str = "00000000-0000-4000-8000-000000000001";
const LEGACY_AUTHOR: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const NEW_AUTHOR: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// `watch` で呼ばれたら引数を 1 行ずつ記録して眠る偽 nostaro。それ以外は失敗で返す。
fn write_fake_nostaro(dir: &Path, log: &Path) -> PathBuf {
    let path = dir.join("nostaro");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  watch) echo \"$*\" >> '{}'; exec sleep 300 ;;\n  *) exit 1 ;;\nesac\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn write_placement(dir: &Path, nostaro: &Path, legacy_author: &str) -> PathBuf {
    let config = json!({
        "relays": ["wss://relay.invalid"],
        "self_pubkey": "a".repeat(64),
        "name": "crab",
        "access": {"owner": ["b".repeat(64)]},
        "watches": [{
            "id": 4,
            "interval_secs": 1800,
            "filter": {"authors": [legacy_author], "keywords": [], "kinds": [1]}
        }],
    });
    let placement = json!({
        "core_socket": dir.join("core.sock"),
        "nostaro_bin": nostaro,
        "instances": [{
            "instance_id": INSTANCE,
            "agent_id": "agent",
            "address": "nostr-agent",
            "config_b64": base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&config).unwrap()),
        }],
    });
    let path = dir.join("placement.json");
    std::fs::write(&path, placement.to_string()).unwrap();
    path
}

/// 実 binary を起動して hello に ok を返す。以後の frame は読み捨てる。
fn start_gateway(socket: &Path, placement: &Path) -> Child {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).unwrap();
    let mut gateway = Command::new(env!("CARGO_BIN_EXE_nostr-gateway"))
        .arg("instance")
        .arg(placement)
        .stdin(Stdio::null())
        .env_remove("NOSTARO_SECRET_KEY")
        .spawn()
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut accepted = None;
    assert!(
        wait_until(Duration::from_secs(15), || {
            if let Ok((stream, _)) = listener.accept() {
                accepted = Some(stream);
            }
            accepted.is_some() || gateway.try_wait().unwrap().is_some()
        }),
        "nostr-gateway never connected"
    );
    let stream = accepted.expect("nostr-gateway exited before connecting");
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(hello["m"], "hello", "{hello}");
    let names: Vec<&str> = hello["operations"]
        .as_array()
        .map(|ops| ops.iter().filter_map(|op| op["name"].as_str()).collect())
        .unwrap_or_default();
    for op in ["watch_list", "watch_set", "watch_delete"] {
        assert!(names.contains(&op), "hello must declare {op}: {names:?}");
    }
    writeln!(writer, "{}", json!({"id": hello["id"], "m": "ok"})).unwrap();
    std::thread::spawn(move || {
        let _keep_listener = listener;
        let mut line = String::new();
        while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            line.clear();
        }
        drop(writer);
    });
    gateway
}

fn stop(mut gateway: Child) {
    unsafe { libc::kill(gateway.id() as i32, libc::SIGTERM) };
    if !wait_until(Duration::from_secs(10), || {
        gateway.try_wait().unwrap().is_some()
    }) {
        let _ = gateway.kill();
    }
    let _ = gateway.wait();
}

fn watch_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("--author="))
        .map(str::to_string)
        .collect()
}

#[test]
fn legacy_watch_migrates_once_and_settings_edits_apply_live() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("watch-args.log");
    let nostaro = write_fake_nostaro(dir.path(), &log);
    let placement = write_placement(dir.path(), &nostaro, LEGACY_AUTHOR);
    let socket = dir.path().join("core.sock");
    let store = dir.path().join(format!("nostaro-watches-{INSTANCE}.json"));

    // 1. 初回起動: 配置の watch がそのまま保存ファイルへ移り、同じ authors で車線が立つ。
    let gateway = start_gateway(&socket, &placement);
    assert!(
        wait_until(Duration::from_secs(15), || watch_lines(&log)
            .iter()
            .any(|l| l.contains(LEGACY_AUTHOR))),
        "legacy watch lane never started: {:?}",
        std::fs::read_to_string(&log)
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
    assert_eq!(stored["watches"][0]["id"], json!(4));
    assert_eq!(stored["watches"][0]["interval_secs"], json!(1800));
    assert_eq!(stored["watches"][0]["authors"], json!([LEGACY_AUTHOR]));
    assert_eq!(
        std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // 2. 設定画面と同じ書き方（保存ファイルの差し替え）で authors を変えると、再起動なしで反映される。
    let edited = json!({"watches": [{
        "id": 4, "interval_secs": 900, "authors": [NEW_AUTHOR], "keywords": [], "kinds": [1]
    }]});
    let tmp = store.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&edited).unwrap()).unwrap();
    std::fs::rename(&tmp, &store).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || watch_lines(&log)
            .iter()
            .any(|l| l.contains(NEW_AUTHOR))),
        "edited watch never applied: {:?}",
        std::fs::read_to_string(&log)
    );
    stop(gateway);

    // 3. 再起動: 保存ファイルが正本。配置の旧 authors を再び種にしない。
    std::fs::write(&log, "").unwrap();
    let gateway = start_gateway(&socket, &placement);
    assert!(
        wait_until(Duration::from_secs(15), || watch_lines(&log)
            .iter()
            .any(|l| l.contains(NEW_AUTHOR))),
        "stored watch not used after restart: {:?}",
        std::fs::read_to_string(&log)
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !watch_lines(&log).iter().any(|l| l.contains(LEGACY_AUTHOR)),
        "placement watches must not be re-seeded after the store exists"
    );
    stop(gateway);
}

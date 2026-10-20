//! D-1091: この instance が hello で宣言する環境変数（`run_env`）。
//!
//! 実 binary を mock core の UDS につなぎ、**送られた hello frame そのもの**を見る。unit では
//! 「map の作り方」しか固定できないので、production 経路（main → spawn_instance → hello）で
//! 宣言が実際に wire に載ることをここで固定する。
//!
//! 固定する不変条件:
//! - 1 instance のプロセスは、自分の relays-only config と自分の鍵を宣言する。
//! - 1 プロセスに複数 instance を載せた配置では、**誰にも鍵を宣言しない**（他人の鍵を継承させない）。
//! - 鍵の値は frame にだけ載り、ログ・エラーには出ない。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

const INSTANCE_A: &str = "00000000-0000-4000-8000-000000000001";
const INSTANCE_B: &str = "00000000-0000-4000-8000-000000000002";
const SECRET_ENV: &str = "NOSTARO_SECRET_KEY";
const CONFIG_ENV: &str = "NOSTARO_CONFIG";

/// 何も publish しない偽 nostaro（このテストは hello だけを見る）。
fn write_fake_nostaro(dir: &Path) -> PathBuf {
    let path = dir.join("nostaro");
    std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn instance(instance_id: &str, address: &str, name: &str) -> Value {
    let config = json!({
        "relays": ["wss://relay.invalid"],
        "self_pubkey": "a".repeat(64),
        "name": name,
        "access": {"owner": ["b".repeat(64)]},
    });
    json!({
        "instance_id": instance_id,
        "revision": 1,
        "address": address,
        "config_b64": base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&config).unwrap()),
    })
}

/// 配置を書いて実 binary を起動し、mock core で受けた hello frame を instance_id 順に返す。
fn hellos_for(instances: Vec<Value>, secret: Option<&str>) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("core.sock");
    let nostaro = write_fake_nostaro(dir.path());
    let placement = json!({
        "core_socket": socket,
        "nostaro_bin": nostaro,
        "instances": instances,
    });
    let placement_path = dir.path().join("placement.json");
    std::fs::write(&placement_path, placement.to_string()).unwrap();

    let listener = UnixListener::bind(&socket).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nostr-gateway"));
    command
        .arg("instance")
        .arg(&placement_path)
        .stdin(Stdio::null())
        .env_remove(SECRET_ENV);
    if let Some(secret) = secret {
        command.env(SECRET_ENV, secret);
    }
    let mut gateway = command.spawn().unwrap();

    let expect = instances_len(&placement_path);
    let mut frames = Vec::new();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while frames.len() < expect && Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let hello: Value = serde_json::from_str(&line).unwrap();
                // ack だけ返し、bind は返さない（watch 車線を起こさない）。
                let _ = writeln!(writer, "{}", json!({"id": hello["id"], "m": "ok"}));
                frames.push(hello);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if gateway.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
    let _ = gateway.kill();
    let _ = gateway.wait();
    assert_eq!(
        frames.len(),
        expect,
        "expected one hello per instance: {frames:?}"
    );
    frames.sort_by_key(|frame| frame["instance_id"].as_str().unwrap_or("").to_string());
    frames
}

fn instances_len(placement_path: &Path) -> usize {
    let raw: Value = serde_json::from_slice(&std::fs::read(placement_path).unwrap()).unwrap();
    raw["instances"].as_array().unwrap().len()
}

#[test]
fn a_single_instance_process_declares_its_own_config_and_key() {
    let frames = hellos_for(
        vec![instance(INSTANCE_A, "nostr-a", "crab-a")],
        Some("nsec1alpha"),
    );
    let env = frames[0]["run_env"]
        .as_object()
        .unwrap_or_else(|| panic!("hello carries no run_env: {}", frames[0]));
    let config = env[CONFIG_ENV].as_str().unwrap();
    assert!(
        config.contains(INSTANCE_A),
        "config path belongs to this instance: {config}"
    );
    assert_eq!(env[SECRET_ENV].as_str(), Some("nsec1alpha"));
}

#[test]
fn a_two_instance_process_declares_no_key_to_either() {
    let frames = hellos_for(
        vec![
            instance(INSTANCE_A, "nostr-a", "crab-a"),
            instance(INSTANCE_B, "nostr-b", "crab-b"),
        ],
        Some("nsec1alpha"),
    );
    let first = frames[0]["run_env"].as_object().unwrap();
    let second = frames[1]["run_env"].as_object().unwrap();
    assert!(
        first.get(SECRET_ENV).is_none() && second.get(SECRET_ENV).is_none(),
        "an instance must never inherit the process key: {first:?} / {second:?}"
    );
    assert_ne!(
        first[CONFIG_ENV], second[CONFIG_ENV],
        "each instance declares its own config path"
    );
    assert!(frames
        .iter()
        .all(|frame| frame.to_string().find("nsec1alpha").is_none()));
}

#[test]
fn without_a_key_the_declaration_is_config_only() {
    let frames = hellos_for(vec![instance(INSTANCE_A, "nostr-a", "crab-a")], None);
    let env = frames[0]["run_env"].as_object().unwrap();
    assert!(env.get(SECRET_ENV).is_none(), "{env:?}");
    assert!(env.contains_key(CONFIG_ENV));
}

//! SIGTERM で止めた instance が nostaro 子プロセスを残さないこと（D-1070 の後続）。
//! 実 binary を mock core の UDS につなぎ、偽 nostaro watch を起動させてから SIGTERM を送る。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

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

fn write_fake_nostaro(dir: &Path, pid_file: &Path) -> std::path::PathBuf {
    let path = dir.join("nostaro");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  watch) echo $$ > '{}'; exec sleep 300 ;;\n  *) exit 1 ;;\nesac\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn sigterm_stops_nostaro_watch_child() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("core.sock");
    let pid_file = dir.path().join("watch.pid");
    let nostaro = write_fake_nostaro(dir.path(), &pid_file);
    let config = json!({
        "relays": ["wss://relay.invalid"],
        "self_pubkey": "a".repeat(64),
        "name": "crab",
        "access": {"owner": ["b".repeat(64)]},
    });
    let placement = json!({
        "core_socket": socket,
        "nostaro_bin": nostaro,
        "instances": [{
            "instance_id": "00000000-0000-4000-8000-000000000001",
            "revision": 1,
            "address": "nostr-agent",
            "config_b64": base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&config).unwrap()),
        }],
    });
    let placement_path = dir.path().join("placement.json");
    std::fs::write(&placement_path, placement.to_string()).unwrap();

    let listener = UnixListener::bind(&socket).unwrap();
    let mut gateway = Command::new(env!("CARGO_BIN_EXE_nostr-gateway"))
        .arg(&placement_path)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();

    // mock core: hello に ok、続けて bind を送り ack を受ける。
    listener.set_nonblocking(true).unwrap();
    let mut accepted = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            if let Ok((stream, _)) = listener.accept() {
                accepted = Some(stream);
            }
            accepted.is_some() || gateway.try_wait().unwrap().is_some()
        }),
        "nostr-gateway never connected"
    );
    let stream = match accepted {
        Some(stream) => stream,
        None => panic!(
            "nostr-gateway exited before connecting: {:?}",
            gateway.wait()
        ),
    };
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
    writeln!(writer, "{}", json!({"id": hello["id"], "m": "ok"})).unwrap();
    writeln!(
        writer,
        "{}",
        json!({"id": "bind:00000000-0000-4000-8000-000000000002", "m": "bind", "binding_id": "00000000-0000-4000-8000-000000000002", "address": "nostr-agent"})
    )
    .unwrap();
    // mock core keeps draining frames so the gateway never blocks on the socket.
    std::thread::spawn(move || {
        let mut line = String::new();
        while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            line.clear();
        }
    });

    assert!(
        wait_until(Duration::from_secs(10), || std::fs::read_to_string(
            &pid_file
        )
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)),
        "fake nostaro watch never started"
    );
    let watch_pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(watch_pid));

    unsafe { libc::kill(gateway.id() as i32, libc::SIGTERM) };
    let exited = wait_until(Duration::from_secs(10), || {
        gateway.try_wait().unwrap().is_some()
    });
    let watch_left = wait_until(Duration::from_secs(5), || !alive(watch_pid));
    if !watch_left {
        unsafe { libc::kill(watch_pid, libc::SIGKILL) };
    }
    if !exited {
        let _ = gateway.kill();
    }
    let _ = gateway.wait();
    assert!(exited, "nostr-gateway did not exit on SIGTERM");
    assert!(watch_left, "nostaro watch {watch_pid} outlived its gateway");
}

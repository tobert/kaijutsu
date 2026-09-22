//! `kaijutsu-mcp --connect` answers MCP before the kernel does.
//!
//! Claude Code gives a stdio server a short window to answer its version
//! probe; a server that answers late gets a pipelined legacy `initialize`
//! that rmcp handles badly, and one that exits gets no tools at all. The
//! kernel connection, session registration, and hook socket all happen behind
//! the MCP handshake, so none of them can hold it: not a kernel that is down,
//! and not a registration that retries.
//!
//! These run the real binary, with `XDG_RUNTIME_DIR` pointed at a scratch
//! directory so the hook socket never meets a live session's.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kj-mcp-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A port nothing listens on: bind, read the number, release it.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn spawn_connected_mcp(scratch: &Scratch, port: u16) -> Child {
    let key = scratch.0.join("key");
    kaijutsu_server::SshServerConfig::ephemeral(0)
        .root_key()
        .write_openssh_file(&key, Default::default())
        .expect("write test key");
    Command::new(env!("CARGO_BIN_EXE_kaijutsu-mcp"))
        .args(["--connect", "--host", "127.0.0.1", "--port", &port.to_string(), "--key-file"])
        .arg(&key)
        .env("XDG_RUNTIME_DIR", &scratch.0)
        .env_remove("KAIJUTSU_KEY_FINGERPRINT")
        .env_remove("KAIJUTSU_KEY_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kaijutsu-mcp")
}

/// Send `request` and wait up to `limit` for the line answering its id.
fn answer_within(child: &mut Child, request: Value, limit: Duration) -> Value {
    let id = request["id"].clone();
    writeln!(child.stdin.as_mut().unwrap(), "{request}").unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let started = Instant::now();
    loop {
        let remaining = limit.checked_sub(started.elapsed()).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("no answer to {request} within {limit:?}")
        });
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = child.wait().unwrap();
                panic!("kaijutsu-mcp closed stdout before answering {request} ({status})");
            }
        }
    }
}

#[test]
fn the_discover_probe_is_answered_while_the_kernel_is_unreachable() {
    let scratch = Scratch::new("down");
    let mut child = spawn_connected_mcp(&scratch, closed_port());

    let answer = answer_within(
        &mut child,
        json!({
            "jsonrpc": "2.0", "id": "probe", "method": "server/discover",
            "params": { "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": { "name": "startup-test", "version": "0" },
            } },
        }),
        Duration::from_secs(2),
    );
    assert!(answer["result"]["supportedVersions"].is_array(), "{answer:#}");
    let _ = child.kill();
}

/// Tool calls wait on the gate while registration runs, and give up at the
/// limit rather than hang when it never settles.
#[tokio::test]
async fn the_startup_gate_holds_calls_until_settled_and_no_longer_than_its_limit() {
    use kaijutsu_mcp::StartupGate;

    let never = StartupGate::pending();
    let started = Instant::now();
    assert!(!never.settled_within(Duration::from_millis(100)).await);
    assert!(started.elapsed() >= Duration::from_millis(100));

    let gate = StartupGate::pending();
    let settler = gate.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        settler.settle();
    });
    assert!(gate.settled_within(Duration::from_secs(5)).await);
    assert!(StartupGate::open().settled_within(Duration::ZERO).await);
}

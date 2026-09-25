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

/// A session whose MCP starts while the kernel is down joins once the kernel
/// answers. The kernel starts only after the MCP logs the end of its startup
/// window, so this exercises the late path however long that window grows.
#[test]
fn a_session_started_before_the_kernel_joins_once_it_answers() {
    use kaijutsu_client::{KeySource, SshConfig};
    use kaijutsu_mcp::{Backend, KaijutsuMcp};
    use kaijutsu_server::{SshServer, SshServerConfig};

    let scratch = Scratch::new("late");
    let port = closed_port();
    let config = SshServerConfig::ephemeral(port);
    let root_key = config.root_key();
    let key_path = scratch.0.join("key");
    root_key.write_openssh_file(&key_path, Default::default()).expect("write test key");

    // Startup joins `cc-<cwd basename>-<first 8 of the session id>`.
    let session_id = format!("{:08x}-late-kernel", std::process::id());
    let label_suffix = format!("-{}", &session_id[..8]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaijutsu-mcp"))
        .args(["--connect", "--insecure", "--host", "127.0.0.1", "--port", &port.to_string(), "--key-file"])
        .arg(&key_path)
        .env("XDG_RUNTIME_DIR", &scratch.0)
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_SESSION_ID", &session_id)
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDE_PID")
        .env_remove("KAIJUTSU_KEY_FINGERPRINT")
        .env_remove("KAIJUTSU_KEY_FILE")
        .env_remove("KAIJUTSU_PARENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kaijutsu-mcp");

    // Drain stderr for the child's whole life; a full pipe would stall it.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let window_ended = Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = window_ended.checked_duration_since(Instant::now()).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("kaijutsu-mcp never logged the end of its startup registration window")
        });
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("Auto-register found no kernel") => break,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("kaijutsu-mcp exited during startup ({:?})", child.wait());
            }
        }
    }
    std::thread::spawn(move || while rx.recv().is_ok() {});

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    let joined = rt.block_on(local.run_until(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("rebind the reserved port");
        tokio::task::spawn_local(async move {
            let _ = SshServer::new(config).run_on_listener(listener).await;
        });

        let observer = KaijutsuMcp::connect_with_config(
            SshConfig {
                host: "127.0.0.1".to_string(),
                port,
                username: "observer".to_string(),
                key_source: KeySource::InMemory(root_key),
                insecure: true,
            },
            "observer",
            None,
        );
        let Backend::Remote(remote) = observer.backend() else { unreachable!() };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        while tokio::time::Instant::now() < deadline {
            let contexts = remote.actor.list_contexts().await.unwrap_or_default();
            if let Some(ctx) = contexts.iter().find(|c| c.label.ends_with(&label_suffix)) {
                return Some(ctx.id);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        None
    }));
    let _ = child.kill();
    {
        // Live tasks hold SSH channels whose Drop spawns onto Tokio.
        let _runtime = rt.enter();
        drop(local);
    }
    assert!(joined.is_some(), "the session never registered after the kernel came up");
}

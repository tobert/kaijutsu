//! The hook client must answer before its host gives up.
//!
//! `kaijutsu-mcp hook claude|codex` runs on every hook event of every
//! session. If the MCP server behind its socket stops answering, the client
//! has to exit 0 on its own deadline: a client that waits for the host's kill
//! turns each tool call into a hook error, in every session at once.
//!
//! These tests run the real binary against a fake listener, with
//! `XDG_RUNTIME_DIR` pointed at a scratch directory so socket discovery never
//! reaches a live session's server.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kaijutsu_types::timeout::tiers;

/// How the fake listener misbehaves.
#[derive(Clone, Copy)]
enum Wedge {
    /// Answers the liveness ping, then never answers the event itself.
    AfterPing,
    /// Accepts connections and never reads or writes.
    OnAccept,
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kj-hook-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(dir.join("kaijutsu")).expect("create scratch runtime dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serve `wedge` on `path` from a background thread. Held connections are
/// kept alive for the life of the test process.
fn spawn_wedged_listener(path: &Path, wedge: Wedge) {
    let listener = UnixListener::bind(path).expect("bind fake hook socket");
    std::thread::spawn(move || {
        let mut held: Vec<UnixStream> = Vec::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            match wedge {
                Wedge::OnAccept => held.push(stream),
                Wedge::AfterPing => {
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    if line.contains(r#""event":"ping""#) {
                        let mut stream = stream;
                        let pong = format!(
                            r#"{{"status":"ok","pid":{},"pending_drifts":0}}"#,
                            std::process::id()
                        );
                        let _ = writeln!(stream, "{pong}");
                    } else {
                        held.push(stream);
                    }
                }
            }
        }
    });
}

/// Run `kaijutsu-mcp hook claude` against `socket` with a Claude fixture on
/// stdin. Kills the child and fails the test if it outlives `limit`.
fn run_hook_client(runtime_dir: &Path, socket: &Path, limit: Duration) -> (std::process::Output, Duration) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/user_prompt_submit.json");
    let payload = std::fs::read(&fixture).expect("read fixture");

    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_kaijutsu-mcp"))
        .args(["hook", "claude", "--socket"])
        .arg(socket)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook client");
    child.stdin.take().unwrap().write_all(&payload).expect("write hook payload");

    loop {
        if child.try_wait().expect("poll hook client").is_some() {
            break;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let out = child.wait_with_output().expect("reap hook client");
            panic!(
                "hook client still running after {limit:?}; the host would have \
                 killed it and reported a hook error. stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let elapsed = started.elapsed();
    (child.wait_with_output().expect("collect hook client output"), elapsed)
}

fn assert_fails_open(out: &std::process::Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "a stalled listener must not block the action; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "a fail-open exit prints no hook response, got:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_listener_that_stalls_after_the_ping_fails_open_before_the_host_gives_up() {
    let scratch = Scratch::new("after-ping");
    let socket = scratch.0.join("kaijutsu/hook-wedged.sock");
    spawn_wedged_listener(&socket, Wedge::AfterPing);

    let (out, elapsed) = run_hook_client(&scratch.0, &socket, tiers::CC_HOOK_DEADLINE * 2);

    assert_fails_open(&out);
    assert!(
        elapsed < tiers::HOST_HOOK_DEADLINE,
        "hook client took {elapsed:?}; the tightest host kills it at {:?}",
        tiers::HOST_HOOK_DEADLINE
    );
}

#[test]
fn a_listener_that_never_reads_fails_open_at_probe_speed() {
    let scratch = Scratch::new("on-accept");
    let socket = scratch.0.join("kaijutsu/hook-wedged.sock");
    spawn_wedged_listener(&socket, Wedge::OnAccept);

    let (out, elapsed) = run_hook_client(&scratch.0, &socket, tiers::CC_HOOK_DEADLINE * 2);

    assert_fails_open(&out);
    assert!(
        elapsed < tiers::HANDSHAKE,
        "a dead ping should cost about one probe, took {elapsed:?}"
    );
}

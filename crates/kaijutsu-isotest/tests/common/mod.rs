//! Shared harness for the isotest suite: boot a real `kaijutsu-server` child
//! process, connect over loopback SSH with `kaijutsu-client`, join the
//! genesis ROOT context. See `docs/isotest.md` for the rationale.
//!
//! `tests/common/mod.rs` (a subdirectory, not a bare `tests/common.rs`) is
//! the standard Rust convention for code shared between integration test
//! binaries without cargo treating it as a test target of its own —
//! `contrib/isotest`'s `cargo test --no-run --message-format=json`
//! discovery only picks up `tests/*.rs`, never `tests/*/*.rs`.
//!
//! Originally lived only in `isolation.rs` (the process-lifecycle suite);
//! extracted here so `filesystem.rs` (slice 2, VFS protection) can reuse the
//! exact same boot/connect/join path rather than a second copy that could
//! drift from it.
#![allow(dead_code)] // not every test binary uses every helper here

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_client::rpc::KernelHandle;
use kaijutsu_client::{connect_ssh, KeySource, RpcClient, SshConfig};
use russh::keys::{Algorithm, PrivateKey};

/// Skip unless we're inside the contrib/isotest container. Keeps a bare host
/// `cargo test` fast, and keeps this suite from acting on a machine with a
/// live kernel. Returns `true` when the caller should skip (and has already
/// printed why).
pub fn skip_unless_isotest() -> bool {
    if std::env::var("KAIJUTSU_ISOTEST").as_deref() != Ok("1") {
        eprintln!("skipped: run via contrib/isotest (KAIJUTSU_ISOTEST=1)");
        return true;
    }
    false
}

/// Every credential this suite mints carries this label — in the auth.db
/// nick, the pubkey comment, and the SSH username — so nothing it creates
/// can be mistaken for a durable identity.
pub const EPHEMERAL_LABEL: &str = "isotest-ephemeral";

pub fn server_bin() -> PathBuf {
    PathBuf::from(
        std::env::var("KAIJUTSU_SERVER_BIN")
            .expect("KAIJUTSU_SERVER_BIN is set by contrib/isotest"),
    )
}

/// capnp-rpc types are !Send: every test body runs on a current-thread
/// runtime inside a LocalSet, same as kaijutsu-server/tests/common.
pub fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// A kaijutsu-server child process on its own $HOME, plus the client key the
/// server was taught to accept (`add-key` runs before boot — the shipped
/// binary has allow_anonymous=false and there is no registration RPC).
pub struct TestKernel {
    pub child: Child,
    pub home: PathBuf,
    pub port: u16,
    pub key: Arc<PrivateKey>,
}

impl TestKernel {
    pub fn boot(name: &str, port: u16) -> Self {
        let home = std::env::temp_dir().join(format!("isotest-{name}"));
        Self::boot_at(home, port)
    }

    /// Boot on an explicit $HOME — the restart test reuses one across boots.
    pub fn boot_at(home: PathBuf, port: u16) -> Self {
        std::fs::create_dir_all(&home).expect("create test home");

        // Test rigs ALWAYS use an ephemeral key, clearly labeled as such:
        // generated fresh per boot, never reused, and the label survives
        // into auth.db (nick) and the key comment so a stray entry is
        // unmistakably a throwaway.
        let key = Arc::new(
            PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519)
                .expect("generate ephemeral client key"),
        );
        let mut pubkey = key.public_key().clone();
        pubkey.set_comment(EPHEMERAL_LABEL);
        let pub_path = home.join("isotest-ephemeral.pub");
        // add-key is idempotent enough for the restart case: re-adding the
        // same fingerprint on the same auth.db is fine.
        std::fs::write(
            &pub_path,
            pubkey.to_openssh().expect("serialize client pubkey"),
        )
        .expect("write client pubkey");
        let status = Command::new(server_bin())
            .args(["add-key", pub_path.to_str().unwrap(), "--nick", EPHEMERAL_LABEL])
            .env("HOME", &home)
            .status()
            .expect("run add-key");
        assert!(status.success(), "add-key failed with {status}");

        // Server output goes to files under $HOME so `contrib/isotest --keep`
        // debugging can read the story after the fact.
        let out = std::fs::File::create(home.join("server.stdout.log")).unwrap();
        let err = std::fs::File::create(home.join("server.stderr.log")).unwrap();
        let child = Command::new(server_bin())
            .args(["--port", &port.to_string()])
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn kaijutsu-server");

        TestKernel { child, home, port, key }
    }

    /// Retry-connect until the listener answers and auth completes. The
    /// server binds its listener during boot; from a child process the
    /// honest readiness signal is a successful authenticated connection.
    pub async fn connect_client(&self, key_source: KeySource) -> RpcClient {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let config = SshConfig {
                host: "127.0.0.1".into(),
                port: self.port,
                username: EPHEMERAL_LABEL.into(),
                key_source: key_source.clone(),
                insecure: true, // no known_hosts TOFU writes in the container
            };
            match connect_ssh(config).await {
                Ok(client) => return client,
                Err(e) => {
                    if Instant::now() > deadline {
                        panic!(
                            "server on port {} never became connectable: {e}\n--- server.stderr.log:\n{}",
                            self.port,
                            std::fs::read_to_string(self.home.join("server.stderr.log"))
                                .unwrap_or_default()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    }

    /// Connect with the in-memory ephemeral key and bind the kernel.
    pub async fn connect(&self) -> KernelHandle {
        let client = self
            .connect_client(KeySource::InMemory(self.key.clone()))
            .await;
        let (kernel, _id) = client.bind_kernel().await.expect("bind_kernel");
        // Leak the RpcClient so its connection outlives this call: dropping
        // it would tear down the transport under the KernelHandle.
        std::mem::forget(client);
        kernel
    }

    pub fn sigkill(&mut self) {
        self.child.kill().expect("SIGKILL server");
        self.child.wait().expect("reap server");
    }

    pub fn sigterm_and_wait(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait().expect("try_wait server") {
                Some(_) => return,
                None if Instant::now() > deadline => {
                    // SIGTERM is allowed to be un-handled someday-graceful,
                    // but it must not hang; escalate loudly.
                    panic!("server ignored SIGTERM for 10s");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

/// Join the genesis ROOT context (a director: exec + facade:shell out of the
/// box) and return the handle ready for tool calls.
pub async fn join_root(kernel: &KernelHandle) {
    let root = kernel
        .resolve_context_label("ROOT")
        .await
        .expect("resolve_context_label")
        .expect("genesis ROOT context exists on a fresh kernel");
    kernel
        .join_context(root.id, "isotest")
        .await
        .expect("join ROOT");
}

/// Start a background job through the shell tool; returns the bg id parsed
/// from the tool's text reply ("started background process <id>; …").
pub async fn start_bg(kernel: &KernelHandle, command: &str) -> String {
    let r = kernel
        .call_mcp_tool("shell", &serde_json::json!({"command": command, "background": true}))
        .await
        .expect("call shell tool");
    assert!(!r.is_error, "shell(background) errored: {}", r.content);
    r.content
        .strip_prefix("started background process ")
        .and_then(|rest| rest.split([';', ' ']).next())
        .unwrap_or_else(|| panic!("unparseable shell reply: {}", r.content))
        .to_string()
}

/// Run a command through the FOREGROUND `shell` tool and return its output.
///
/// NOT a real host process: a foreground `shell` call materializes kaish and
/// runs the command through kaish's own interpreter, whose filesystem
/// builtins (`echo`, `cat`, `ln`, `mkdir`, `tee`, …) all route through
/// kaijutsu's VFS/`MountTable` — the exact same read-only/writable mount
/// policy the file-tool tests are probing. That makes this the right choice
/// for `kj` control-plane commands (`kj context set …`) and for VFS-mediated
/// reads that should reflect the real file either way, but the WRONG choice
/// for seeding a file meant to exist independently of VFS policy: writing
/// through a read-only mount here fails for the same reason the test wants
/// to prove, before the test even starts. Use [`run_real`] for that.
pub async fn run_shell(kernel: &KernelHandle, command: &str) -> String {
    let r = kernel
        .call_mcp_tool("shell", &serde_json::json!({"command": command, "background": false}))
        .await
        .expect("call shell tool");
    assert!(
        !r.is_error,
        "setup command failed: `{command}` -> {}",
        r.content
    );
    r.content
}

/// Run a command as a REAL host process (`/bin/sh -c <command>`) and block
/// until it exits, returning its accumulated output.
///
/// `background: true` on the `shell` tool bypasses kaish's materialization
/// entirely (`background_exec.rs` module docs) and spawns a real host
/// subprocess — the only way to touch the filesystem independently of
/// kaijutsu's VFS/mount policy from this harness. Used to seed files whose
/// existence must be independent of the mount under test (e.g. a file under
/// the read-only root mount that the VFS itself could never have written)
/// and to verify real on-disk bytes afterward. Panics (loudly, with the
/// captured output) if the command doesn't exit zero within 15s — a broken
/// setup step must never be silently accepted.
pub async fn run_real(kernel: &KernelHandle, command: &str) -> String {
    let bg_id = start_bg(kernel, command).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status_line = loop {
        let r = kernel
            .call_mcp_tool("list_background_processes", &serde_json::json!({}))
            .await
            .expect("list_background_processes");
        if let Some(line) = r.content.lines().find(|l| l.starts_with(&bg_id))
            && line.contains("[exited")
        {
            break line.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "real host command didn't exit within 15s: `{command}`"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let out = kernel
        .call_mcp_tool(
            "read_background_output",
            &serde_json::json!({"id": bg_id, "offset": 0}),
        )
        .await
        .expect("read_background_output");
    assert!(
        status_line.contains("[exited=0]"),
        "real host setup command failed: `{command}` -> {status_line}\noutput: {}",
        out.content
    );
    out.content
}

/// The reliable PID source: `list_background_processes` text lines are
/// "<bg-id> pid=<PID> [<status>] <command>".
pub async fn bg_pid(kernel: &KernelHandle, bg_id: &str) -> u32 {
    let r = kernel
        .call_mcp_tool("list_background_processes", &serde_json::json!({}))
        .await
        .expect("list_background_processes");
    for line in r.content.lines() {
        if line.starts_with(bg_id) {
            if let Some(pid) = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix("pid="))
                .and_then(|p| p.parse().ok())
            {
                return pid;
            }
        }
    }
    panic!("no pid for {bg_id} in: {}", r.content);
}

// ---------------------------------------------------------------------------
// /proc scanning — the whole point of the empty PID namespace. Used only by
// the process-lifecycle suite (isolation.rs); harmless to have available here.

pub fn proc_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .collect()
}

pub fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split('\0')
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

pub fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

pub fn pids_matching(pattern: &str) -> Vec<(u32, String)> {
    proc_pids()
        .into_iter()
        .map(|p| (p, cmdline(p)))
        .filter(|(_, c)| c.contains(pattern))
        .collect()
}

/// Count ESTABLISHED TCP connections involving `port` (hex-matched against
/// /proc/net/tcp{,6}). Lets the disconnect test *observe* the transport
/// actually closing instead of trusting a client-side drop to imply it.
pub fn established_conns(port: u16) -> usize {
    let needle = format!(":{port:04X}");
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|s| s.lines().skip(1).map(str::to_string).collect::<Vec<_>>())
        .filter(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // fields: sl local_address rem_address st ...  st 01 = ESTABLISHED
            f.len() > 3 && (f[1].ends_with(&needle) || f[2].ends_with(&needle)) && f[3] == "01"
        })
        .count()
}

pub fn wait_gone(pid: u32, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !pid_alive(pid)
}

/// The free invariant: after teardown, this namespace holds only PID 1
/// (catatonit), ourselves, and whatever the test explicitly allows.
pub fn assert_no_survivors(allowed: &[u32]) {
    let me = std::process::id();
    let survivors: Vec<(u32, String)> = proc_pids()
        .into_iter()
        .filter(|p| *p != 1 && *p != me && !allowed.contains(p))
        .map(|p| (p, cmdline(p)))
        .collect();
    assert!(
        survivors.is_empty(),
        "processes survived teardown: {survivors:?}"
    );
}

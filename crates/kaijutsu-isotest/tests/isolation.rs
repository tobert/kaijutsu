//! Process-lifecycle isolation tests. Run via `contrib/isotest`.
//!
//! Topology inside the container: catatonit (podman --init) is PID 1, this
//! binary spawns `kaijutsu-server` as a real child on a fresh $HOME, connects
//! over loopback SSH, starts background jobs through the `shell` tool, then
//! kills the server for real and asserts on `/proc`. The PID namespace
//! started empty, so any survivor is ours and `assert_no_survivors` is a
//! provable invariant, not a heuristic.
//!
//! Honest limits: PDEATHSIG fires when the spawning *thread* dies. `kill -9`
//! takes every thread with it, so these tests cover the real dev-loop
//! scenario (runner restart, OOM kill). The inverse footgun — a healthy
//! kernel whose spawning thread exits early, spuriously killing a job — is
//! exercised only by `bg_job_survives_client_disconnect` below, which pins
//! the *documented* lifetime contract.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_client::{KeySource, RpcClient, SshConfig, connect_ssh};
use kaijutsu_client::rpc::KernelHandle;
use russh::keys::{Algorithm, PrivateKey};
use serde_json::json;

// ---------------------------------------------------------------------------
// gate + harness plumbing

/// Self-skip unless we're inside the contrib/isotest container. Keeps a bare
/// host `cargo test` fast, and keeps this suite from kill -9'ing anything on
/// a machine with a live kernel.
macro_rules! isotest_gate {
    () => {
        if std::env::var("KAIJUTSU_ISOTEST").as_deref() != Ok("1") {
            eprintln!("skipped: run via contrib/isotest (KAIJUTSU_ISOTEST=1)");
            return;
        }
    };
}

/// Every credential this suite mints carries this label — in the auth.db
/// nick, the pubkey comment, and the SSH username — so nothing it creates
/// can be mistaken for a durable identity.
const EPHEMERAL_LABEL: &str = "isotest-ephemeral";

fn server_bin() -> PathBuf {
    PathBuf::from(
        std::env::var("KAIJUTSU_SERVER_BIN")
            .expect("KAIJUTSU_SERVER_BIN is set by contrib/isotest"),
    )
}

/// capnp-rpc types are !Send: every test body runs on a current-thread
/// runtime inside a LocalSet, same as kaijutsu-server/tests/common.
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
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
struct TestKernel {
    child: Child,
    home: PathBuf,
    port: u16,
    key: Arc<PrivateKey>,
}

impl TestKernel {
    fn boot(name: &str, port: u16) -> Self {
        let home = std::env::temp_dir().join(format!("isotest-{name}"));
        Self::boot_at(home, port)
    }

    /// Boot on an explicit $HOME — the restart test reuses one across boots.
    fn boot_at(home: PathBuf, port: u16) -> Self {
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
    async fn connect_client(&self, key_source: KeySource) -> RpcClient {
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
    async fn connect(&self) -> KernelHandle {
        let client = self
            .connect_client(KeySource::InMemory(self.key.clone()))
            .await;
        let (kernel, _id) = client.bind_kernel().await.expect("bind_kernel");
        // Leak the RpcClient so its connection outlives this call: dropping
        // it would tear down the transport under the KernelHandle.
        std::mem::forget(client);
        kernel
    }

    fn sigkill(&mut self) {
        self.child.kill().expect("SIGKILL server");
        self.child.wait().expect("reap server");
    }

    fn sigterm_and_wait(&mut self) {
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

// ---------------------------------------------------------------------------
// /proc scanning — the whole point of the empty PID namespace

fn proc_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .collect()
}

fn cmdline(pid: u32) -> String {
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

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn pids_matching(pattern: &str) -> Vec<(u32, String)> {
    proc_pids()
        .into_iter()
        .map(|p| (p, cmdline(p)))
        .filter(|(_, c)| c.contains(pattern))
        .collect()
}

/// Count ESTABLISHED TCP connections involving `port` (hex-matched against
/// /proc/net/tcp{,6}). Lets the disconnect test *observe* the transport
/// actually closing instead of trusting a client-side drop to imply it.
fn established_conns(port: u16) -> usize {
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

fn wait_gone(pid: u32, grace: Duration) -> bool {
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
fn assert_no_survivors(allowed: &[u32]) {
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

// ---------------------------------------------------------------------------
// driving the kernel

/// Join the genesis ROOT context (a director: exec + facade:shell out of the
/// box) and return the handle ready for tool calls.
async fn join_root(kernel: &KernelHandle) {
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
async fn start_bg(kernel: &KernelHandle, command: &str) -> String {
    let r = kernel
        .call_mcp_tool("shell", &json!({"command": command, "background": true}))
        .await
        .expect("call shell tool");
    assert!(!r.is_error, "shell(background) errored: {}", r.content);
    r.content
        .strip_prefix("started background process ")
        .and_then(|rest| rest.split([';', ' ']).next())
        .unwrap_or_else(|| panic!("unparseable shell reply: {}", r.content))
        .to_string()
}

/// The reliable PID source: `list_background_processes` text lines are
/// "<bg-id> pid=<PID> [<status>] <command>".
async fn bg_pid(kernel: &KernelHandle, bg_id: &str) -> u32 {
    let r = kernel
        .call_mcp_tool("list_background_processes", &json!({}))
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
// tests

/// The PDEATHSIG proof: SIGKILL the kernel mid-job; the job must die with it.
/// This is the `kill -9` / kaijutsu-runner.sh restart scenario the bespoke
/// background_exec path calls its deciding factor.
#[test]
fn orphan_guard_on_sigkill() {
    isotest_gate!();
    run_local(async {
        let mut tk = TestKernel::boot("sigkill", 2301);
        let kernel = tk.connect().await;
        join_root(&kernel).await;

        let bg = start_bg(&kernel, "sleep 300").await;
        let job = bg_pid(&kernel, &bg).await;
        assert!(pid_alive(job), "background job should be running");

        // The client connection stays open across the kill: the job's
        // PDEATHSIG is bound to the server's per-connection thread, so
        // dropping the client first would kill the job for the wrong
        // reason and pass vacuously.
        tk.sigkill();

        assert!(
            wait_gone(job, Duration::from_secs(5)),
            "orphan guard failed: job {job} ({}) outlived SIGKILLed kernel",
            cmdline(job)
        );
        assert_no_survivors(&[]);
    });
}

/// Same guard under default-action SIGTERM — the polite half of the restart
/// story. If the server someday handles SIGTERM gracefully, the registry
/// kill path must produce the same outcome; either way nothing survives.
#[test]
fn orphan_guard_on_sigterm() {
    isotest_gate!();
    run_local(async {
        let mut tk = TestKernel::boot("sigterm", 2302);
        let kernel = tk.connect().await;
        join_root(&kernel).await;

        let bg = start_bg(&kernel, "sleep 300").await;
        let job = bg_pid(&kernel, &bg).await;
        assert!(pid_alive(job));

        tk.sigterm_and_wait();

        assert!(
            wait_gone(job, Duration::from_secs(5)),
            "job {job} outlived SIGTERMed kernel"
        );
        assert_no_survivors(&[]);
    });
}

/// kill_background_process must take the whole setpgid process group, not
/// just the direct sh child — pins background_exec.rs's process-group kill.
#[test]
fn kill_reaps_whole_process_tree() {
    isotest_gate!();
    run_local(async {
        let mut tk = TestKernel::boot("treekill", 2303);
        let kernel = tk.connect().await;
        join_root(&kernel).await;

        let bg = start_bg(&kernel, "sh -c 'sleep 301 & sleep 302 & wait'").await;
        let job = bg_pid(&kernel, &bg).await;

        // Wait for the grandchildren to exist before killing.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids_matching("sleep 30").len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let tree = pids_matching("sleep 30");
        assert!(tree.len() >= 2, "grandchildren never appeared: {tree:?}");

        let r = kernel
            .call_mcp_tool("kill_background_process", &json!({"id": bg}))
            .await
            .expect("kill_background_process");
        assert!(!r.is_error, "kill errored: {}", r.content);

        assert!(wait_gone(job, Duration::from_secs(5)), "sh leader survived");
        for (pid, cmd) in tree {
            assert!(
                wait_gone(pid, Duration::from_secs(5)),
                "group kill missed grandchild {pid} ({cmd})"
            );
        }

        tk.sigterm_and_wait();
        assert_no_survivors(&[]);
    });
}

/// The dev-loop restart: kill -9, boot again on the same $HOME. No process
/// survives the generation boundary, and the new kernel's registry must not
/// claim the dead job is still running.
#[test]
fn restart_leaves_no_orphans_and_registry_stays_honest() {
    isotest_gate!();
    run_local(async {
        let home = std::env::temp_dir().join("isotest-restart");
        let mut tk = TestKernel::boot_at(home.clone(), 2304);
        let kernel = tk.connect().await;
        join_root(&kernel).await;

        let bg = start_bg(&kernel, "sleep 300").await;
        let job = bg_pid(&kernel, &bg).await;
        assert!(pid_alive(job));

        tk.sigkill();
        assert!(wait_gone(job, Duration::from_secs(5)));

        // Same $HOME (same kernel.db, same auth.db), fresh port to dodge
        // listener TIME_WAIT flakes.
        let mut tk2 = TestKernel::boot_at(home, 2305);
        let kernel2 = tk2.connect().await;
        join_root(&kernel2).await;

        let r = kernel2
            .call_mcp_tool("list_background_processes", &json!({}))
            .await
            .expect("list_background_processes after restart");
        for line in r.content.lines() {
            assert!(
                !line.contains("[running]"),
                "registry lies after restart — claims running: {line}"
            );
        }
        assert!(
            pids_matching("sleep 300").is_empty(),
            "a pre-restart job is still alive"
        );

        tk2.sigterm_and_wait();
        assert_no_survivors(&[]);
    });
}

/// Pins the DOCUMENTED background contract (background_exec.rs module docs):
/// a job "survives across shell tool invocations" and dies only with the
/// context, the kernel, or an explicit kill — NOT with the client that
/// happened to start it. Code reading suggests PDEATHSIG is bound to the
/// server's per-connection thread, which would break this contract on
/// client disconnect; this test is the empirical check.
#[test]
fn bg_job_survives_client_disconnect() {
    isotest_gate!();
    run_local(async {
        let mut tk = TestKernel::boot("disconnect", 2306);

        let job = {
            // Scope-bound connection: unlike connect() (which leaks the
            // RpcClient on purpose), this one drops — and disconnects — at
            // the end of this block.
            let client = tk
                .connect_client(KeySource::InMemory(tk.key.clone()))
                .await;
            let (kernel, _id) = client.bind_kernel().await.expect("bind");
            join_root(&kernel).await;
            let bg = start_bg(&kernel, "sleep 300").await;
            let job = bg_pid(&kernel, &bg).await;
            assert!(pid_alive(job));
            job
            // client + kernel drop here → SSH connection closes → the
            // server's per-connection thread unwinds.
        };

        // Don't trust the drop: wait until the transport is observably gone
        // (no ESTABLISHED connection to the server port), then give the
        // server a beat to run any disconnect-path cleanup it might have.
        let deadline = Instant::now() + Duration::from_secs(10);
        while established_conns(tk.port) > 0 {
            assert!(
                Instant::now() < deadline,
                "client drop never closed the SSH connection — test would be vacuous"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;

        let alive = pid_alive(job);
        // Clean up regardless of verdict so the invariant check is honest.
        tk.sigterm_and_wait();
        wait_gone(job, Duration::from_secs(5));
        assert_no_survivors(&[]);

        assert!(
            alive,
            "documented contract broken: background job died when its \
             starting client disconnected (PDEATHSIG bound to the server's \
             per-connection thread, not the kernel's lifetime)"
        );
    });
}

/// The production auth lane: kaijutsu-mcp, kaijutsu-acp, and every field
/// client authenticate via `KeySource::Agent`, which the other tests never
/// touch. Run a real ssh-agent inside the namespace, hand it the labeled
/// ephemeral key (the private key goes into agent memory only — never
/// disk), and drive a background job end-to-end through agent auth. The
/// agent process itself is subject to `assert_no_survivors`.
#[test]
fn agent_auth_production_path() {
    isotest_gate!();
    run_local(async {
        let mut tk = TestKernel::boot("agent", 2307);

        let sock = tk.home.join("isotest-ephemeral-agent.sock");
        let mut agent = Command::new("ssh-agent")
            .args(["-D", "-a", sock.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent (openssh comes from Containerfile.isotest)");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !sock.exists() {
            assert!(
                Instant::now() < deadline,
                "ssh-agent socket never appeared at {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let mut ac = russh::keys::agent::client::AgentClient::connect_uds(&sock)
            .await
            .expect("connect to ssh-agent");
        ac.add_identity(&tk.key, &[])
            .await
            .expect("add ephemeral key to agent");

        // auth_with_agent discovers the socket via SSH_AUTH_SOCK. The
        // runner forces --test-threads=1, so mutating process env is safe.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &sock) };

        let client = tk.connect_client(KeySource::Agent).await;
        let (kernel, _id) = client.bind_kernel().await.expect("bind via agent auth");
        join_root(&kernel).await;

        let bg = start_bg(&kernel, "sleep 300").await;
        let job = bg_pid(&kernel, &bg).await;
        assert!(pid_alive(job), "job started through agent-auth session");

        tk.sigterm_and_wait();
        assert!(wait_gone(job, Duration::from_secs(5)));

        agent.kill().expect("kill ssh-agent");
        agent.wait().expect("reap ssh-agent");
        assert_no_survivors(&[]);
        drop(client);
    });
}

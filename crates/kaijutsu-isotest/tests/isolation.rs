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

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

mod common;
use common::{
    assert_no_survivors, bg_pid, cmdline, established_conns, join_root, pid_alive, pids_matching,
    run_local, skip_unless_isotest, start_bg, wait_gone, TestKernel,
};

use kaijutsu_client::KeySource;

// ---------------------------------------------------------------------------
// tests

/// The PDEATHSIG proof: SIGKILL the kernel mid-job; the job must die with it.
/// This is the `kill -9` / kaijutsu-runner.sh restart scenario the bespoke
/// background_exec path calls its deciding factor.
#[test]
fn orphan_guard_on_sigkill() {
    if skip_unless_isotest() { return; }
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
    if skip_unless_isotest() { return; }
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
    if skip_unless_isotest() { return; }
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
    if skip_unless_isotest() { return; }
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
    if skip_unless_isotest() { return; }
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
    if skip_unless_isotest() { return; }
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

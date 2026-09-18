//! Process-lifecycle isolation tests. Run via `contrib/isotest`.
//!
//! Topology inside the container: catatonit (podman --init) is PID 1, this
//! binary spawns `kaijutsu-server` as a real child on a fresh $HOME, connects
//! over loopback SSH, submits background work through the retained shell RPC,
//! then kills the server for real and asserts on `/proc`. The PID namespace
//! starts empty, so any survivor belongs to this test.
//!
//! On Linux, PDEATHSIG ties a direct child to the OS thread that spawned it.
//! `kill -9` ends every server thread, so it covers abrupt server loss. It
//! does not by itself cover arbitrary grandchildren; the cancellation test
//! separately checks process-group cleanup. The disconnect test checks that
//! completion of a connection task does not end context-owned work.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;
use common::{
    assert_no_survivors, bg_pid, cancel_bg, cmdline, established_conns, join_root, pid_alive, pids_matching,
    run_local, skip_unless_isotest, start_bg, wait_gone, TestKernel,
};

use kaijutsu_client::KeySource;

// ---------------------------------------------------------------------------
// tests

/// The PDEATHSIG proof: SIGKILL the kernel mid-job; the job must die with it.
/// External children use kaish parent-death handling.
#[test]
fn orphan_guard_on_sigkill() {
    if skip_unless_isotest() { return; }
    run_local(async {
        let mut tk = TestKernel::boot("sigkill", 2301);
        let kernel = tk.connect().await;
        let context = join_root(&kernel).await;

        let bg = start_bg(&kernel, context, "/usr/bin/sleep 300").await;
        let job = bg_pid(&kernel, context, &bg, "/usr/bin/sleep 300").await;
        assert!(pid_alive(job), "background job should be running");

        // Keep the submission connection open so this test changes only the
        // server process lifetime. Disconnect behavior has its own test.
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
        let context = join_root(&kernel).await;

        let bg = start_bg(&kernel, context, "/usr/bin/sleep 300").await;
        let job = bg_pid(&kernel, context, &bg, "/usr/bin/sleep 300").await;
        assert!(pid_alive(job));

        tk.sigterm_and_wait();

        assert!(
            wait_gone(job, Duration::from_secs(5)),
            "job {job} outlived SIGTERMed kernel"
        );
        assert_no_survivors(&[]);
    });
}

/// Cancellation must reach the external command and its child process.
#[test]
fn kill_reaps_whole_process_tree() {
    if skip_unless_isotest() { return; }
    run_local(async {
        let mut tk = TestKernel::boot("treekill", 2303);
        let kernel = tk.connect().await;
        let context = join_root(&kernel).await;

        let bg = start_bg(&kernel, context, "/usr/bin/timeout 301 /usr/bin/sleep 302").await;
        let job = bg_pid(
            &kernel,
            context,
            &bg,
            "/usr/bin/timeout 301 /usr/bin/sleep 302",
        ).await;

        // Wait for the grandchildren to exist before killing.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids_matching("sleep 30").len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let tree = pids_matching("sleep 30");
        assert!(tree.len() >= 2, "grandchildren never appeared: {tree:?}");

        cancel_bg(&kernel, context, &bg).await;

        assert!(wait_gone(job, Duration::from_secs(5)), "timeout leader survived");
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
        let context = join_root(&kernel).await;

        let bg = start_bg(&kernel, context, "/usr/bin/sleep 300").await;
        let job = bg_pid(&kernel, context, &bg, "/usr/bin/sleep 300").await;
        assert!(pid_alive(job));

        tk.sigkill();
        assert!(wait_gone(job, Duration::from_secs(5)));

        // Same $HOME (same kernel.db, same auth.db), fresh port to dodge
        // listener TIME_WAIT flakes.
        let mut tk2 = TestKernel::boot_at(home, 2305);
        let kernel2 = tk2.connect().await;
        let context2 = join_root(&kernel2).await;

        let argv = ["wait", "--operation", bg.as_str(), "--timeout", "0"]
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        let operation = kernel2.execute_kj_quiet(context2, &argv).await
            .expect("read operation after restart");
        assert_eq!(operation.exit_code, 0, "kj wait failed: {}", operation.stderr);
        let state = operation.data.expect("operation state after restart");
        assert!(
            state.get("status").and_then(|value| value.as_str()) != Some("running"),
            "registry lies after restart — claims running: {state}"
        );
        assert!(
            pids_matching("/usr/bin/sleep 300").is_empty(),
            "a pre-restart job is still alive"
        );

        tk2.sigterm_and_wait();
        assert_no_survivors(&[]);
    });
}

/// Jobs belong to the kernel context and survive a client disconnect.
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
            let context = join_root(&kernel).await;
            let bg = start_bg(&kernel, context, "/usr/bin/sleep 300").await;
            let job = bg_pid(&kernel, context, &bg, "/usr/bin/sleep 300").await;
            assert!(pid_alive(job));
            job
            // client + kernel drop here → SSH connection closes → the
            // server's connection task finishes.
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
             starting client disconnected"
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
        let context = join_root(&kernel).await;

        let bg = start_bg(&kernel, context, "/usr/bin/sleep 300").await;
        let job = bg_pid(&kernel, context, &bg, "/usr/bin/sleep 300").await;
        assert!(pid_alive(job), "job started through agent-auth session");

        tk.sigterm_and_wait();
        assert!(wait_gone(job, Duration::from_secs(5)));

        agent.kill().expect("kill ssh-agent");
        agent.wait().expect("reap ssh-agent");
        assert_no_survivors(&[]);
        drop(client);
    });
}

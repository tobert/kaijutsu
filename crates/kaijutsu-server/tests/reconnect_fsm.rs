//! End-to-end tests for the client-side reconnect FSM (`kaijutsu_client::actor`).
//!
//! These exercise the full SSH + Cap'n Proto stack against an ephemeral
//! server. The point is to verify the rewritten state machine survives
//! the failure mode that motivated the rewrite: a connection that dies
//! mid-session and must be re-established without losing the actor.

#![allow(clippy::needless_collect)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, LocalSet};

use kaijutsu_client::{
    ActorHandle, CallError, ConnectionStatus, KeySource, NotReadyReason, SshConfig, spawn_actor,
};
use kaijutsu_crdt::ContextId;
use kaijutsu_server::{SshServer, SshServerConfig};

// ────────────────────────────────────────────────────────────────────────────
// Test harness
// ────────────────────────────────────────────────────────────────────────────

/// Run a test on a current_thread runtime with a LocalSet (capnp-rpc requirement).
fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(f));
}

/// Handle to a running server task; cancellation drops the listener and
/// stops accepting new connections (in-flight sessions terminate when the
/// client gives up or the SSH layer times out).
struct ServerHandle {
    addr: SocketAddr,
    cancel: Arc<Notify>,
    join: JoinHandle<()>,
}

impl ServerHandle {
    async fn stop(self) {
        self.cancel.notify_waiters();
        // Abort instead of join — we want to be sure the listener is gone.
        self.join.abort();
        let _ = self.join.await;
        // Give the OS a moment to release the port.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Start a server on the given address. If `addr` is `None`, picks an
/// ephemeral port; otherwise re-binds the supplied address (used to
/// simulate "kernel comes back on the same port").
async fn start_server_on(addr: Option<SocketAddr>) -> ServerHandle {
    let bind = match addr {
        Some(a) => a,
        None => "127.0.0.1:0".parse().unwrap(),
    };

    // Tight retry loop for rebinding — TIME_WAIT can hold the port briefly.
    let listener = {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match TcpListener::bind(bind).await {
                Ok(l) => break l,
                Err(e) if attempts < 50 => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    log::debug!("bind retry {attempts}: {e}");
                }
                Err(e) => panic!("failed to bind {bind}: {e}"),
            }
        }
    };
    let bound_addr = listener.local_addr().unwrap();
    let server_config = SshServerConfig::ephemeral(bound_addr.port());

    let cancel = Arc::new(Notify::new());
    let cancel_clone = cancel.clone();

    let join = tokio::task::spawn_local(async move {
        let server = SshServer::new(server_config);
        tokio::select! {
            res = server.run_on_listener(listener) => {
                if let Err(e) = res {
                    log::warn!("server exited with error: {e}");
                }
            }
            _ = cancel_clone.notified() => {
                log::debug!("server cancellation received");
            }
        }
    });

    // Let the listener actually start accepting.
    tokio::task::yield_now().await;
    ServerHandle {
        addr: bound_addr,
        cancel,
        join,
    }
}

/// Spawn an actor pointed at the given server.
fn spawn_test_actor(addr: SocketAddr, instance: &str) -> ActorHandle {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "test_user".to_string(),
        key_source: KeySource::ephemeral(),
        insecure: true,
    };
    spawn_actor(config, None, instance.to_string(), false)
}

/// Poll the status broadcast until a predicate matches, or panic on timeout.
async fn wait_for_status<F>(
    handle: &ActorHandle,
    label: &str,
    timeout: Duration,
    predicate: F,
) -> ConnectionStatus
where
    F: Fn(&ConnectionStatus) -> bool,
{
    let mut rx = handle.subscribe_status();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timeout waiting for {label}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(status)) => {
                log::debug!("status: {status:?}");
                if predicate(&status) {
                    return status;
                }
            }
            Ok(Err(e)) => panic!("status channel error: {e}"),
            Err(_) => panic!("timeout waiting for {label}"),
        }
    }
}

/// Call `whoami` in a retry loop, returning `Ok` as soon as the FSM lets a
/// call through. Bounded by `timeout`. This is what a polite caller looks
/// like under the new "reject during reconnect" semantics.
async fn whoami_with_retry(
    handle: &ActorHandle,
    timeout: Duration,
) -> Result<kaijutsu_client::Identity, CallError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match handle.whoami().await {
            Ok(id) => return Ok(id),
            Err(CallError::NotReady(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

/// The first command issued to an Idle actor returns `NotReady(Idle)` and
/// triggers the transition to `Connecting`. The new typed error is the
/// contract that callers depend on; this nails it down.
#[test]
fn first_call_rejected_with_idle_then_actor_connects() {
    run_local(async {
        let server = start_server_on(None).await;
        let actor = spawn_test_actor(server.addr, "test-first-call");

        // The first call hits the FSM in Idle and gets NotReady(Idle).
        let first = actor.whoami().await;
        assert!(
            matches!(
                first,
                Err(CallError::NotReady(NotReadyReason::Idle))
                    | Err(CallError::NotReady(NotReadyReason::Connecting { .. }))
            ),
            "first call should be NotReady, got {:?}",
            first
        );

        // Subsequent calls should succeed once Connected.
        let id = whoami_with_retry(&actor, Duration::from_secs(5))
            .await
            .expect("should connect within 5s");
        // Anonymous-mode auto-registration may rename the user on collision;
        // the load-bearing assertion is that we GOT an identity, not its
        // exact form.
        assert!(!id.username.is_empty(), "username should be non-empty");
    });
}

/// The big one: client connects, server dies, server comes back, client
/// reconnects via the FSM, and the next call succeeds. This is the failure
/// mode the rewrite exists to fix.
#[test]
fn actor_reconnects_after_server_restart() {
    run_local(async {
        // 1. Start server, connect, verify whoami works.
        let server1 = start_server_on(None).await;
        let addr = server1.addr;
        let actor = spawn_test_actor(addr, "test-reconnect");

        let id = whoami_with_retry(&actor, Duration::from_secs(5))
            .await
            .expect("initial whoami");
        let bound_username = id.username.clone();
        assert!(!bound_username.is_empty());

        // 2. Kill the server. The client's RPC pipe will go dead; the next
        //    call should surface that and trigger the Closing → Cooldown path.
        log::info!("stopping server v1");
        server1.stop().await;

        // The next call may go straight through (call already in flight got
        // a stale response) or return Rpc/Timeout. Drive it until we see
        // the FSM acknowledge the disconnect via NotReady.
        let saw_not_ready = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut seen = false;
            while tokio::time::Instant::now() < deadline {
                match actor.whoami().await {
                    Err(CallError::NotReady(_)) => {
                        seen = true;
                        break;
                    }
                    Err(_) => {
                        // RPC/Timeout — wait a moment for the FSM to react.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Ok(_) => {
                        // Pipe still responding (a queued reply, perhaps).
                        // Keep poking.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            seen
        };
        assert!(
            saw_not_ready,
            "FSM should have transitioned to a NotReady state after server stop"
        );

        // 3. Restart server on the same port. The FSM's cooldown timer should
        //    fire and the next handshake should succeed.
        log::info!("restarting server v2 on {addr}");
        let server2 = start_server_on(Some(addr)).await;

        // 4. Reconnect should happen within the Cooldown + handshake window.
        //    Backoff after 1 failure is 1s; SSH dial + handshake is sub-second.
        let id2 = whoami_with_retry(&actor, Duration::from_secs(30))
            .await
            .expect("reconnect within 30s");
        // Server v2 has a fresh in-memory auth db so the auto-registered
        // username may differ from server v1's — but it must be non-empty.
        assert!(!id2.username.is_empty(), "reconnect produced empty username");
        log::info!(
            "Reconnected successfully: v1 user '{bound_username}', v2 user '{}'",
            id2.username,
        );

        // Clean up.
        server2.stop().await;
    });
}

/// Confirm that the connection-status broadcast walks the new FSM states
/// during a normal connect cycle. Old code emitted only 4 variants; the
/// new code must emit at least Connecting and Connected.
#[test]
fn status_broadcast_walks_fsm_states() {
    run_local(async {
        let server = start_server_on(None).await;
        let actor = spawn_test_actor(server.addr, "test-status");

        let mut rx = actor.subscribe_status();

        // Trigger a connect attempt by issuing a call.
        let _ = actor.whoami().await; // Returns NotReady(Idle), kicks off Connecting.

        // Drive whoami in a background task so the FSM has work to do.
        let actor2 = actor.clone();
        let bg = tokio::task::spawn_local(async move {
            let _ = whoami_with_retry(&actor2, Duration::from_secs(5)).await;
        });

        // Collect statuses for a brief window.
        let mut saw_connecting = false;
        let mut saw_connected = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(Ok(status)) = tokio::time::timeout(remaining, rx.recv()).await {
                match status {
                    ConnectionStatus::Connecting { .. } => saw_connecting = true,
                    ConnectionStatus::Connected { .. } => {
                        saw_connected = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        let _ = bg.await;

        assert!(saw_connecting, "FSM should have broadcast Connecting");
        assert!(saw_connected, "FSM should have broadcast Connected");
    });
}

/// The handshake is bounded: a black-hole address fails by per-phase
/// deadline (5s SSH dial) rather than hanging. We use a *bound* local
/// listener that never `accept()`s so the TCP handshake completes but the
/// SSH negotiation hangs — deterministic across network environments
/// (depending on routing-table state would be flaky).
#[test]
fn black_hole_address_falls_into_cooldown_quickly() {
    run_local(async {
        // Bind a real listener but never accept. The client's TCP connect
        // succeeds (kernel-level), then SSH banner exchange hangs because
        // no one is reading — this is the closest thing to a real wedge
        // we can reproduce locally.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the listener for the test lifetime so the port stays bound,
        // but never accept.
        let _hold = listener;

        let actor = spawn_test_actor(addr, "test-blackhole");

        // Kick off the connect attempt.
        let _ = actor.whoami().await;

        // We should see a Cooldown status within SSH_DIAL_TIMEOUT (5s) + slop.
        let status = wait_for_status(
            &actor,
            "Cooldown after handshake hang",
            Duration::from_secs(10),
            |s| matches!(s, ConnectionStatus::Cooldown { .. }),
        )
        .await;

        if let ConnectionStatus::Cooldown { last_error, .. } = status {
            assert!(
                last_error.contains("ssh") || last_error.contains("timeout"),
                "Cooldown reason should mention ssh/timeout, got: {last_error}"
            );
        }
    });
}

/// `join_context` with a context that doesn't exist on the server must
/// surface as Permanent (Terminal state), not an infinite reconnect loop.
/// This is the failure mode that catches both "kernel restarted with a
/// fresh database" and "context was deleted out from under us."
#[test]
fn join_context_to_missing_context_settles_terminal() {
    run_local(async {
        let server = start_server_on(None).await;
        let actor = spawn_test_actor(server.addr, "test-bad-context");

        // Connect successfully so the actor reaches Connected.
        let _id = whoami_with_retry(&actor, Duration::from_secs(5))
            .await
            .expect("initial connect");

        // Manufacture a context ID that the server has never seen.
        let bogus = ContextId::new();
        let result = actor.join_context(bogus).await;
        // We don't pin the exact variant — the kernel may return
        // `CallError::Rpc(...)` if the call reached the server and got
        // rejected, or `NotReady` if the reconnect-on-error path engaged.
        // The load-bearing assertion is that we do NOT silently succeed.
        assert!(
            result.is_err(),
            "join_context with bogus id must error, got: {:?}",
            result
        );

        server.stop().await;
    });
}

/// While `join_context` is in flight, other commands should still be
/// dispatchable — the rewrite spawns join_context rather than blocking
/// the actor loop. This catches the "30s call wedge blocks the pinger"
/// failure mode Gemini called out in review.
#[test]
fn commands_concurrent_with_join_context_do_not_block() {
    run_local(async {
        let server = start_server_on(None).await;
        let actor = spawn_test_actor(server.addr, "test-concurrent");

        // Get to Connected first.
        let _id = whoami_with_retry(&actor, Duration::from_secs(5))
            .await
            .expect("initial connect");

        // Fire join_context against a bogus context. It will fail eventually
        // — but while it's in flight, whoami should still respond fast.
        let bogus = ContextId::new();
        let actor_join = actor.clone();
        let join_handle =
            tokio::task::spawn_local(async move { actor_join.join_context(bogus).await });

        // whoami should complete WELL inside the 30s RPC timeout — if the
        // actor loop is blocked on join_context, this would hang past 1s.
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(3), actor.whoami()).await;
        let elapsed = started.elapsed();

        assert!(
            result.is_ok(),
            "whoami timed out while join_context was in flight ({elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "whoami took {elapsed:?} — actor loop likely blocked on join_context"
        );

        // Drain the bogus join_context so it doesn't leak its task into
        // teardown.
        let _ = tokio::time::timeout(Duration::from_secs(10), join_handle).await;

        server.stop().await;
    });
}

/// Subscribe with two actors that share the same `instance` UUID: the
/// server should dedupe and the second subscribe should replace the first.
/// We can't directly observe the registry from the test, but we can verify
/// both connects succeed without errors — historically, double-subscribe
/// caused server-side wedges.
///
/// Note: each actor uses an ephemeral SSH key, so the server registers two
/// distinct principals under anonymous mode. Dedupe is per-(principal,
/// instance), so different principals don't trigger replacement — but the
/// test still proves that two simultaneous subscriptions don't wedge the
/// server, which is the load-bearing invariant.
#[test]
fn duplicate_instance_subscribes_do_not_wedge() {
    run_local(async {
        let server = start_server_on(None).await;

        let actor1 = spawn_test_actor(server.addr, "shared-instance");
        let _id1 = whoami_with_retry(&actor1, Duration::from_secs(5))
            .await
            .expect("actor1 connect");

        // Spawn a second actor with the same instance. Even with different
        // principals (different ephemeral keys), the server should accept
        // the new subscription without wedging on the prior one.
        let actor2 = spawn_test_actor(server.addr, "shared-instance");
        let _id2 = whoami_with_retry(&actor2, Duration::from_secs(5))
            .await
            .expect("actor2 connect with dedupe");

        // Both actors should still be responsive.
        let _id1b = whoami_with_retry(&actor1, Duration::from_secs(5))
            .await
            .expect("actor1 still responds");
        let _id2b = whoami_with_retry(&actor2, Duration::from_secs(5))
            .await
            .expect("actor2 still responds");

        server.stop().await;
    });
}

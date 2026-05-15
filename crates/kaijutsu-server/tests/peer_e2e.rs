//! End-to-end test for the peer registry / drift navigation transport.
//!
//! Exercises the full pipeline:
//!
//!   client B → kernel.invoke_peer (capnp RPC over SSH)
//!   → kernel registry lookup → mpsc bridge
//!   → capnp PeerCommands.invoke callback
//!   → client A's PeerCommandsImpl → mpsc → handler task
//!   → oneshot reply traverses back the same chain.
//!
//! What this protects:
//!
//! * Full round-trip on the renamed Peer surface (attach/invoke).
//! * Re-attach with the same nick replaces the old invocation channel —
//!   the failure mode codified by commit 323ea2e and now also asserted at
//!   the kernel-registry level in `kaijutsu_kernel::peers::tests`.
//! * Disconnect-flavored error (no peer registered under that nick) maps
//!   to a clean RPC error rather than a hang.

mod common;
use common::*;

use kaijutsu_client::PeerConfig;

/// Spawn a worker that drains the peer invocation channel and applies
/// `handler` to each request, replying via the oneshot.
fn spawn_invocation_handler<F>(
    rx: std::sync::mpsc::Receiver<kaijutsu_client::PeerInvocation>,
    handler: F,
) -> std::thread::JoinHandle<()>
where
    F: Fn(&str, &[u8]) -> Result<Vec<u8>, String> + Send + 'static,
{
    std::thread::spawn(move || {
        while let Ok(invocation) = rx.recv() {
            let result = handler(&invocation.action, &invocation.params);
            let _ = invocation.reply.send(result);
        }
    })
}

#[test]
fn test_invoke_peer_round_trip() {
    run_local(async {
        let addr = start_server().await;

        // Client A — registers as a peer under nick "echo".
        let client_a = connect_client(addr).await;
        let (kernel_a, kernel_id) = client_a.bind_kernel().await.unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let _handler = spawn_invocation_handler(rx, |action, _params| {
            Ok(format!("echo: {action}").into_bytes())
        });

        let attach = kernel_a
            .attach_peer(
                &PeerConfig {
                    nick: "echo".to_string(),
                },
                tx,
            )
            .await
            .expect("attach_peer should succeed");
        assert_eq!(attach.nick, "echo");

        // Client B — separate connection to the same kernel; calls echo.
        let client_b = connect_client(addr).await;
        let (kernel_b, kernel_b_id) = client_b.bind_kernel().await.unwrap();
        assert_eq!(kernel_b_id, kernel_id, "shared kernel id should match");

        let response = kernel_b
            .invoke_peer("echo", "hello-peer", &[])
            .await
            .expect("invoke_peer should succeed");
        assert_eq!(response, b"echo: hello-peer");
    });
}

#[test]
fn test_invoke_unknown_peer_errors_cleanly() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.unwrap();

        let err = kernel
            .invoke_peer("nobody-home", "ping", &[])
            .await
            .expect_err("invoke_peer should fail for unknown peer");
        let msg = format!("{err}");
        assert!(
            msg.contains("nobody-home") && msg.to_lowercase().contains("peer"),
            "error should mention nick and peer concept; got: {msg}"
        );
    });
}

#[test]
fn test_reattach_replaces_invocation_channel() {
    run_local(async {
        let addr = start_server().await;

        // Client A registers as "kaijutsu-app" twice. First registration is
        // replaced; only the second handler should see invocations.
        let client_a = connect_client(addr).await;
        let (kernel_a, kernel_id) = client_a.bind_kernel().await.unwrap();

        let (tx_old, rx_old) = std::sync::mpsc::channel();
        let _stale = spawn_invocation_handler(rx_old, |action, _| {
            Ok(format!("stale: {action}").into_bytes())
        });
        kernel_a
            .attach_peer(
                &PeerConfig {
                    nick: "kaijutsu-app".to_string(),
                },
                tx_old,
            )
            .await
            .expect("first attach_peer");

        let (tx_new, rx_new) = std::sync::mpsc::channel();
        let _live = spawn_invocation_handler(rx_new, |action, _| {
            Ok(format!("live: {action}").into_bytes())
        });
        kernel_a
            .attach_peer(
                &PeerConfig {
                    nick: "kaijutsu-app".to_string(),
                },
                tx_new,
            )
            .await
            .expect("re-attach_peer with same nick");

        // Client B invokes; only the live handler should reply.
        let client_b = connect_client(addr).await;
        let (kernel_b, kernel_b_id) = client_b.bind_kernel().await.unwrap();
        assert_eq!(kernel_b_id, kernel_id, "shared kernel id should match");

        let response = kernel_b
            .invoke_peer("kaijutsu-app", "ping", &[])
            .await
            .expect("invoke_peer after reattach");
        assert_eq!(
            response, b"live: ping",
            "re-attach must route to the new invocation channel"
        );
    });
}

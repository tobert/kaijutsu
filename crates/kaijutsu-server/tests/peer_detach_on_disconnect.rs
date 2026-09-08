//! A connection that drops takes its peer registration with it.
//!
//! The peer-invoke bridge task lives on the connection's `LocalSet`, which is
//! dropped the moment the RPC session returns. Any cleanup written *after*
//! that task's loop never runs, so the registry entry outlives the process
//! that made it. The kernel then reports two instances for a node that has
//! one, and `kj audio devices` refuses the ambiguity forever. The fix that
//! this test guards is RAII: `ConnectionState::drop` detaches every peer the
//! connection attached, keyed by an attach token so a reconnect that already
//! replaced the entry is never clobbered.

mod common;
use common::*;

use kaijutsu_client::PeerConfig;
use std::time::Duration;

/// How long the kernel may take to notice a dropped connection and reap its
/// peer. Well above the LocalSet teardown, well below a keepalive window.
const REAP_WINDOW: Duration = Duration::from_secs(3);

async fn peers_named(kernel: &kaijutsu_server::SharedKernel, nick: &str) -> Vec<String> {
    kernel
        .kernel
        .list_peers()
        .await
        .into_iter()
        .filter(|p| p.nick == nick)
        .map(|p| p.instance)
        .collect()
}

async fn wait_until_gone(kernel: &kaijutsu_server::SharedKernel, nick: &str) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + REAP_WINDOW;
    loop {
        let left = peers_named(kernel, nick).await;
        if left.is_empty() || tokio::time::Instant::now() >= deadline {
            return left;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn a_dropped_connection_takes_its_peer_registration_with_it() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;

        let client = connect_client(addr).await;
        let (handle, _) = client.bind_kernel().await.unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        handle
            .attach_peer(
                &PeerConfig { nick: "audio/test".to_string(), instance: "one".to_string() },
                tx,
            )
            .await
            .expect("attach_peer should succeed");
        assert_eq!(peers_named(&kernel, "audio/test").await, vec!["one".to_string()]);

        // The daemon exits: its RPC system aborts and the stream closes.
        drop(handle);
        drop(client);

        let left = wait_until_gone(&kernel, "audio/test").await;
        assert!(
            left.is_empty(),
            "peer registration outlived its connection: audio/test still has {left:?}"
        );
    });
}

#[test]
fn a_restarted_node_leaves_exactly_one_registration() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;

        // First daemon process.
        let old = connect_client(addr).await;
        let (old_handle, _) = old.bind_kernel().await.unwrap();
        let (tx, _rx_old) = std::sync::mpsc::channel();
        old_handle
            .attach_peer(&PeerConfig { nick: "audio/node".to_string(), instance: "proc-a".to_string() }, tx)
            .await
            .unwrap();

        // It restarts: the new process connects with a fresh instance while
        // the old connection is still being torn down.
        let new = connect_client(addr).await;
        let (new_handle, _) = new.bind_kernel().await.unwrap();
        let (tx, _rx_new) = std::sync::mpsc::channel();
        new_handle
            .attach_peer(&PeerConfig { nick: "audio/node".to_string(), instance: "proc-b".to_string() }, tx)
            .await
            .unwrap();
        drop(old_handle);
        drop(old);

        // Poll until only the new process remains, or the window closes.
        let deadline = tokio::time::Instant::now() + REAP_WINDOW;
        let left = loop {
            let left = peers_named(&kernel, "audio/node").await;
            if left == vec!["proc-b".to_string()] || tokio::time::Instant::now() >= deadline {
                break left;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(left, vec!["proc-b".to_string()], "the old process's registration must go, the new one must stay");
    });
}

#[test]
fn a_reconnect_under_the_same_instance_survives_the_old_connections_drop() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;

        // Same process, same instance token: a network blip makes the actor
        // reconnect and re-attach before the kernel has dropped the old
        // connection.
        let old = connect_client(addr).await;
        let (old_handle, _) = old.bind_kernel().await.unwrap();
        let (tx, _rx_old) = std::sync::mpsc::channel();
        old_handle
            .attach_peer(&PeerConfig { nick: "audio/node".to_string(), instance: "same".to_string() }, tx)
            .await
            .unwrap();

        let new = connect_client(addr).await;
        let (new_handle, _) = new.bind_kernel().await.unwrap();
        let (tx, _rx_new) = std::sync::mpsc::channel();
        new_handle
            .attach_peer(&PeerConfig { nick: "audio/node".to_string(), instance: "same".to_string() }, tx)
            .await
            .unwrap();

        // The old connection dies AFTER the re-attach replaced its entry.
        drop(old_handle);
        drop(old);
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            peers_named(&kernel, "audio/node").await,
            vec!["same".to_string()],
            "the old connection's drop must not remove the re-attached entry"
        );
    });
}

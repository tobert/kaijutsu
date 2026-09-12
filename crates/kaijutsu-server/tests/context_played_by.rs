//! `contexts.played_by` at creation (`docs/character.md`, "A context is
//! played by a character" + slice 4's "`register_session` sets `played_by`").
//!
//! `register_session` (kaijutsu-mcp) always reaches a fresh context through
//! `createContext`, the same RPC every wire client uses — there is no
//! MCP-specific creation path. These tests exercise `create_context_inner`
//! (the RPC's shared recipe, `rpc.rs`) directly over the wire, the way
//! `context_origin_host.rs` proves `setContextOriginHost` without an MCP
//! process in the loop.
//!
//! `played_by` never rides the wire yet (no capnp field), so assertions read
//! the row straight out of `KernelDb` via the test-only kernel handle
//! (`start_server_with_kernel_handle`).

mod common;
use common::*;

use std::sync::Arc;

use kaijutsu_client::{KeySource, RpcClient, SshConfig};
use kaijutsu_server::{AuthDb, SshServer, SshServerConfig};
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

/// A normal wire-created context has a requester but no performer. Creating
/// a context does not silently make the connected human the model actor.
#[test]
fn create_context_leaves_played_by_unset() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kj.create_context("played-by-hajime-test").await.unwrap();

        let hajime = kernel
            .kernel_db
            .lock()
            .get_character_by_name(kaijutsu_kernel::seed_character::HAJIME)
            .unwrap()
            .expect("every kernel seeds hajime at cold start");

        let row = kernel
            .kernel_db
            .lock()
            .get_context(context_id)
            .unwrap()
            .expect("just-created context must have a row");

        assert_eq!(
            row.created_by, hajime.principal_id,
            "anonymous auto-register binds to hajime, so the connecting principal IS hajime"
        );
        assert_eq!(
            row.played_by,
            None,
            "ordinary context creation records its requester, not an inferred performer"
        );
    });
}

/// A principal with no character row (a real, authenticated, ordinarily
/// bound key — never anonymous, so it never falls back to hajime) leaves
/// `played_by` NULL. No character is minted and the request does not fail:
/// `played_by` is metadata, not authority, and a principal without a sheet
/// is a legitimate pre-character state.
#[test]
fn create_context_leaves_played_by_null_for_a_characterless_principal() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let auth_db_path = tmp.path().join("auth.db");
        let unmapped = PrincipalId::new();
        let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        {
            // Bind the key BEFORE the server opens the same file, so
            // `db.authenticate` finds it on the very first connection —
            // never the anonymous branch, which would bind to hajime
            // instead of proving the characterless case.
            let auth_db = AuthDb::open(&auth_db_path).unwrap();
            auth_db
                .add_key(unmapped, key.public_key(), Some("characterless-test"))
                .unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        config.auth_db_path = Some(auth_db_path);
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            let server = SshServer::new(config);
            if let Err(e) = server
                .run_on_listener_with_kernel_sink(listener, kernel_tx)
                .await
            {
                log::error!("Server error: {}", e);
            }
        });
        let kernel = kernel_rx.await.expect("server dropped the kernel handle");
        tokio::task::yield_now().await;

        let ssh_config = SshConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "characterless".to_string(),
            key_source: KeySource::InMemory(Arc::new(key)),
            insecure: true,
        };
        let mut ssh_client = kaijutsu_client::SshClient::new(ssh_config);
        let rpc_channel = ssh_client.connect().await.expect("SSH connect failed");
        let client = RpcClient::new(rpc_channel.into_stream())
            .await
            .expect("RPC client init failed");
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = kj.create_context("played-by-null-test").await.unwrap();

        assert!(
            kernel.kernel_db.lock().get_character(unmapped).unwrap().is_none(),
            "the fixture is only valid if this principal really has no character row"
        );

        let row = kernel
            .kernel_db
            .lock()
            .get_context(context_id)
            .unwrap()
            .expect("just-created context must have a row");
        assert_eq!(
            row.created_by, unmapped,
            "the bound key must authenticate straight to its own principal, not anonymous/hajime"
        );
        assert_eq!(
            row.played_by, None,
            "a characterless creating principal must leave played_by NULL, not fail"
        );
    });
}

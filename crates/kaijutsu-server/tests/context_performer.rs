//! `contexts.performer` at creation (`docs/character.md`, "A context is
//! played by a character" + slice 4's "`register_session` sets `performer`").
//!
//! `register_session` (kaijutsu-mcp) always reaches a fresh context through
//! `createContext`, the same RPC every wire client uses — there is no
//! MCP-specific creation path. These tests exercise `create_context_inner`
//! (the RPC's shared recipe, `rpc.rs`) directly over the wire, the way
//! `context_origin_host.rs` proves `setContextOriginHost` without an MCP
//! process in the loop.
//!
//! `performer` never rides the wire yet (no capnp field), so assertions read
//! the row straight out of `KernelDb` via the test-only kernel handle
//! (`start_server_with_kernel_handle`).

mod common;
use common::*;

use std::sync::Arc;

use kaijutsu_client::{KeySource, RpcClient, SshConfig};
use kaijutsu_server::{AuthDb, SshServer, SshServerConfig};
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

/// A wire-created context has a requester and a performer: the requester is
/// the connected human, the performer the character the create names. Creating
/// a context does not make the connected human the model actor.
#[test]
fn create_context_records_the_requester_and_the_named_performer() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();

        let context_id = create_context(&kj, "played-by-root-test").await.unwrap();

        let root = kernel
            .kernel_db
            .lock()
            .get_character_by_name(SshServerConfig::EPHEMERAL_ROOT)
            .unwrap()
            .expect("the ephemeral server's init step creates its root character");

        let row = kernel
            .kernel_db
            .lock()
            .get_context(context_id)
            .unwrap()
            .expect("just-created context must have a row");

        assert_eq!(
            row.created_by, root.principal_id,
            "connect_client authenticates with the root character's own bound key, so the \
             connecting principal IS the root character"
        );
        let performer = kernel.kernel_db.lock().get_character_by_name(PERFORMER).unwrap().unwrap();
        assert_eq!(
            row.performer,
            Some(performer.principal_id),
            "the performer is the character the create names, not the requester"
        );
        assert_ne!(row.performer, Some(root.principal_id));
        let senpai = kernel.kernel_db.lock().senpai_of(context_id).unwrap();
        assert!(senpai.is_some(), "ordinary context creation records a senpai");
        assert_eq!(
            senpai, row.forked_from,
            "the context it was created from is its senpai"
        );
        drop(kj);
        drop(client);
        tokio::task::yield_now().await;
    });
}

/// A principal with no character row (a real, authenticated key bound to its
/// own principal, not the root character `connect_client` uses) leaves
/// `performer` NULL. No character is minted and the request does not fail:
/// `performer` is metadata, not authority, and a principal without a sheet
/// is a legitimate pre-character state.
#[test]
fn create_context_refuses_a_characterless_principal() {
    run_local(async {
        let tmp = tempfile::tempdir().unwrap();
        let auth_db_path = tmp.path().join("auth.db");
        let unmapped = PrincipalId::new();
        let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        {
            // Bind the key BEFORE the server opens the same file, so
            // `db.authenticate` finds it on the very first connection —
            // it must resolve to `unmapped` directly, not the root
            // character, to prove the characterless case.
            let auth_db = AuthDb::open(&auth_db_path).unwrap();
            auth_db
                .add_key(unmapped, key.public_key(), Some("characterless-test"))
                .unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = SshServerConfig::ephemeral(addr.port());
        common::allow_in_shipped_gate(&config, &["kj context create"]); // model credentials create their own contexts in setup
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
        let mut client = RpcClient::new(rpc_channel.into_stream())
            .await
            .expect("RPC client init failed");
        client.retain_ssh_session(ssh_client);
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();

        let refused = create_context_as(&kj, "played-by-null-test", "anyone").await.unwrap_err();

        assert!(
            kernel.kernel_db.lock().get_character(unmapped).unwrap().is_none(),
            "the fixture is only valid if this principal really has no character row"
        );
        assert!(
            refused.contains("no character sheet"),
            "a new context answers to its creator, so a characterless principal cannot create one: {refused}"
        );
        assert!(
            kernel.kernel_db.lock().find_context_by_label("played-by-null-test").unwrap().is_none(),
            "a refused create commits nothing"
        );
        drop(kj);
        drop(client);
        tokio::task::yield_now().await;
    });
}

/// `listContexts` and `resolveContextLabel` carry the facts the kernel
/// computes from performers and pointers: the root console is the person's
/// and is performed by a person, the console is the one the person's sheet
/// names, an MCP session's parent is the person's hub, and the hub has no
/// performer. Falsified by leaving the facts at their wire defaults.
#[test]
fn context_listings_carry_the_person_console_and_session_facts() {
    run_local(async {
        let (addr, _kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();
        let person = SshServerConfig::EPHEMERAL_ROOT;
        let contexts = kj.list_contexts().await.unwrap();
        let root = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        let run = |argv: Vec<String>| {
            let kj = kj.clone();
            async move {
                let result = kj.execute_kj_quiet(root, &argv).await.unwrap();
                assert_eq!(result.exit_code, 0, "kj {}: {}", argv.join(" "), result.stderr);
            }
        };
        let words = |line: &str| line.split(' ').map(String::from).collect::<Vec<_>>();
        run(words(&format!("context create {PERFORMER} --name facts-console"))).await;
        run(words(&format!("character set {person} --console facts-console"))).await;
        run(words(&format!("context create {PERFORMER} --name facts-session --parent mcp/{person}"))).await;

        let listed = kj.list_contexts().await.unwrap();
        let find = |label: &str| listed.iter().find(|c| c.label == label).unwrap_or_else(|| panic!("{label} is listed")).clone();
        let root_info = listed.iter().find(|c| c.id == root).unwrap();
        assert_eq!(root_info.performer.as_deref(), Some(person));
        assert!(root_info.performed_by_person && root_info.is_root_console, "{root_info:?}");
        assert!(!root_info.is_console && !root_info.is_mcp_session, "{root_info:?}");
        let console = find("facts-console");
        assert!(console.is_console && !console.performed_by_person && !console.is_root_console, "{console:?}");
        assert_eq!(console.performer.as_deref(), Some(PERFORMER));
        let session = find("facts-session");
        assert!(session.is_mcp_session && !session.is_console, "{session:?}");
        let hub = find(&format!("mcp/{person}"));
        assert_eq!(hub.performer, None, "the hub has no performer: {hub:?}");
        assert!(!hub.is_mcp_session && !hub.performed_by_person, "{hub:?}");

        let resolved = kj.resolve_context_label("facts-console").await.unwrap().expect("the console resolves");
        assert!(resolved.is_console && resolved.performer.as_deref() == Some(PERFORMER), "{resolved:?}");
        let resolved = kj.resolve_context_label("facts-session").await.unwrap().expect("the session resolves");
        assert!(resolved.is_mcp_session, "{resolved:?}");
    });
}

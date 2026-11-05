//! An MCP client's session registers under its root's MCP hub with no ask,
//! and ends by archiving itself (`docs/senpai-kohai.md`, "MCP sessions").

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{run_local};
use kaijutsu_client::{ActorHandle, CallError, KeySource, SshConfig, context_archive_argv, spawn_actor};
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_types::PrincipalId;
use russh::keys::{Algorithm, PrivateKey};

async fn connect(addr: std::net::SocketAddr, name: &str, key: Arc<PrivateKey>) -> ActorHandle {
    let actor = spawn_actor(SshConfig {
        host: addr.ip().to_string(), port: addr.port(), username: name.into(),
        key_source: KeySource::InMemory(key), insecure: true,
    }, None, "mcp-session-hub-wire".into(), false);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match actor.whoami().await {
                Ok(_) => break,
                Err(CallError::NotReady(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(error) => panic!("client connection failed: {error}"),
            }
        }
    }).await.expect("client connects");
    actor
}

/// Register `label` the way `register_session` does: from the kernel's only
/// root console, `forked_from` the hub amy's sheet names. Returns the new
/// context's id.
async fn register(actor: &ActorHandle, label: &str, performer: &str) -> kaijutsu_types::ContextId {
    let contexts = actor.list_contexts().await.unwrap();
    let parent = kaijutsu_client::choose_parent(None, &contexts).unwrap();
    let hub = actor.mcp_hub_of(parent.context_id, &contexts).await.unwrap().expect("a root console has a hub");
    let hub_label = contexts.iter().find(|c| c.id == hub).map(|c| c.label.as_str());
    assert_eq!(hub_label, Some("mcp/amy"));
    actor
        .create_context_in_client_cwd_under(parent.context_id, label, performer, None, Some(&hub.to_hex()))
        .await
        .unwrap_or_else(|error| panic!("an mcp registration raises no ask and is not refused: {error}"))
        .id
}

/// Two non-root characters, `claude` and `bob`, register under amy's hub with
/// no ask; each ends its own session with no ask, and neither archives the
/// other's.
#[test]
fn a_non_root_registration_lands_under_the_hub_and_archives_itself_with_no_ask() {
    run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = kaijutsu_server::SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        common::allow_in_shipped_gate(&config, &[]);
        let auth = kaijutsu_server::AuthDb::open(config.auth_db_path.as_ref().unwrap()).unwrap();
        let mut keys = Vec::new();
        for name in ["claude", "bob"] {
            let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
            let principal = PrincipalId::new();
            auth.add_key(principal, key.public_key(), Some(name)).unwrap();
            keys.push((name, principal, Arc::new(key)));
        }
        drop(auth);
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            kaijutsu_server::SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let kernel = kernel_rx.await.unwrap();
        // An MCP connection's character runs the `mcp` directory, so its
        // sessions read `[rc.mcp]`, which allows a session's own archive.
        for (name, principal_id, _) in &keys {
            kernel.kernel_db.lock().insert_character(&CharacterRow { rc_dir: Some("mcp".into()), console_ctx: None, mcp_hub_ctx: None,
                principal_id: *principal_id, name: (*name).into(), created_at: 0, retired_at: None,
                handoff_ctx: None, root_ctx: None, root: false,
            }).unwrap();
        }
        let claude = connect(addr, "claude", keys[0].2.clone()).await;
        let bob = connect(addr, "bob", keys[1].2.clone()).await;

        let claude_session = register(&claude, "claude-session", "claude").await;
        let bob_session = register(&bob, "bob-session", "bob").await;

        {
            let db = kernel.kernel_db.lock();
            let hub = db.find_context_by_label("mcp/amy").unwrap().expect("amy has a hub");
            assert_eq!(hub.performer, None, "a hub has no performer");
            let console = db.find_context_by_label("amy").unwrap().expect("amy has a console");
            for (session, name) in [(claude_session, "claude"), (bob_session, "bob")] {
                let row = db.get_context(session).unwrap().unwrap();
                assert_eq!(row.forked_from, Some(hub.context_id), "{name}'s session is forked from the hub");
                assert_eq!(row.created_by, db.get_character_by_name(name).unwrap().unwrap().principal_id);
                let character = db.context_character(session).unwrap().expect("a session has a context character");
                assert_eq!(character.senpai_ctx, Some(console.context_id), "the root console is the senpai");
                assert_eq!(db.get_character(character.principal_id).unwrap().unwrap().name, name);
            }
            assert!(db.list_pending_asks().unwrap().is_empty(), "registration raised no ask: {:?}", db.list_pending_asks().unwrap());
        }

        // A character cannot archive another character's session.
        let refused = bob
            .execute_kj_quiet(bob_session, context_archive_argv(claude_session))
            .await
            .expect("the gate does not hold an archive from an mcp session");
        assert_ne!(refused.exit_code, 0, "bob archived claude's session: {} {}", refused.stdout, refused.stderr);
        assert!(refused.stderr.contains("may archive it"), "{}", refused.stderr);
        assert!(!kernel.kernel_db.lock().get_context(claude_session).unwrap().unwrap().is_archived());

        // The session ends by archiving itself, with no ask.
        claude.archive_context_from_parent(claude_session).await.expect("a session archives itself");
        bob.archive_context_from_parent(bob_session).await.expect("a session archives itself");
        let db = kernel.kernel_db.lock();
        assert!(db.get_context(claude_session).unwrap().unwrap().is_archived());
        assert!(db.get_context(bob_session).unwrap().unwrap().is_archived());
        assert!(db.list_pending_asks().unwrap().is_empty(), "ending a session raised no ask: {:?}", db.list_pending_asks().unwrap());
    });
}

//! A client creates its context in its own directory when the kernel can see
//! that directory, and says so when it cannot.

mod common;

use std::time::Duration;

use common::{root_key_source, run_local, start_server};
use kaijutsu_client::{ActorHandle, CallError, SshConfig, spawn_actor};

async fn connected_actor(addr: std::net::SocketAddr) -> ActorHandle {
    let actor = spawn_actor(SshConfig {
        host: addr.ip().to_string(), port: addr.port(), username: "amy".into(),
        key_source: root_key_source(addr), insecure: true,
    }, None, "context-create-cwd-wire".into(), false);
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

#[test]
fn a_client_directory_the_kernel_sees_becomes_the_cwd_and_one_it_cannot_is_reported() {
    run_local(async {
        let addr = start_server().await;
        let actor = connected_actor(addr).await;
        let contexts = actor.list_contexts().await.unwrap();
        let parent = kaijutsu_client::choose_parent(None, &contexts).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();

        let created = actor
            .create_context_in_client_cwd(parent.context_id, "in-client-cwd", "default", None, Some(here))
            .await
            .unwrap();
        assert_eq!(created.cwd_refused, None);
        assert_eq!(actor.get_context_cwd(created.id).await.unwrap().as_deref(), Some(here));

        // A directory the kernel host does not have, as when the client runs
        // on another machine: the context is still created, without that
        // cwd, and the refusal comes back for the client to show.
        let elsewhere = dir.path().join("only-on-the-client");
        let elsewhere = elsewhere.to_str().unwrap();
        let created = actor
            .create_context_in_client_cwd(parent.context_id, "elsewhere", "default", None, Some(elsewhere))
            .await
            .unwrap();
        let refused = created.cwd_refused.expect("the refusal is reported");
        assert!(refused.contains(elsewhere), "the refusal names the directory: {refused}");
        assert_eq!(actor.get_context_cwd(created.id).await.unwrap(), None, "the parent has no cwd to inherit");

        // Any other refusal is still an error.
        let error = actor
            .create_context_in_client_cwd(parent.context_id, "elsewhere", "default", None, Some(here))
            .await
            .expect_err("a label conflict is not a cwd refusal");
        assert!(error.is_label_conflict(), "{error}");
    });
}

/// A model's create in Amy's root context is held for her approval. The held
/// command carries `--cwd-optional`, so when Amy allows it the kernel drops
/// the directory it cannot see and creates the context without it.
#[test]
fn a_held_create_drops_a_directory_the_kernel_cannot_see_when_it_runs() {
    use kaijutsu_client::{CreateContextError, KeySource};
    use russh::keys::{Algorithm, PrivateKey};
    use std::sync::Arc;

    run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = kaijutsu_server::SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        common::allow_in_shipped_gate(&config, &[]);
        let amy_key = config.root_key();
        let claude = kaijutsu_types::PrincipalId::new();
        let claude_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
        kaijutsu_server::AuthDb::open(config.auth_db_path.as_ref().unwrap())
            .unwrap()
            .add_key(claude, claude_key.public_key(), Some("claude"))
            .unwrap();
        let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_local(async move {
            kaijutsu_server::SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
        });
        let kernel = kernel_rx.await.unwrap();
        kernel.kernel_db.lock().insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: claude, name: "claude".into(), created_at: 0, retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();

        let connect = |name: &str, key: KeySource| {
            let actor = spawn_actor(SshConfig {
                host: addr.ip().to_string(), port: addr.port(), username: name.into(),
                key_source: key, insecure: true,
            }, None, "context-create-cwd-wire".into(), false);
            async move {
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
        };
        let claude_actor = connect("claude", KeySource::InMemory(Arc::new(claude_key))).await;
        let parent = kaijutsu_client::choose_parent(None, &claude_actor.list_contexts().await.unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("only-on-the-client");
        let elsewhere = elsewhere.to_str().unwrap();

        let held = claude_actor
            .create_context_in_client_cwd(parent.context_id, "held", "mcp", Some("claude"), Some(elsewhere))
            .await
            .expect_err("Amy's root context holds a model's create for her");
        let ask_id = match &held {
            CreateContextError::Held { refusal } => refusal.ask_id().expect("a held create names its ask").to_owned(),
            other => panic!("expected a held create, got {other:?}"),
        };
        let ask = kernel.kernel_db.lock().get_approval(&ask_id).unwrap().unwrap();
        let source = ask.exec_source.clone().unwrap_or_default();
        assert!(source.contains("--cwd-optional"), "the kernel decides when the held create runs: {source}");

        let amy_actor = connect("amy", KeySource::InMemory(amy_key)).await;
        let allowed = amy_actor
            .execute_kj(parent.context_id, vec!["ledger".into(), "allow".into(), ask_id.clone()])
            .await
            .unwrap();
        assert_eq!(allowed.exit_code, 0, "kj ledger allow: {} {}", allowed.stdout, allowed.stderr);

        let created = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(context) = claude_actor.resolve_context_label("held").await.unwrap() {
                    break context;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the approved create lands");
        assert_eq!(claude_actor.get_context_cwd(created.id).await.unwrap(), None, "the parent has no cwd to inherit");
    });
}

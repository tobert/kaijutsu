//! Publication retirement survives startup and the typed SSH client.

mod common;

use std::time::Duration;
use approval_ledger::{ask::create_ask_recorded, decide::{decide, DecideInput}, types::{NewAsk, Origin}};
use kaijutsu_client::{CallError, KeySource, SshConfig, spawn_actor};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::{ContextId, PrincipalId};

#[test]
fn typed_client_distinguishes_approval_from_publication_abandonment() {
    common::run_local(async {
        for allow in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
            let key = config.root_key();
            let path = config.data_dir.as_ref().unwrap().join("kernel.db");
            let amy = kaijutsu_kernel::KernelDb::open(&path).unwrap()
                .get_character_by_name("amy").unwrap().unwrap().principal_id;
            let marker = config.data_dir.as_ref().unwrap().join("must-not-run");
            let request = {
                // Model a crash after the reviewer answers, before pair publication.
                let conn = rusqlite::Connection::open(&path).unwrap();
                let id = create_ask_recorded(&conn, &NewAsk {
                    context_id: ContextId::new().as_bytes().to_vec(), principal_id: amy.as_bytes().to_vec(),
                    actor_id: PrincipalId::new().as_bytes().to_vec(), reviewer_id: amy.as_bytes().to_vec(),
                    origin: Origin::ShellGate, instance: None, tool: None, hook_id: None,
                    description: "interrupted publication".into(), statements: vec![], authorized_label: None,
                    rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
                    exec_source: Some(format!("echo unexpected > '{}'", marker.display())), exec_stdin: None,
                    continuation_epoch: None, env: vec![],
                }, |conn, id| {
                    conn.execute("INSERT INTO approval_pair_handoffs(request_id) VALUES (?1)", [id])?;
                    Ok(())
                }).unwrap();
                decide(&conn, &id, DecideInput { allow, decided_option: Some(if allow { "allow_once" } else { "deny" }), ..Default::default() }).unwrap();
                id
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let server = tokio::task::spawn_local(async move {
                SshServer::new(config).run_on_listener_with_kernel_sink(listener, tx).await.unwrap();
            });
            let kernel = rx.await.unwrap();
            let actor = spawn_actor(SshConfig {
                host: addr.ip().to_string(), port: addr.port(), username: "amy".into(),
                key_source: KeySource::InMemory(key), insecure: true,
            }, None, "publication-wire".into(), false);
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    match actor.whoami().await {
                        Ok(_) => break,
                        Err(CallError::NotReady(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
                        Err(error) => panic!("client connection failed: {error}"),
                    }
                }
            }).await.expect("client connects");
            let contexts = actor.list_contexts().await.unwrap();
            let root = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
            let detail = kaijutsu_client::ledger::show_ask_detail(&actor, root, &request).await.unwrap().unwrap();
            assert_eq!(detail.status, if allow { "allowed" } else { "denied" });
            assert_eq!(detail.decided_option.as_deref(), Some(if allow { "allow_once" } else { "deny" }));
            assert!(detail.redeemed_at.is_some(), "the abandoned invocation cannot spend this answer later");
            let reason = detail.publication_abandoned.expect("typed client retains the retirement reason");
            assert!(reason.contains("Kernel restarted"), "{reason}");
            assert!(reason.contains("Source did not run."), "{reason}");
            assert!(!marker.exists(), "startup must not execute the approved source");
            drop(actor);
            kernel.kernel.shutdown_runtime_worker().await.unwrap();
            server.abort();
            let _ = server.await;
        }
    });
}

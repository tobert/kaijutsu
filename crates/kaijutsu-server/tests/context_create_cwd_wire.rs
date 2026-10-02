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

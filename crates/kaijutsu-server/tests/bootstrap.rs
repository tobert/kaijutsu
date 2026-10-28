//! e2e: `kaijutsu-server bootstrap` leaves a kernel whose `ROOT` console
//! answers its first prompt (`docs/server-cli.md`, "bootstrap").
//!
//! Bootstrap runs against the same paths a serving kernel uses, then the
//! server boots on them and the root character types into `ROOT` over the
//! wire. The mock backend's canned reply is the answer.

mod common;

use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KeySource, RpcClient, ServerEvent, SshClient, SshConfig, turn_events_channel};
use kaijutsu_kernel::KernelDb;
use kaijutsu_server::bootstrap::{BackendChoice, BootstrapOptions, bootstrap};
use kaijutsu_server::{SshServer, SshServerConfig};
use kaijutsu_types::{BlockKind, BlockQuery, Role};

fn options(config: &SshServerConfig, backend: Option<BackendChoice>) -> BootstrapOptions {
    BootstrapOptions {
        root: "amy".to_string(),
        root_key: config.root_key().public_key().clone(),
        key_comment: None,
        backends: backend.into_iter().collect(),
        console_performer: "root".to_string(),
    }
}

fn kernel_db(config: &SshServerConfig) -> KernelDb {
    KernelDb::open(config.data_dir.as_ref().expect("ephemeral data dir").join("kernel.db"))
        .expect("open kernel.db")
}

/// A fresh kernel, bootstrapped with a backend, answers in `ROOT`. A second
/// bootstrap finds nothing to do.
#[test]
fn a_bootstrapped_kernel_answers_in_root() {
    run_local(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
        let opts = options(&config, Some(BackendChoice::mock("mock-model")));

        let first = bootstrap(&config, &opts).await.expect("bootstrap");
        assert!(!first.ran.is_empty(), "a fresh kernel needs setup: {first:?}");
        let again = bootstrap(&config, &opts).await.expect("bootstrap again");
        assert!(again.ran.is_empty(), "a second bootstrap has nothing to do: {again:?}");

        let root_ctx = {
            let db = kernel_db(&config);
            let row = db.find_context_by_label("ROOT").unwrap().expect("bootstrap creates ROOT");
            assert_eq!(row.context_type, "console");
            let performer = db.get_character_by_name("root").unwrap().expect("the console performer exists");
            assert_eq!(row.played_by, Some(performer.principal_id), "ROOT is played by its model character");
            let cast = db.character_cast(performer.principal_id).unwrap().expect("the performer plays the house cast");
            let slot = db.get_cast_slot(cast.cast_id, "console").unwrap().expect("the house cast fills the console role");
            assert_eq!(slot.model, "mock-model");
            row.context_id
        };

        let key = (*config.root_key()).clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = SshServer::new(config).run_on_listener(listener).await {
                log::error!("bootstrap test server error: {e}");
            }
        });

        let mut ssh = SshClient::new(SshConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "amy".to_string(),
            key_source: KeySource::InMemory(std::sync::Arc::new(key)),
            insecure: true,
        });
        let channel = ssh.connect().await.expect("SSH connect");
        let mut client = RpcClient::new(channel.into_stream()).await.expect("RPC client init");
        client.retain_ssh_session(ssh);
        let (amy, _) = client.bind_kernel().await.expect("bind_kernel");

        let (callback, mut turns) = turn_events_channel(16);
        amy.subscribe_turn_events(callback).await.expect("subscribe_turn_events");
        amy.join_context(root_ctx, "amy-in-root").await.expect("join ROOT");
        amy.edit_input(root_ctx, 0, "hello", 0).await.expect("edit_input");
        amy.submit_input(root_ctx, false).await.expect("submit_input");

        loop {
            match tokio::time::timeout(Duration::from_secs(30), turns.recv()).await {
                Ok(Ok(ServerEvent::TurnCompleted { context_id, .. })) if context_id == root_ctx => break,
                Ok(Ok(ServerEvent::TurnFailed { context_id, error, .. })) if context_id == root_ctx => {
                    panic!("ROOT's turn failed: {error}")
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("turn push channel error: {e}"),
                Err(_) => panic!("timed out waiting for ROOT to answer"),
            }
        }
        let blocks = amy.get_blocks(root_ctx, &BlockQuery::All).await.expect("get_blocks");
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == BlockKind::Text && b.role == Role::Model && b.content.contains("Mock summary")),
            "ROOT answered with the mock's reply"
        );
    });
}

/// With no backend, bootstrap still creates `ROOT` and says how to give it a
/// model. A later bootstrap with a backend adds only the backend.
#[test]
fn without_a_backend_root_waits_for_one() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");

        let bare = bootstrap(&config, &options(&config, None)).await.expect("bootstrap without a backend");
        assert!(
            bare.notes.iter().any(|note| note.contains("kj backend set")),
            "the report names the verb that adds a backend: {bare:?}"
        );
        {
            let db = kernel_db(&config);
            assert!(db.find_context_by_label("ROOT").unwrap().is_some(), "ROOT exists without a model");
            assert!(db.get_cast_by_label("house").unwrap().is_none(), "no cast without a backend");
        }

        let filled = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mock-model"))))
            .await
            .expect("bootstrap with a backend");
        assert_eq!(
            filled.ran,
            [
                "kj backend set mock --kind mock --key-optional",
                "kj cast create house",
                "kj cast slot set house console --backend mock --model mock-model --max-tokens 16384",
                "kj cast slot set house distill --backend mock --model mock-model --max-tokens 16384",
                "kj character set root --cast house",
                "kj context rotate ROOT",
            ],
            "the backend, the house cast, and a rotated ROOT that takes its slots"
        );
    });
}

/// A model bootstrap has no window for gets no model row, and the report
/// says so and names the verb that adds one.
#[test]
fn an_unknown_model_is_named_in_the_report() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let report = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mystery-model"))))
            .await
            .expect("bootstrap with an unknown model");
        assert!(
            report.notes.iter().any(|note| note.contains("mystery-model") && note.contains("kj backend model set")),
            "the report names the model and the verb: {report:?}"
        );
    });
}

/// ROOT's performer must be someone other than the root character, who
/// reviews it. Naming the root is refused, and nothing is made.
#[test]
fn the_root_cannot_play_root() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let mut opts = options(&config, None);
        opts.console_performer = "amy".to_string();
        let err = bootstrap(&config, &opts).await.expect_err("the root cannot perform ROOT");
        assert!(err.contains("--console-as"), "{err}");
        assert!(kernel_db(&config).find_context_by_label("ROOT").unwrap().is_none(), "nothing was made");
    });
}

/// Rotation keeps a context's character, so a re-run naming another performer
/// cannot move ROOT to it: the report says to archive ROOT, and ROOT is not
/// rotated for nothing.
#[test]
fn a_different_console_performer_is_reported_and_root_is_left_alone() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let mut opts = options(&config, Some(BackendChoice::mock("mock-model")));
        bootstrap(&config, &opts).await.expect("bootstrap");
        opts.console_performer = "other".to_string();
        let again = bootstrap(&config, &opts).await.expect("bootstrap as another performer");
        assert!(
            again.notes.iter().any(|note| note.contains("ROOT is not played by other")),
            "the report names the mismatch: {again:?}"
        );
        assert!(!again.ran.iter().any(|line| line.contains("context rotate")), "ROOT was not rotated: {again:?}");
    });
}

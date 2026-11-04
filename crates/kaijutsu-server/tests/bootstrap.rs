//! e2e: `kaijutsu-server bootstrap` leaves a kernel whose console
//! answers its first prompt (`docs/server-cli.md`, "bootstrap").
//!
//! Bootstrap runs against the same paths a serving kernel uses, then the
//! server boots on them and the root character types into the console over the
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

/// A fresh kernel, bootstrapped with a backend, answers in the console. A second
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
            let row = db.find_context_by_label("console").unwrap().expect("bootstrap creates the console");
            assert_eq!(row.context_type, "console");
            let performer = db.get_character_by_name("root").unwrap().expect("the console performer exists");
            assert_eq!(row.performer, Some(performer.principal_id), "the console is played by its model character");
            let model = db.get_character_model(performer.principal_id).unwrap().expect("the performer holds a model");
            assert_eq!(model.model, "mock-model");
            assert!(db.list_character_slots(performer.principal_id).unwrap().is_empty(), "a model, not role slots");
            let slot = db.context_slot(row.context_id, "model").unwrap().expect("the console copied the model under model");
            assert_eq!(slot.model, "mock-model");
            assert!(db.context_slot(row.context_id, "distill").unwrap().is_some(), "and under distill");
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
        amy.join_context(root_ctx, "amy-in-root").await.expect("join the console");
        amy.edit_input(root_ctx, 0, "hello", 0).await.expect("edit_input");
        amy.submit_input(root_ctx, false).await.expect("submit_input");

        loop {
            match tokio::time::timeout(Duration::from_secs(30), turns.recv()).await {
                Ok(Ok(ServerEvent::TurnCompleted { context_id, .. })) if context_id == root_ctx => break,
                Ok(Ok(ServerEvent::TurnFailed { context_id, error, .. })) if context_id == root_ctx => {
                    panic!("the console's turn failed: {error}")
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("turn push channel error: {e}"),
                Err(_) => panic!("timed out waiting for the console to answer"),
            }
        }
        let blocks = amy.get_blocks(root_ctx, &BlockQuery::All).await.expect("get_blocks");
        assert!(
            blocks
                .iter()
                .any(|b| b.kind == BlockKind::Text && b.role == Role::Model && b.content.contains("Mock summary")),
            "the console answered with the mock's reply"
        );
    });
}

/// With no backend, bootstrap still creates the console and says how to give it a
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
            assert!(db.find_context_by_label("console").unwrap().is_some(), "the console exists without a model");
            let root = db.get_character_by_name("root").unwrap().expect("the console performer exists");
            assert!(db.list_character_slots(root.principal_id).unwrap().is_empty(), "no slot without a backend");
            assert!(db.get_character_model(root.principal_id).unwrap().is_none(), "no model without a backend");
        }

        let filled = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mock-model"))))
            .await
            .expect("bootstrap with a backend");
        assert_eq!(
            filled.ran,
            [
                "kj backend set mock --kind mock --key-optional",
                "kj character model set root mock/mock-model --max-tokens 16384",
                "kj context rotate console",
            ],
            "the backend, the performer's model, and a rotated the console that takes it"
        );
        let again = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mock-model"))))
            .await
            .expect("bootstrap again");
        assert!(again.ran.is_empty(), "a second run changes nothing: {again:?}");
    });
}

/// A slot gives a performer no chat model: one that holds only a `distill`
/// slot gets bootstrap's model, keeps its slot, and the console it plays is
/// rotated to take the model.
///
/// Falsified by counting a role slot as the performer's model.
#[test]
fn a_performer_with_only_a_distill_slot_gets_a_model() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        bootstrap(&config, &options(&config, None)).await.expect("bootstrap without a backend");
        {
            let db = kernel_db(&config);
            let root = db.get_character_by_name("root").unwrap().unwrap();
            db.seed_test_slot_entries(root.principal_id, &[("distill", "mock", "mock-model")]).unwrap();
        }
        let report = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mock-model"))))
            .await
            .expect("bootstrap with a backend");
        assert!(report.ran.iter().any(|line| line.contains("character model set root")), "{report:?}");
        assert!(report.ran.iter().any(|line| line.contains("context rotate console")), "{report:?}");
        let db = kernel_db(&config);
        let root = db.get_character_by_name("root").unwrap().unwrap();
        assert!(db.get_character_model(root.principal_id).unwrap().is_some());
        assert!(db.get_character_slot(root.principal_id, "distill").unwrap().is_some(), "the slot stays");
        let console = db.find_context_by_label("console").unwrap().unwrap();
        assert!(db.context_slot(console.context_id, "model").unwrap().is_some(), "the rotated console has a model");
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

/// the console's performer must be someone other than the root character, who
/// reviews it. Naming the root is refused, and nothing is made.
#[test]
fn the_root_cannot_play_root() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let mut opts = options(&config, None);
        opts.console_performer = "amy".to_string();
        let err = bootstrap(&config, &opts).await.expect_err("the root cannot perform the console");
        assert!(err.contains("--console-as"), "{err}");
        assert!(kernel_db(&config).find_context_by_label("console").unwrap().is_none(), "nothing was made");
    });
}

/// Rotation keeps a context's character, so a re-run naming another performer
/// cannot move console to it: the report says to archive console, and the console is not
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
            again.notes.iter().any(|note| note.contains("The console is not played by other")),
            "the report names the mismatch: {again:?}"
        );
        assert!(!again.ran.iter().any(|line| line.contains("context rotate")), "the console was not rotated: {again:?}");
    });
}

/// The root's sheet points at the console bootstrap makes, and a rotation that
/// gives the console its model moves the pointer to the successor.
#[test]
fn bootstrap_points_the_root_at_the_console_and_rotation_moves_it() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let bare = bootstrap(&config, &options(&config, None)).await.expect("bootstrap without a backend");
        assert!(bare.ran.iter().any(|l| l == "kj character set amy --console console"), "{bare:?}");
        let before = {
            let db = kernel_db(&config);
            let console = db.find_context_by_label("console").unwrap().expect("the console exists").context_id;
            let amy = db.get_character_by_name("amy").unwrap().unwrap();
            assert_eq!(amy.console_ctx, Some(console), "the root points at its console");
            console
        };

        let filled = bootstrap(&config, &options(&config, Some(BackendChoice::mock("mock-model"))))
            .await
            .expect("bootstrap with a backend");
        assert!(filled.ran.iter().any(|l| l == "kj context rotate console"), "{filled:?}");
        let db = kernel_db(&config);
        let after = db.find_context_by_label("console").unwrap().expect("the console exists").context_id;
        assert_ne!(before, after, "the rotation made a successor");
        let amy = db.get_character_by_name("amy").unwrap().unwrap();
        assert_eq!(amy.console_ctx, Some(after), "the pointer moved with the label");
        assert!(!filled.ran.iter().any(|l| l.contains("character set amy --console")), "the rotation moved it: {filled:?}");
    });
}

/// A console made before the sheet had a pointer is accepted as it stands, and
/// bootstrap sets the pointer without making another context.
#[test]
fn bootstrap_adopts_an_existing_console_the_sheet_does_not_point_at() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        bootstrap(&config, &options(&config, None)).await.expect("bootstrap without a backend");
        let console = {
            let db = kernel_db(&config);
            let amy = db.get_character_by_name("amy").unwrap().unwrap();
            db.set_character_console_ctx(amy.principal_id, None).unwrap();
            db.find_context_by_label("console").unwrap().unwrap().context_id
        };

        let again = bootstrap(&config, &options(&config, None)).await.expect("bootstrap again");
        assert_eq!(again.ran, ["kj character set amy --console console"], "{again:?}");
        let db = kernel_db(&config);
        assert_eq!(db.find_context_by_label("console").unwrap().unwrap().context_id, console, "no second console");
        assert_eq!(db.get_character_by_name("amy").unwrap().unwrap().console_ctx, Some(console));

        let third = bootstrap(&config, &options(&config, None)).await.expect("bootstrap a third time");
        assert!(third.ran.is_empty(), "the pointer is set, so nothing is left: {third:?}");
    });
}

/// The note about a performer whose directory is not `console` says that the
/// change moves every context the character plays, and does not recommend it
/// when the character plays a context of another type: it names that context.
#[test]
fn the_rc_note_warns_that_the_change_moves_every_context_the_character_plays() {
    run_local(async {
        let config = SshServerConfig::ephemeral_with_root(0, "amy");
        let opts = options(&config, Some(BackendChoice::mock("mock-model")));
        bootstrap(&config, &opts).await.expect("bootstrap");
        {
            let db = kernel_db(&config);
            let root = db.get_character_by_name("root").unwrap().unwrap();
            db.set_character_rc_dir(root.principal_id, Some("coder")).unwrap();
        }
        let only_console = bootstrap(&config, &opts).await.expect("bootstrap again");
        let note = only_console.notes.iter().find(|n| n.contains("runs rc directory coder")).expect("rc note");
        assert!(note.contains("kj character set root --rc console"), "{note}");
        assert!(note.contains("every context root plays"), "{note}");

        {
            let db = kernel_db(&config);
            let root = db.get_character_by_name("root").unwrap().unwrap();
            let console = db.find_context_by_label("console").unwrap().unwrap();
            let mut row = console.clone();
            row.context_id = kaijutsu_types::ContextId::new();
            row.label = Some("side-coder".to_string());
            row.context_type = "coder".to_string();
            row.performer = None;
            row.senpai_ctx = console.senpai_ctx;
            let ws = db.get_or_create_default_workspace(row.created_by).unwrap();
            db.insert_placed_context(&row, ws).unwrap();
            db.write_context_character(row.context_id, root.principal_id, console.senpai_ctx, None).unwrap();
        }
        let with_coder = bootstrap(&config, &opts).await.expect("bootstrap with a coder context");
        let note = with_coder.notes.iter().find(|n| n.contains("runs rc directory coder")).expect("rc note");
        assert!(note.contains("side-coder"), "the note names the context of another type: {note}");
        assert!(!note.contains("kj character set root --rc console"), "and does not recommend the change: {note}");
    });
}

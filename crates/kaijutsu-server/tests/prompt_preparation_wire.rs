//! Interactive prompt preparation belongs to the kernel after admission.

mod common;

use std::time::Duration;

use common::{connect_client, create_context, run_local, seed_turn_identity, start_server_with_mock_llm_kernel_handle};
use kaijutsu_client::{MidiExchangeSlot, ServerEvent, block_events_channel, turn_events_channel};
use kaijutsu_types::{ContextId, Role, Status};
use tokio::sync::broadcast::Receiver;

#[derive(Clone, Copy)]
enum InputPath {
    Prompt,
    ChatSubmit,
}

#[test]
fn prompt_preparation_survives_the_submitting_ssh_client_disconnect() {
    disconnect_during_preparation(InputPath::Prompt);
}

#[test]
fn chat_submit_preparation_survives_the_submitting_ssh_client_disconnect() {
    disconnect_during_preparation(InputPath::ChatSubmit);
}

#[test]
fn shutdown_joins_submit_lifecycle_before_finishing_the_turn() {
    run_local(async {
        use kaijutsu_kernel::vfs::VfsOps;
        use std::path::Path;
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "shutdown-during-submit").await.unwrap();
        seed_turn_identity(&server, context);
        let vfs = server.kernel.vfs();
        vfs.mkdir(Path::new("/config/rc/preparation"), 0o755).await.unwrap();
        vfs.mkdir(Path::new("/config/rc/preparation/submit"), 0o755).await.unwrap();
        vfs.write_all(Path::new("/config/rc/preparation/submit/S00-wait.kai"),
            b"kj block create --role system --kind text --content submit-entered\nsleep 60\n").await.unwrap();
        vfs.write_all(Path::new("/config/rc/preparation/submit/S10-later.kai"),
            b"kj block create --role system --kind text --content must-not-run\n").await.unwrap();
        server.kernel_db.lock().update_context_type(context, "preparation").unwrap();
        kj.edit_input(context, 0, "retained submitted draft", 0).await.unwrap();
        let (block_callback, mut blocks) = block_events_channel(32, MidiExchangeSlot::new());
        kj.subscribe_blocks(block_callback).await.unwrap();
        let (turn_callback, mut turns) = turn_events_channel(32);
        kj.subscribe_turn_events(turn_callback).await.unwrap();
        let submitter = kj.clone();
        let request = tokio::task::spawn_local(async move { submitter.submit_input(context, false).await });
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), blocks.recv()).await.unwrap().unwrap();
            if matches!(event, ServerEvent::BlockInserted { context_id, block }
                if context_id == context && block.content == "submit-entered") { break; }
        }
        assert!(server.kernel.turn_in_flight(context), "submit lifecycle already owns its turn");
        tokio::time::timeout(Duration::from_secs(3), server.kernel.shutdown_runtime_worker()).await
            .expect("shutdown must signal the script and join its cleanup").unwrap();
        assert!(request.await.unwrap().is_err());
        assert!(matches!(recv_terminal(&mut turns, context).await, ServerEvent::TurnCompleted { .. }));
        let retained = kj.get_blocks(context, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert!(retained.iter().any(|block| block.content == "retained submitted draft" && block.status == Status::Done));
        assert!(retained.iter().any(|block| block.content == "submit-entered"));
        assert!(!retained.iter().any(|block| block.content == "must-not-run" || block.role == Role::Model));
        assert!(!server.kernel.turn_in_flight(context));
    });
}

fn disconnect_during_preparation(path: InputPath) {
    run_local(async move {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let observer_client = connect_client(addr).await;
        let (observer, _) = observer_client.bind_kernel().await.unwrap();
        let submitting_client = connect_client(addr).await;
        let (submitting, _) = submitting_client.bind_kernel().await.unwrap();
        let context = create_context(&submitting, "disconnect-during-preparation").await.unwrap();
        seed_turn_identity(&server, context);
        submitting.join_context(context, "submitter").await.unwrap();

        if matches!(path, InputPath::ChatSubmit) {
            submitting.edit_input(context, 0, "chat survives disconnect", 0).await.unwrap();
        }

        let (block_callback, mut blocks) = block_events_channel(32, MidiExchangeSlot::new());
        observer.subscribe_blocks(block_callback).await.unwrap();
        let (turn_callback, mut turns) = turn_events_channel(32);
        observer.subscribe_turn_events(turn_callback).await.unwrap();

        // Provider selection takes the matching read guard in
        // `spawn_admitted_turn`. Holding the writer parks preparation after
        // input persistence, without a wall-clock race.
        let registry = server.kernel.llm().write().await;
        let request = match path {
            InputPath::Prompt => {
                let kernel = submitting.clone();
                tokio::task::spawn_local(async move {
                    kernel.prompt("prompt survives disconnect", None, context).await.map(|_| ())
                })
            }
            InputPath::ChatSubmit => {
                let kernel = submitting.clone();
                tokio::task::spawn_local(async move {
                    kernel.submit_input(context, false).await.map(|_| ())
                })
            }
        };

        wait_for_authored_input(&mut blocks, context, path).await;
        assert!(
            server.kernel.turn_in_flight(context),
            "admitted preparation must retain turn liveness before provider selection"
        );

        request.abort();
        assert!(request.await.is_err(), "the submitting RPC must be cancelled before disconnect");
        drop(submitting);
        drop(submitting_client);
        drop(registry);

        match recv_terminal(&mut turns, context).await {
            ServerEvent::TurnCompleted { .. } => {}
            ServerEvent::TurnFailed { error, .. } => {
                panic!("kernel-owned preparation must settle after disconnect: {error}")
            }
            event => panic!("expected a terminal turn event, got {event:?}"),
        }
        assert!(
            !server.kernel.turn_in_flight(context),
            "terminal delivery releases the accepted turn lease"
        );
        let retained = observer.get_blocks(context, &kaijutsu_types::BlockQuery::All).await.unwrap();
        assert!(retained.iter().any(|block| block.role == Role::Model
            && block.status == Status::Done && !block.content.is_empty()),
            "disconnected preparation must retain the model's completed output");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

async fn wait_for_authored_input(
    events: &mut Receiver<ServerEvent>,
    context: ContextId,
    path: InputPath,
) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("timed out waiting for the input mutation")
            .expect("block event channel closed");
        match (path, event) {
            (InputPath::Prompt, ServerEvent::BlockInserted { context_id, block })
                if context_id == context
                    && block.role == Role::User
                    && block.content == "prompt survives disconnect" => return,
            (InputPath::ChatSubmit, ServerEvent::BlockStatusChanged { context_id, status, .. })
                if context_id == context && status == Status::Done => return,
            _ => continue,
        }
    }
}

async fn recv_terminal(events: &mut Receiver<ServerEvent>, context: ContextId) -> ServerEvent {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("timed out waiting for terminal turn delivery after disconnect")
            .expect("turn event channel closed");
        match event {
            event @ ServerEvent::TurnCompleted { context_id, .. }
            | event @ ServerEvent::TurnFailed { context_id, .. }
                if context_id == context => return event,
            _ => continue,
        }
    }
}

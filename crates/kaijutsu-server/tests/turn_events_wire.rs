//! e2e: the turn-outcome **wire surface** — the capnp `TurnEvents` callback
//! interface and its `subscribeTurnEvents` push channel.
//!
//! Drives a real SSH + Cap'n Proto round-trip end to end against a mock LLM
//! backend, no GUI. This is the test that would have been impossible to write
//! before: turn completion had no wire representation at all, so a client's
//! only recourse was to poll block status and guess. The assertions here are
//! the things polling *cannot* tell you —
//!
//!   * that a turn ended, as a push, without asking;
//!   * *which block* the turn produced (rather than "the last one, probably");
//!   * that the ending was a cancel and not a finish.
//!
//! It deliberately drives the INTERACTIVE path (`prompt`), because that is the
//! path that used to announce nothing whatsoever — the turn an app or an ACP
//! frontend most needs to hear about.

mod common;

use std::time::Duration;

use tokio::sync::broadcast::Receiver;

use common::{connect_client, run_local, start_server_with_mock_llm_kernel_handle};
use kaijutsu_client::{
    KernelHandle, ServerEvent, TurnCompletedStopReason, TurnOrigin, turn_events_channel,
};
use kaijutsu_types::ContextId;
use kaijutsu_types::{BlockKind, BlockQuery, PrincipalId, Role};

/// Model turns require an explicit, distinct performer/reviewer assignment.
/// Wire context creation deliberately leaves the performer unset, so these
/// turn-event tests arrange the identity contract before prompting.
fn assign_turn_identity(
    server: &kaijutsu_server::SharedKernel,
    context: ContextId,
    actor: PrincipalId,
) {
    let reviewer = PrincipalId::new();
    let db = server.kernel_db.lock();
    if db.get_character(actor).unwrap().is_none() {
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: actor,
            name: "turn-actor".into(),
            created_at: 0,
            retired_at: None,
            handoff_ctx: None,
        })
        .unwrap();
    }
    db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
        principal_id: reviewer,
        name: "turn-reviewer".into(),
        created_at: 0,
        retired_at: None,
        handoff_ctx: None,
    })
    .unwrap();
    db.update_context_review(context, Some(actor), Some(reviewer))
        .unwrap();
}

/// Drain the turn push channel until a terminal event for `context` arrives.
///
/// A timeout is a hard failure, not a skip: a missing push is exactly the bug
/// this test exists to catch, and "the turn probably finished" is the guess the
/// channel was built to replace.
async fn recv_turn_event(rx: &mut Receiver<ServerEvent>, context: ContextId) -> ServerEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
            Ok(Ok(ev @ ServerEvent::TurnCompleted { context_id, .. }))
            | Ok(Ok(ev @ ServerEvent::TurnFailed { context_id, .. }))
                if context_id == context =>
            {
                return ev;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("turn push channel error: {e}"),
            Err(_) => panic!("timed out waiting for a turn event on {context}"),
        }
    }
}

/// Fetch every block in a context (the wire's own view, for cross-checking the
/// block id the push claimed).
async fn get_all_blocks(
    kernel: &KernelHandle,
    context_id: ContextId,
) -> Vec<kaijutsu_types::BlockSnapshot> {
    kernel
        .get_blocks(context_id, &BlockQuery::All)
        .await
        .expect("get_blocks")
}

/// The headline: subscribe, drive an interactive turn, and receive a pushed
/// `TurnCompleted` naming the exact block the model wrote.
///
/// The `output_block_id` assertion is the sharp end. A polling client can only
/// ever conclude "the newest model block is probably the answer" — a guess that
/// races any sibling writer (drift, a shell result, another agent's MCP call)
/// landing in the same context. The push names the block the turn *produced*.
#[test]
fn interactive_turn_pushes_completed_with_its_output_block() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let actor = client.whoami().await.unwrap().principal_id;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let ctx = kernel.create_context("turns").await.unwrap();
        assign_turn_identity(&server, ctx, actor);
        let _joined = kernel.join_context(ctx, "test").await.unwrap();

        // Subscribe BEFORE prompting, so the event cannot be missed.
        let (callback, mut rx) = turn_events_channel(64);
        kernel.subscribe_turn_events(callback).await.unwrap();

        kernel
            .prompt("write a phrase", None, ctx)
            .await
            .expect("prompt accepted");

        let output_block_id = match recv_turn_event(&mut rx, ctx).await {
            ServerEvent::TurnCompleted {
                context_id,
                output_block_id,
                stop_reason,
                origin,
                ..
            } => {
                assert_eq!(context_id, ctx);
                assert_eq!(
                    stop_reason,
                    TurnCompletedStopReason::EndTurn,
                    "the mock provider ends its turn cleanly"
                );
                assert_eq!(
                    origin,
                    TurnOrigin::Interactive,
                    "a `prompt` turn is interactive — and it still announces"
                );
                output_block_id.expect("the mock wrote text, so the id must be carried")
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        };

        // The id names a real Model/Text block in this context — not the user's
        // prompt block, and not a fabricated id.
        let blocks = get_all_blocks(&kernel, ctx).await;
        let output = blocks
            .iter()
            .find(|b| b.id == output_block_id)
            .unwrap_or_else(|| {
                panic!("TurnCompleted named a block that isn't in the log: {blocks:#?}")
            });
        assert_eq!(
            output.role,
            Role::Model,
            "the turn's output is the MODEL's block, not the seed prompt"
        );
        assert_eq!(output.kind, BlockKind::Text);
    });
}

/// A cancelled turn arrives as `TurnCompleted` with a cancelled stop reason —
/// never as a failure, and never as a clean `EndTurn`.
///
/// This is the distinction ACP's `session/cancel` → `stopReason: cancelled`
/// needs, and the one a polling client can never make: from the outside, a
/// cancelled turn and a finished turn both look like "blocks stopped changing".
#[test]
fn cancelled_turn_pushes_a_cancelled_stop_reason() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let actor = client.whoami().await.unwrap().principal_id;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let ctx = kernel.create_context("cancel").await.unwrap();
        assign_turn_identity(&server, ctx, actor);
        let _joined = kernel.join_context(ctx, "test").await.unwrap();

        let (callback, mut rx) = turn_events_channel(64);
        kernel.subscribe_turn_events(callback).await.unwrap();

        kernel
            .prompt("write a phrase", None, ctx)
            .await
            .expect("prompt accepted");

        // Soft-cancel the turn. The mock provider is fast, so this races the
        // stream — which is fine and is the honest thing to assert: EITHER the
        // cancel lands (a cancelled reason) OR the turn had already finished
        // (EndTurn). What must never happen is a cancel arriving as a `Failed`
        // with a stringly error, which is what this path used to publish.
        let _ = kernel.interrupt_context(ctx, false).await;

        match recv_turn_event(&mut rx, ctx).await {
            ServerEvent::TurnCompleted {
                stop_reason,
                origin,
                ..
            } => {
                assert_eq!(origin, TurnOrigin::Interactive);
                // Whichever side of the race wins, the reason must be one of
                // exactly two things — and if the cancel landed it must be the
                // SOFT one, because that is what we asked for. An immediate
                // cancel here would mean soft/hard attribution is broken.
                assert!(
                    matches!(
                        stop_reason,
                        TurnCompletedStopReason::EndTurn
                            | TurnCompletedStopReason::CancelledSoft
                    ),
                    "a soft-cancelled turn either finished first (EndTurn) or reports \
                     CancelledSoft; got {stop_reason:?}"
                );
            }
            ServerEvent::TurnFailed { error, .. } => panic!(
                "a cancel must never surface as a failure — that stringly error is \
                 exactly what the structured stop reason replaced: {error}"
            ),
            other => panic!("expected a turn outcome, got {other:?}"),
        }
    });
}

/// An AUTONOMOUS turn (`kj fork --prompt`) reaches the same wire channel,
/// tagged `Autonomous` and naming the child context.
///
/// This is gap #5's substrate — delegation join. A parent that forks a child
/// with a seed has, until now, had no way to learn the child acted
/// (`request_child_turn` is fire-and-forget); it can now wait on this push
/// instead of polling the child's block log.
///
/// This is also the ONE path that actually publishes `turn.requested` — an
/// interactive `prompt()` calls `spawn_llm_for_prompt` directly and never
/// touches the bus, so `onTurnStarted` can only be observed on the autonomous
/// (`kj fork`/`kj drive`/drift) path. Hence this test, not the interactive
/// one above, is where `TurnStarted` gets exercised.
#[test]
fn autonomous_fork_turn_pushes_started_then_completed_for_the_child() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let actor = client.whoami().await.unwrap().principal_id;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let main_ctx = kernel.create_context("parent").await.unwrap();
        assign_turn_identity(&server, main_ctx, actor);
        let _joined = kernel.join_context(main_ctx, "test").await.unwrap();

        let (callback, mut rx) = turn_events_channel(64);
        kernel.subscribe_turn_events(callback).await.unwrap();

        // POSIX-style fork with a seed: returns immediately, child starts acting.
        let block_id = kernel
            .shell_execute(
                r#"kj fork --name explorer --prompt "investigate the bug""#,
                main_ctx,
                true,
            )
            .await
            .expect("kj fork accepted");
        let _ = block_id;

        // The child's context id is not known up front, so the FIRST signal
        // this test can key on is `TurnStarted` for a context that isn't the
        // parent's. `TurnFlow::Requested` carries no `origin` field, so this
        // is the only filter available — and it is enough, since nothing
        // else in this test drives a turn on any other context.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let child_ctx = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no TurnStarted arrived — the fork's turn request never announced"
            );
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ServerEvent::TurnStarted { context_id, .. })) if context_id != main_ctx => {
                    break context_id;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("turn push channel error: {e}"),
                Err(_) => panic!("timed out waiting for the child's TurnStarted"),
            }
        };

        // Now that the child's context is known, the outcome push must name
        // that SAME context and be tagged Autonomous.
        match recv_turn_event(&mut rx, child_ctx).await {
            ServerEvent::TurnCompleted { context_id, origin, .. } => {
                assert_eq!(
                    context_id, child_ctx,
                    "TurnStarted and TurnCompleted must name the same child context"
                );
                assert_eq!(
                    origin,
                    TurnOrigin::Autonomous,
                    "a `kj fork --prompt` turn is autonomous"
                );
            }
            ServerEvent::TurnFailed { error, .. } => {
                panic!("the child's autonomous turn failed: {error}")
            }
            other => panic!("unreachable: {other:?}"),
        }
    });
}

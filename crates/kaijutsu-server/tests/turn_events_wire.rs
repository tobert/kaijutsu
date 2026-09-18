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
use common::{create_context};

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
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
    }
    db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
        principal_id: reviewer,
        name: "turn-reviewer".into(),
        created_at: 0,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
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
        // The client connects as the root character, which cannot perform
        // a turn, so the performer is a separate principal.
        let actor = PrincipalId::new();
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let ctx = create_context(&kernel, "turns").await.unwrap();
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
        // The client connects as the root character, which cannot perform
        // a turn, so the performer is a separate principal.
        let actor = PrincipalId::new();
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let ctx = create_context(&kernel, "cancel").await.unwrap();
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
        // The client connects as the root character, which cannot perform
        // a turn, so the performer is a separate principal.
        let actor = PrincipalId::new();
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let main_ctx = create_context(&kernel, "parent").await.unwrap();
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
        let (child_ctx, admitted_id) = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no TurnStarted arrived — the fork's turn request never announced"
            );
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ServerEvent::TurnStarted { context_id, turn_id, .. })) if context_id != main_ctx => {
                    break (context_id, turn_id);
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("turn push channel error: {e}"),
                Err(_) => panic!("timed out waiting for the child's TurnStarted"),
            }
        };

        // Now that the child's context is known, the outcome push must name
        // that SAME context and be tagged Autonomous.
        match recv_turn_event(&mut rx, child_ctx).await {
            ServerEvent::TurnCompleted { context_id, turn_id, origin, .. } => {
                assert_eq!(turn_id, admitted_id);
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

#[test]
fn overlapping_drives_keep_their_admission_ids_through_cancellation() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "overlapping-turns").await.unwrap();
        assign_turn_identity(&server, context, PrincipalId::new());
        kernel.join_context(context, "turn-identity").await.unwrap();
        let help = kernel.execute_kj_quiet(context, &["drive".into(), "--help".into()]).await.unwrap();
        assert_eq!(help.exit_code, 0, "{}", help.stderr);
        assert!(help.stdout.contains("turn ID"));
        eprintln!("{}", help.stdout);
        let (callback, mut events) = turn_events_channel(64);
        kernel.subscribe_turn_events(callback).await.unwrap();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let mut admitted = std::collections::HashSet::new();
        for prompt in ["first", "second"] {
            let result = kernel.execute_kj_quiet(context, &[
                "drive".into(), "--prompt".into(), prompt.into(),
            ]).await.unwrap();
            assert_eq!(result.exit_code, 0, "{}", result.stderr);
            let id: kaijutsu_types::TurnId = serde_json::from_value(result.data.unwrap()["turn_id"].clone()).unwrap();
            assert!(admitted.insert(id), "each drive admits a distinct turn");
        }
        let mut started = std::collections::HashSet::new();
        while started.len() != admitted.len() {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            if let ServerEvent::TurnStarted { turn_id, context_id, .. } = event {
                assert_eq!(context_id, context);
                assert!(admitted.contains(&turn_id));
                assert!(started.insert(turn_id));
            }
        }
        assert_eq!(server.kernel.turns().active_count(context), 2);
        assert!(kernel.interrupt_context(context, true).await.unwrap());
        for _ in 0..2 {
            match recv_turn_event(&mut events, context).await {
                ServerEvent::TurnCompleted { turn_id, stop_reason, output_block_id, .. } => {
                    assert!(admitted.remove(&turn_id), "completion must settle one original admission");
                    assert_eq!(stop_reason, TurnCompletedStopReason::CancelledImmediate);
                    assert!(output_block_id.is_none(), "queued cancellation must not enter inference");
                }
                other => panic!("expected cancellation, got {other:?}"),
            }
        }
        assert!(admitted.is_empty());
        assert!(!server.kernel.turn_in_flight(context));
        drop(held);
    });
}

#[test]
fn timed_drives_keep_admission_targets_through_the_client() {
    run_local(async {
        use kaijutsu_hyoushigi::{Disposition, FallbackReason, TickDelta, WorkId, WorkStatus};
        use kaijutsu_kernel::{hyoushigi::{Attachment, BeatPolicy}, llm::{MockClient, Provider, stream::StreamEvent}};
        use kaijutsu_server::beat::BeatScheduler;
        use kaijutsu_types::{Tick, TrackId, TurnId};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "timed-player").await.unwrap();
        let performer = PrincipalId::new();
        assign_turn_identity(&server, context, performer);
        kj.join_context(context, "timed-player").await.unwrap();
        let (callback, mut events) = turn_events_channel(64);
        kj.subscribe_turn_events(callback).await.unwrap();
        let help = kj.execute_kj_quiet(context, &["drive".into(), "--help".into()]).await.unwrap();
        assert_eq!(help.exit_code, 0);
        for flag in ["--score-at", "--track", "--fallback"] { assert!(help.stdout.contains(flag)); }
        eprintln!("{}", help.stdout);
        let track = TrackId::new("admitted-score").unwrap();
        let mut scheduler = BeatScheduler::new(server.kernel.clone(), server.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1), beats_per_phrase: 16,
        }).unwrap();
        let timeline = server.kernel.track_timeline(&track).unwrap();
        let score = server.kernel_db.lock().get_track(track.as_str()).unwrap().unwrap().score_context_id.unwrap();
        let base = tokio::time::Instant::now();
        scheduler.play(&track, base);
        let abc = "X:1\nK:C\nCDEF|\n";
        let mut beat = 0u64;
        for case in ["good", "stale", "missed", "malformed", "closed_eof", "untimed"] {
            {
                let mut registry = server.kernel.llm().write().await;
                let mock = if case == "closed_eof" {
                    MockClient::new("").with_scripted_stream(vec![vec![StreamEvent::TextStart,
                        StreamEvent::TextDelta(abc.into()), StreamEvent::TextEnd]])
                } else { MockClient::new(if case == "malformed" { "not music" } else { abc }) };
                registry.register("mock", std::sync::Arc::new(Provider::Mock(mock)));
                assert!(registry.set_default("mock"));
                registry.set_default_model("mock-model");
            }
            let before = kj.get_blocks(score, &BlockQuery::All).await.unwrap().len();
            let admitted_at = timeline.lock().playhead();
            let intended = admitted_at + TickDelta::new(6);
            let session = server.kernel.turns().conversations().get_or_create(context);
            let held = session.lock().await;
            let mut args = vec!["drive".into(), "--prompt".into(), format!("phrase {case}")];
            if case != "untimed" {
                args.extend(["--track".into(), track.as_str().into(), "--score-at".into(), intended.get().to_string(),
                    "--fallback".into(), "last-good".into()]);
            }
            let result = kj.execute_kj_quiet(context, &args).await.unwrap();
            assert_eq!(result.exit_code, 0, "{}", result.stderr);
            let data = result.data.unwrap();
            let turn: TurnId = serde_json::from_value(data["turn_id"].clone()).unwrap();
            let work: Option<WorkId> = serde_json::from_value(data["work_id"].clone()).unwrap();
            assert_eq!(work.is_some(), case != "untimed");
            let seed = server.documents.block_snapshots(context).unwrap().into_iter()
                .find(|block| block.content == format!("phrase {case}")).unwrap().id;
            let advance = if case == "missed" { 6 } else { 2 };
            for _ in 0..advance {
                beat += 1;
                scheduler.fire_due(base + Duration::from_secs(beat));
            }
            if let Some(work) = work {
                assert_eq!(timeline.lock().status(work).unwrap().start, intended);
                assert_eq!(timeline.lock().status(work).unwrap().started_at, Some(admitted_at));
            }
            drop(held);
            match recv_turn_event(&mut events, context).await {
                ServerEvent::TurnCompleted { turn_id, stop_reason, output_block_id, .. } => {
                    assert_eq!(turn_id, turn);
                    assert_ne!(case, "closed_eof", "missing terminal confirmation cannot complete");
                    if case == "missed" {
                        assert_eq!(stop_reason, TurnCompletedStopReason::CancelledImmediate);
                        assert!(output_block_id.is_none());
                    } else { assert!(output_block_id.is_some()); }
                }
                ServerEvent::TurnFailed { turn_id, error, .. } if case == "closed_eof" => {
                    assert_eq!(turn_id, turn);
                    assert!(error.starts_with("Invalid provider stream:"));
                    assert!(error.contains("before Done"));
                }
                other => panic!("unexpected controlled turn outcome for {case}: {other:?}"),
            }
            if case == "stale" { server.documents.set_excluded(context, &seed, true).unwrap(); }
            while timeline.lock().playhead() < intended + TickDelta::new(1) {
                beat += 1;
                scheduler.fire_due(base + Duration::from_secs(beat));
            }
            if let Some(work) = work {
                let result = kj.execute_kj_quiet(context, &[
                    "transport".into(), "work".into(), "--track".into(), track.as_str().into(),
                ]).await.unwrap();
                assert_eq!(result.exit_code, 0, "{}", result.stderr);
                let rows: Vec<WorkStatus> = serde_json::from_value(result.data.unwrap()).unwrap();
                let row = rows.iter().find(|row| row.id == work).unwrap();
                assert_eq!(row.start, intended);
                assert_eq!(row.attempt, 1);
                if case == "good" {
                    assert!(matches!(row.disposition, Some(Disposition::Committed { .. })), "{row:?}");
                } else {
                    let expected = match case {
                        "stale" => FallbackReason::InvalidBasis,
                        "missed" => FallbackReason::DeadlineMissed,
                        "malformed" | "closed_eof" => FallbackReason::ResolveFailed,
                        _ => unreachable!(),
                    };
                    assert!(matches!(&row.disposition, Some(Disposition::Fallback { reason, .. }) if *reason == expected), "{row:?}");
                }
            }
            let blocks = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
            if case == "untimed" { assert_eq!(blocks.len(), before); }
            else {
                let notation: Vec<_> = blocks.iter().filter(|block| block.tick == Some(intended) && block.content == abc).collect();
                assert_eq!(notation.len(), 1, "exactly one phrase at the admitted tick: {case}");
                assert_eq!(notation[0].id.principal_id, if case == "good" { performer } else { PrincipalId::beat() });
            }
            assert_eq!(timeline.lock().future_len(), 0);
        }
        // Run the shipped tick body through rc, with the same captured transport
        // variables the scheduler supplies. Nothing is reseeded on the host.
        server.kernel_db.lock().update_context_type(context, "musician").unwrap();
        let admission = server.kernel.admit_context(context).unwrap();
        let admitted_at = timeline.lock().playhead();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let vars = std::collections::HashMap::from([
            ("KJ_TRACK".into(), track.as_str().into()), ("KJ_TICK".into(), admitted_at.get().to_string()),
            ("KJ_PHRASE_BEATS".into(), "8".into()), ("KJ_TEMPO".into(), "120".into()),
            ("KJ_PHRASE".into(), "1".into()), ("KJ_HEARD".into(), "[]".into()),
        ]);
        kaijutsu_kernel::rc::run(&server.kj_dispatcher, kaijutsu_kernel::rc::RcInvocation {
            vars, ..kaijutsu_kernel::rc::RcInvocation::new("tick", &admission)
        }, &kaijutsu_kernel::KjCaller {
            principal_id: performer, actor_id: performer, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
        }).await.unwrap();
        assert_eq!(timeline.lock().future_len(), 1, "shipped tick script admits a score turn");
        assert!(server.documents.block_snapshots(context).unwrap().iter().any(|block|
            block.content.starts_with(&format!("Commit at tick {}.", admitted_at.get() + 8))));
        drop(held);
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));
        while timeline.lock().playhead() < admitted_at + TickDelta::new(9) {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let blocks = kj.get_blocks(score, &BlockQuery::All).await.unwrap();
        assert_eq!(blocks.iter().filter(|block| block.content == abc && block.tick == Some(admitted_at + TickDelta::new(8))).count(), 1);
        // Neither missing track nor a past target may start a producer.
        for args in [vec!["drive", "--score-at", "1"], vec!["drive", "--track", "admitted-score", "--score-at", "1"]] {
            let result = kj.execute_kj_quiet(context, &args.into_iter().map(str::to_string).collect::<Vec<_>>()).await.unwrap();
            assert_ne!(result.exit_code, 0);
            assert!(!server.kernel.turn_in_flight(context));
        }
        assert!(timeline.lock().playhead() > Tick::ZERO);
    });
}

#[test]
fn a_provider_panic_publishes_failure_after_its_open_blocks_settle() {
    run_local(async {
        use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};
        use kaijutsu_types::Status;
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "panicking-player").await.unwrap();
        assign_turn_identity(&server, context, PrincipalId::new());
        kj.join_context(context, "panic-cleanup").await.unwrap();
        let mock = MockClient::new("").with_scripted_stream(vec![vec![
            StreamEvent::TextStart, StreamEvent::TextDelta("partial output survives".into()),
        ]]).panics_when_exhausted();
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(mock)));
        let (callback, mut events) = turn_events_channel(32);
        kj.subscribe_turn_events(callback).await.unwrap();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let admitted = kj.execute_kj_quiet(context, &["drive".into(), "--prompt".into(), "begin".into()]).await.unwrap();
        assert_eq!(admitted.exit_code, 0, "{}", admitted.stderr);
        let id: kaijutsu_types::TurnId = serde_json::from_value(admitted.data.unwrap()["turn_id"].clone()).unwrap();
        drop(held);
        match recv_turn_event(&mut events, context).await {
            ServerEvent::TurnFailed { turn_id, error, .. } => {
                assert_eq!(turn_id, id);
                assert!(error.contains("panicked"));
            }
            other => panic!("panic must fail the admitted turn: {other:?}"),
        }
        let blocks = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
        assert_eq!(blocks.iter().find(|b| b.content == "partial output survives").unwrap().status, Status::Error);
        assert!(!blocks.iter().any(|b| b.status == Status::Running));
        assert!(!server.kernel.turn_in_flight(context));
        assert!(server.kernel.shutdown_runtime_worker().await.is_err());
    });
}

#[test]
fn tool_failure_reaches_the_client_with_its_result_and_error_flag() {
    run_local(async {
        use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};
        use kaijutsu_types::{BlockKind, Status};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "tool-error-player").await.unwrap();
        let performer = PrincipalId::new();
        assign_turn_identity(&server, context, performer);
        kj.join_context(context, "tool-error-settlement").await.unwrap();
        let done = || StreamEvent::Done { stop_reason: Some("end_turn".into()), input_tokens: None, output_tokens: None, extra: None };
        let mock = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "wire-tool-error".into(), name: "missing_tool".into(), input: serde_json::json!({}) }, done()],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("The tool is unavailable.".into()), StreamEvent::TextEnd, done()],
        ]);
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(mock)));
        let (callback, mut events) = turn_events_channel(32);
        kj.subscribe_turn_events(callback).await.unwrap();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let admitted = kj.execute_kj_quiet(context, &["drive".into(), "--prompt".into(), "use the tool".into()]).await.unwrap();
        assert_eq!(admitted.exit_code, 0, "{}", admitted.stderr);
        drop(held);
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));
        let blocks = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
        let pair: Vec<_> = blocks.iter().filter(|block| block.tool_use_id.as_deref() == Some("wire-tool-error")).collect();
        assert_eq!(pair.len(), 2);
        assert!(pair.iter().all(|block| block.status == Status::Error));
        let call = pair.iter().find(|block| block.kind == BlockKind::ToolCall).unwrap();
        let result = pair.iter().find(|block| block.kind == BlockKind::ToolResult).unwrap();
        assert_eq!(call.id.principal_id, performer);
        assert_eq!(result.id.principal_id, PrincipalId::system());
        assert!(result.is_error);
        assert!(!result.content.is_empty());
        assert!(!blocks.iter().any(|block| block.status == Status::Running));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[test]
fn authored_tool_results_preserve_their_initial_status() {
    run_local(async {
        use kaijutsu_client::AuthorBlock;
        use kaijutsu_types::{BlockKind, Role, Status};
        let (addr, _server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "reserved-result").await.unwrap();
        kj.join_context(context, "result-status").await.unwrap();
        let author = PrincipalId::new();
        for status in [Status::Running, Status::Waiting, Status::Done, Status::Error] {
            let call = kj.author_block(&AuthorBlock::tool_call(context, author, "external", serde_json::json!({}), None)).await.unwrap();
            let mut request = AuthorBlock::text(context, author, Role::Tool, "output");
            request.kind = BlockKind::ToolResult;
            request.parent_id = Some(call);
            request.status = status;
            let result = kj.author_block(&request).await.unwrap();
            let blocks = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
            let output = blocks.iter().find(|block| block.id == result).unwrap();
            assert_eq!(output.status, status);
            assert_eq!(output.is_error, status == Status::Error);
            let final_status = if status == Status::Error { Status::Done } else { Status::Error };
            kj.complete_block(context, &result, final_status, final_status == Status::Error, Some(9)).await.unwrap();
            let completed = kj.get_blocks(context, &BlockQuery::All).await.unwrap();
            let output = completed.iter().find(|block| block.id == result).unwrap();
            assert_eq!(output.status, final_status);
            assert_eq!(output.is_error, final_status == Status::Error);
            assert_eq!(output.exit_code, Some(9));
        }
    });
}

#[test]
fn performer_reassignment_rejects_a_queued_inference_over_the_wire() {
    run_local(async {
        use kaijutsu_kernel::llm::{MockClient, Provider};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "reassigned-turn").await.unwrap();
        let performer = PrincipalId::new();
        let replacement = PrincipalId::new();
        {
            let db = server.kernel_db.lock();
            let requester = db.get_context(context).unwrap().unwrap().created_by;
            for (principal_id, name) in [(performer, "first-performer"), (replacement, "next-performer")] {
                db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
                    principal_id, name: name.into(), created_at: 0, retired_at: None,
                    handoff_ctx: None, root_ctx: None, root: false,
                }).unwrap();
            }
            db.update_context_review(context, Some(performer), Some(requester)).unwrap();
        }
        {
            let mut registry = server.kernel.llm().write().await;
            registry.register("mock", std::sync::Arc::new(Provider::Mock(
                MockClient::new("").with_scripted_stream(vec![]))));
        }
        let (callback, mut events) = turn_events_channel(64);
        kj.subscribe_turn_events(callback).await.unwrap();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        kj.prompt("queued for the first performer", None, context).await.unwrap();
        let epoch = server.kernel_db.lock().continuation_epoch(context).unwrap()
            .expect("accepted prompt opens its continuation before returning");
        let result = kj.execute_kj_quiet(context, &[
            "context".into(), "set".into(), ".".into(), "--as".into(), "next-performer".into(),
        ]).await.unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(server.kernel_db.lock().continuation_epoch(context).unwrap(), None);
        drop(held);
        match recv_turn_event(&mut events, context).await {
            ServerEvent::TurnFailed { error, .. } => assert!(error.contains("continuation closed"), "{error}"),
            other => panic!("reassigned inference must not run: {other:?}"),
        }
        assert!(!server.kernel_db.lock().record_continuation_request(context, epoch, kaijutsu_types::now_millis() as i64).unwrap());
        assert!(!server.kernel.turn_in_flight(context));
        let blocks = get_all_blocks(&kj, context).await;
        assert!(!blocks.iter().any(|block| block.role == Role::Model && block.kind == BlockKind::Text));
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

#[test]
fn context_wait_over_rpc_keeps_waiting_for_overlapping_turns() {
    run_local(async {
        let (addr, shared) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kernel, "wait-target").await.unwrap();
        let observer = create_context(&kernel, "wait-observer").await.unwrap();
        let first = shared.kernel.turns().begin(context);
        let last = shared.kernel.turns().begin(context);
        let baseline = shared.kernel.turn_flows().topic_subscribers("turn.completed");
        let args = ["wait".into(), "--timeout".into(), "10".into(), context.to_hex()];
        let mut wait = Box::pin(kernel.execute_kj_quiet(observer, &args));
        tokio::select! {
            result = &mut wait => panic!("context wait returned before its turns: {result:?}"),
            subscribed = tokio::time::timeout(Duration::from_secs(3), async {
                while shared.kernel.turn_flows().topic_subscribers("turn.completed") <= baseline {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }) => subscribed.expect("wire wait must subscribe before events are published"),
        }
        let first_id = first.id();
        drop(first);
        shared.kernel.turn_flows().publish(kaijutsu_kernel::flows::TurnFlow::Completed {
            turn_id: first_id, context_id: context, principal_id: PrincipalId::system(), output_block_id: None,
            reason: kaijutsu_kernel::flows::TurnStopReason::EndTurn, origin: Default::default(),
        });
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut wait).await.is_err(),
            "the actual client must not see context completion while another accepted turn remains");
        let last_id = last.id();
        drop(last);
        shared.kernel.turn_flows().publish(kaijutsu_kernel::flows::TurnFlow::Failed {
            turn_id: last_id, context_id: context, principal_id: PrincipalId::system(),
            error: "last controlled turn failed".into(), origin: Default::default(),
        });
        let result = tokio::time::timeout(Duration::from_secs(5), wait).await.unwrap().unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        let data = result.data.unwrap();
        assert_eq!(data["status"], "failed");
        assert_eq!(data["detail"], "last controlled turn failed");
        assert_eq!(data["resolved_by"], "event");
        let help = kernel.execute_kj_quiet(observer, &["wait".into(), "--help".into()]).await.unwrap();
        assert_eq!(help.exit_code, 0, "{}", help.stderr);
        println!("{}", help.stdout);
        drop(kernel);
        drop(client);
        shared.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

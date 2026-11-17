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
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None,
            principal_id: actor,
            name: "turn-actor".into(),
            created_at: 0,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
    }
    db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None,
        principal_id: reviewer,
        name: "turn-reviewer".into(),
        created_at: 0,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
    })
    .unwrap();
    db.assign_test_review_recasting(context, Some(actor), Some(reviewer))
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
            .prompt("write a phrase", ctx)
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
            .prompt("write a phrase", ctx)
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
        common::perform_with_rc_dir(&server, context, "musician");
        let admission = server.kernel.admit_context(context).unwrap();
        let admitted_at = timeline.lock().playhead();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let vars = std::collections::HashMap::from([
            ("KJ_TRACK".into(), track.as_str().into()), ("KJ_TICK".into(), admitted_at.get().to_string()),
            ("KJ_PHRASE_BEATS".into(), "8".into()), ("KJ_TEMPO".into(), "120".into()),
            ("KJ_PHRASE".into(), "1".into()), ("KJ_HEARD".into(), "[]".into()),
            ("KJ_COST_MS".into(), "0".into()), ("KJ_LEAD_BEATS".into(), "0".into()),
            ("KJ_OPEN_WORK".into(), "0".into()),
        ]);
        let cancel = tokio_util::sync::CancellationToken::new();
        kaijutsu_kernel::rc::run(&server.kj_dispatcher, kaijutsu_kernel::rc::RcInvocation {
            vars, ..kaijutsu_kernel::rc::RcInvocation::new("tick", &admission, &cancel)
        }, &kaijutsu_kernel::KjCaller {
            principal_id: performer, actor_id: performer, reviewer_id: None, context_id: Some(context),
            session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
            cancel,
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
            vec![StreamEvent::ToolUse { id: "wire-tool-error".into(), name: "missing_tool".into(), input: serde_json::json!({}), signature: None }, done()],
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

/// A later hydration of a turn's log sends the bytes the turn sent. A shell
/// result reaches the model as its JSON envelope while its block shows people
/// the readable output, so the block must also keep what was sent
/// (`docs/conversation-session.md`, "Tool results replay as sent").
#[test]
fn a_shell_result_rehydrates_as_the_model_received_it() {
    run_local(async {
        use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "shell-replay").await.unwrap();
        assign_turn_identity(&server, context, PrincipalId::new());
        kj.join_context(context, "shell-replay").await.unwrap();
        let done = |stop: &str| StreamEvent::Done { stop_reason: Some(stop.into()), input_tokens: None, output_tokens: None, extra: None };
        let (mock, sent) = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "replay-shell".into(), name: "shell".into(),
                input: serde_json::json!({"command": "echo hello from kaish"}), signature: None }, done("tool_use")],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("ack".into()), StreamEvent::TextEnd, done("end_turn")],
        ]).recording_sent_messages();
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(mock)));
        let (callback, mut events) = turn_events_channel(32);
        kj.subscribe_turn_events(callback).await.unwrap();
        let admitted = kj.execute_kj_quiet(context, &["drive".into(), "--prompt".into(), "run it".into()]).await.unwrap();
        assert_eq!(admitted.exit_code, 0, "{}", admitted.stderr);
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));

        let sent = sent.lock().clone();
        assert_eq!(sent.len(), 2);
        let wire = serde_json::to_string(&sent[1]).unwrap();
        assert!(wire.contains("hello from kaish"), "the shell ran and its output reached the model: {wire}");
        // A clean success reaches the model as its output alone
        // (docs/shell-envelope.md, "What a model turn reads"), so the block
        // stores that text once and keeps the envelope as its record.
        let result = server.kernel.blocks().block_snapshots(context).unwrap().into_iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_use_id.as_deref() == Some("replay-shell"))
            .expect("the shell result block");
        let sent_result = sent[1].iter().rev().find_map(|m| match &m.content {
            kaijutsu_kernel::llm::MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                kaijutsu_kernel::llm::ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            }),
            _ => None,
        }).expect("the request carries the tool result");
        assert_eq!(sent_result, "hello from kaish\n");
        assert_eq!(result.content, "hello from kaish\n");
        assert_eq!(result.model_content, None, "output equal to content is stored once");
        let envelope: serde_json::Value = serde_json::from_str(result.shell_envelope.as_deref()
            .expect("the kaish record is kept")).unwrap();
        assert_eq!(envelope["status"], "done");
        assert_eq!(envelope["exit_code"], 0);
        assert_eq!(envelope["stdout"], "", "the record does not store the output a second time");
        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();
        mailbox.catch_up(&server.kernel.blocks().block_snapshots(context).unwrap());
        let mut expected = sent[1].clone();
        expected.push(kaijutsu_kernel::llm::Message::assistant("ack"));
        let replayed: Vec<serde_json::Value> = mailbox.snapshot().iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        let expected: Vec<serde_json::Value> = expected.iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        if let Some(i) = (0..replayed.len().max(expected.len())).find(|&i| replayed.get(i) != expected.get(i)) {
            panic!("message {i} differs\nreplayed: {:#}\nsent: {:#}",
                replayed.get(i).unwrap_or(&serde_json::Value::Null), expected.get(i).unwrap_or(&serde_json::Value::Null));
        }
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

/// A failing shell call reaches the model as its output with stderr marked,
/// then `[exit N]`, and nothing else: the facts are already text, so the
/// envelope's JSON is not appended a second time (`docs/shell-envelope.md`,
/// "What a model turn reads"). Hydration replays the same text.
#[test]
fn a_failing_shell_result_marks_stderr_and_sends_no_json() {
    run_local(async {
        use kaijutsu_kernel::llm::{MockClient, Provider, stream::StreamEvent};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "shell-stderr").await.unwrap();
        assign_turn_identity(&server, context, PrincipalId::new());
        kj.join_context(context, "shell-stderr").await.unwrap();
        let done = |stop: &str| StreamEvent::Done { stop_reason: Some(stop.into()), input_tokens: None, output_tokens: None, extra: None };
        let (mock, sent) = MockClient::new("").with_scripted_stream(vec![
            vec![StreamEvent::ToolUse { id: "stderr-shell".into(), name: "shell".into(),
                input: serde_json::json!({"command": "echo on stdout; cat /no-such-file-zq"}), signature: None }, done("tool_use")],
            vec![StreamEvent::TextStart, StreamEvent::TextDelta("ack".into()), StreamEvent::TextEnd, done("end_turn")],
        ]).recording_sent_messages();
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(mock)));
        let (callback, mut events) = turn_events_channel(32);
        kj.subscribe_turn_events(callback).await.unwrap();
        let admitted = kj.execute_kj_quiet(context, &["drive".into(), "--prompt".into(), "run it".into()]).await.unwrap();
        assert_eq!(admitted.exit_code, 0, "{}", admitted.stderr);
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));

        let sent = sent.lock().clone();
        assert_eq!(sent.len(), 2);
        let (sent_result, is_error) = sent[1].iter().rev().find_map(|m| match &m.content {
            kaijutsu_kernel::llm::MessageContent::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                kaijutsu_kernel::llm::ContentBlock::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
                _ => None,
            }),
            _ => None,
        }).expect("the request carries the tool result");
        assert!(is_error, "a nonzero exit rides the error flag: {sent_result}");
        assert!(sent_result.starts_with("on stdout\n[stderr]\n"), "stdout first, then the marker: {sent_result:?}");
        assert!(sent_result.contains("no-such-file-zq"), "stderr follows the marker: {sent_result:?}");
        let last = sent_result.lines().last().unwrap();
        assert!(last.starts_with("[exit ") && last.ends_with(']'), "the exit fact ends the result: {sent_result:?}");
        assert!(!sent_result.contains("<error"), "no error envelope: {sent_result:?}");
        assert!(!sent_result.contains("\"exit_code\""), "no JSON: {sent_result:?}");

        let mut mailbox = kaijutsu_kernel::ConversationMailbox::new();
        mailbox.catch_up(&server.kernel.blocks().block_snapshots(context).unwrap());
        let mut expected = sent[1].clone();
        expected.push(kaijutsu_kernel::llm::Message::assistant("ack"));
        let replayed: Vec<serde_json::Value> = mailbox.snapshot().iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        let expected: Vec<serde_json::Value> = expected.iter().map(|m| serde_json::to_value(m).unwrap()).collect();
        if let Some(i) = (0..replayed.len().max(expected.len())).find(|&i| replayed.get(i) != expected.get(i)) {
            panic!("message {i} differs\nreplayed: {:#}\nsent: {:#}",
                replayed.get(i).unwrap_or(&serde_json::Value::Null), expected.get(i).unwrap_or(&serde_json::Value::Null));
        }
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

/// The page turn runs the shipped `rotate` rc through the server's beat
/// scheduler: the successor keeps the label and the track, runs the `create`
/// rc again, and takes over the tick; the predecessor is archived.
#[test]
fn the_page_turn_replaces_a_musician_with_a_fully_set_successor_on_the_same_track() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let performer = common::rc_performer(&kj, "musician").await.unwrap();
        let label = "page-turner";
        let original = common::create_context_as(&kj, label, &performer).await.unwrap();
        let operator = kaijutsu_client::choose_parent(None, &kj.list_contexts().await.unwrap()).unwrap().context_id;
        let transport = |args: &[&str]| {
            let mut argv = vec!["transport".to_string()];
            argv.extend(args.iter().map(|arg| arg.to_string()));
            argv
        };
        let original_hex = original.to_string();
        // The create rc attached the context to the lane its label names.
        let track = label;
        for argv in [
            transport(&["tempo", "1200", "--track", track]),
            transport(&["rotate", "--every", "2", "--context", &original_hex, "--track", track]),
            transport(&["play", "--track", track]),
        ] {
            let result = kj.execute_kj_quiet(operator, &argv).await.unwrap();
            assert_eq!(result.exit_code, 0, "{argv:?}: {}{}", result.stdout, result.stderr);
        }
        // Two phrases of 32 beats at 50 ms each reach the rotate horizon in 3.2 s.
        let successor = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let rows = kj.list_contexts().await.unwrap();
                if let Some(row) = rows.iter().find(|row| row.id != original && !row.archived
                    && (row.label == label || row.forked_from == Some(original)))
                {
                    return row.clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.expect("the rotate horizon produces a live successor");
        assert_eq!(successor.label, label, "the successor keeps the label");
        let successor = successor.id;
        // One page turn is the subject: clear the cadence the successor
        // inherited, well before its own horizon 3.2 s later.
        let successor_hex = successor.to_string();
        let off = transport(&["rotate", "off", "--context", &successor_hex, "--track", track]);
        let result = kj.execute_kj_quiet(operator, &off).await.unwrap();
        assert_eq!(result.exit_code, 0, "{off:?}: {}{}", result.stdout, result.stderr);
        let rows = kj.list_contexts().await.unwrap();
        assert!(rows.iter().find(|row| row.id == original).expect("the original is retained").archived,
            "the predecessor is archived");
        assert_eq!(rows.iter().filter(|row| row.label == label && !row.archived).count(), 1);

        let blocks = kj.get_blocks(successor, &BlockQuery::All).await.unwrap();
        for (name, text) in [
            ("stance", include_str!("../../../assets/defaults/rc/musician/create/S00-stance.md")),
            ("chair", include_str!("../../../assets/defaults/rc/musician/chairs/bass.md")),
            ("primer", include_str!("../../../assets/defaults/rc/musician/create/S15-abc-primer.md")),
        ] {
            let first_line = text.lines().next().unwrap();
            assert!(blocks.iter().any(|block| block.content.contains(first_line)),
                "the successor carries the {name} block ({first_line:?})");
        }
        assert!(blocks.iter().any(|block| block.content.starts_with("We are a cybernetic system making music together.")),
            "the successor carries the stance");

        let attached: Vec<String> = server.kernel_db.lock().list_attachments_for_context(successor).unwrap()
            .into_iter().map(|row| row.track_id).collect();
        assert_eq!(attached, vec![track.to_string()], "the successor is attached to the same track");

        // The successor's own tick admits work after the page turn.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let blocks = kj.get_blocks(successor, &BlockQuery::All).await.unwrap();
                if blocks.iter().any(|block| block.content.starts_with("Commit at tick ")) { return; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.expect("the successor's tick lifecycle admits a phrase");
        let listed = kj.execute_kj_quiet(operator, &transport(&["list"])).await.unwrap();
        assert!(listed.stdout.contains(track), "{}", listed.stdout);
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

/// Create `label` from the shipped `musician` rc directory with `--env` pairs,
/// returning the new context.
async fn create_musician_with_env(
    kj: &KernelHandle,
    performer: &str,
    label: &str,
    env: &[&str],
) -> ContextId {
    let contexts = kj.list_contexts().await.unwrap();
    let parent = kaijutsu_client::choose_parent(None, &contexts).unwrap();
    let mut argv = kaijutsu_client::context_create_argv(label, performer, None);
    for pair in env {
        argv.push("--env".to_string());
        argv.push(pair.to_string());
    }
    let result = kj.execute_kj_quiet(parent.context_id, &argv).await.unwrap();
    kaijutsu_client::context_id_from_create_result(&result).unwrap_or_else(|e| panic!("create {label}: {e}"))
}

/// Create the character `name` with the shipped `musician` rc directory from
/// the kernel's only root context, as `kj character create` does for a chair.
async fn musician_character(kj: &KernelHandle, name: &str) -> String {
    let contexts = kj.list_contexts().await.unwrap();
    let root = kaijutsu_client::choose_parent(None, &contexts).unwrap();
    let argv = ["character", "create", name, "--rc", "musician"].map(String::from).to_vec();
    let result = kj.execute_kj_quiet(root.context_id, &argv).await.unwrap();
    assert_eq!(result.exit_code, 0, "kj character create {name}: {}", result.stderr);
    name.to_string()
}

/// Two musicians made from the one shipped `musician` rc directory, each
/// played by its own character, sit on one band track: `KJ_BAND` names the
/// track and `KJ_CHAIR` the chair, bass when unset. A second live context of
/// a character already on the track is refused, naming the consequence and
/// the fix. An unknown chair is an Error block that names it and the chairs
/// that exist. The page turn keeps the chair and the band.
#[test]
fn two_musicians_share_a_band_track_with_their_own_chairs() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let performer = common::rc_performer(&kj, "musician").await.unwrap();
        let drummer = musician_character(&kj, "drummer").await;
        let bass_chair = include_str!("../../../assets/defaults/rc/musician/chairs/bass.md");
        let drums_chair = include_str!("../../../assets/defaults/rc/musician/chairs/drums.md");
        let blocks_of = |context: ContextId| server.documents.block_snapshots(context).unwrap();
        let has_chair = |context: ContextId, chair: &str| {
            let first_line = chair.lines().next().unwrap();
            blocks_of(context).iter().any(|block| block.content.contains(first_line))
        };

        let bass = create_musician_with_env(&kj, &performer, "bass", &["KJ_BAND=band"]).await;
        let band_before = server.kernel_db.lock().get_track("band").unwrap().expect("the band track exists");
        let drums = create_musician_with_env(&kj, &drummer, "drums", &["KJ_BAND=band", "KJ_CHAIR=drums"]).await;

        assert!(has_chair(bass, bass_chair), "no KJ_CHAIR plays bass");
        assert!(!has_chair(bass, drums_chair), "bass carries no drum chair");
        assert!(has_chair(drums, drums_chair), "KJ_CHAIR=drums plays drums");
        assert!(!has_chair(drums, bass_chair), "drums carries no bass chair");

        let track_of = |context: ContextId| -> Vec<String> {
            server.kernel_db.lock().list_attachments_for_context(context).unwrap()
                .into_iter().map(|row| row.track_id).collect()
        };
        assert_eq!(track_of(bass), vec!["band".to_string()]);
        assert_eq!(track_of(drums), vec!["band".to_string()]);
        let band_after = server.kernel_db.lock().get_track("band").unwrap().unwrap();
        assert_eq!((band_after.period_ms, band_after.beats_per_phrase), (band_before.period_ms, band_before.beats_per_phrase),
            "a second attachment leaves the track's clock alone");
        assert_eq!(server.kernel_db.lock().list_tracks().unwrap().len(), 1, "one track, not one per label");

        // A second live context of bass's character on the band is refused:
        // the attach fails loudly and says why and what to do instead.
        let twin = create_musician_with_env(&kj, &performer, "twin", &["KJ_BAND=band"]).await;
        let errors: Vec<String> = blocks_of(twin).into_iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect();
        assert!(errors.iter().any(|e| e.contains("not attached to track 'band'") && e.contains(&bass.short())
                && e.contains("skip its tick") && e.contains("kj character create")),
            "the refusal names the context already there, the consequence and the fix: {errors:?}");
        assert!(track_of(twin).is_empty(), "the refused context has no attachment row");
        let live = kj.list_tracks().await.unwrap();
        let band = live.iter().find(|t| t.id.as_str() == "band").expect("the band track is live");
        assert!(!band.attached.contains(&twin) && band.attached.contains(&bass) && band.attached.contains(&drums),
            "only bass and drums are attached: {:?}", band.attached);

        // An unknown chair fails the chair script loudly and names the choices.
        let kazoo = create_musician_with_env(&kj, &drummer, "kazoo", &["KJ_CHAIR=kazoo"]).await;
        let errors: Vec<String> = blocks_of(kazoo).into_iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect();
        assert!(errors.iter().any(|e| e.contains("kazoo") && e.contains("bass") && e.contains("drums")),
            "an unknown chair is an Error block naming it and the chairs that exist: {errors:?}");
        assert!(!has_chair(kazoo, bass_chair) && !has_chair(kazoo, drums_chair), "an unknown chair seats no chair");

        // A plain `kj context rotate` carries the env: the successor keeps the
        // chair and the band. The predecessor leaves the track before the
        // successor, played by the same character, attaches.
        common::context_verb(&kj, "rotate", drums, &[]).await;
        let successor = kj.list_contexts().await.unwrap().into_iter()
            .find(|row| row.label == "drums" && !row.archived && row.id != drums).expect("a live successor").id;
        assert!(has_chair(successor, drums_chair), "the successor plays drums");
        assert!(!has_chair(successor, bass_chair));
        assert_eq!(track_of(successor), vec!["band".to_string()], "the successor re-attaches to the band");
        let live = kj.list_tracks().await.unwrap();
        let band = live.iter().find(|t| t.id.as_str() == "band").unwrap();
        assert!(band.attached.contains(&successor) && !band.attached.contains(&drums),
            "the successor, not its predecessor, is live on the band: {:?}", band.attached);
        let errors: Vec<String> = blocks_of(successor).into_iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect();
        assert!(errors.is_empty(), "the successor's create lifecycle raised no error: {errors:?}");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

/// Archiving an attached context removes it from the scheduler's live
/// attachments, so another context of the same character can take its chair.
#[test]
fn archiving_an_attached_musician_frees_its_chair_on_the_track() {
    run_local(async {
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let performer = common::rc_performer(&kj, "musician").await.unwrap();
        let errors_of = |context: ContextId| -> Vec<String> {
            server.documents.block_snapshots(context).unwrap().into_iter()
                .filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect()
        };
        let live_on_band = || async {
            let live = kj.list_tracks().await.unwrap();
            live.iter().find(|t| t.id.as_str() == "band").map(|t| t.attached.clone()).unwrap_or_default()
        };

        let first = create_musician_with_env(&kj, &performer, "first", &["KJ_BAND=band"]).await;
        assert!(live_on_band().await.contains(&first), "the first musician is live on the band");

        common::context_verb(&kj, "archive", first, &["--confirm"]).await;
        assert!(!live_on_band().await.contains(&first), "an archived context leaves the live attachments");

        let second = create_musician_with_env(&kj, &performer, "second", &["KJ_BAND=band"]).await;
        assert!(errors_of(second).is_empty(), "the freed chair is taken without a refusal: {:?}", errors_of(second));
        assert!(live_on_band().await.contains(&second), "the second musician is live on the band");
        server.kernel.shutdown_runtime_worker().await.unwrap();
    });
}

/// Run the shipped `tick` rc with the variables the scheduler snapshots for the
/// context, and return them.
async fn run_shipped_tick(
    server: &kaijutsu_server::SharedKernel,
    scheduler: &kaijutsu_server::beat::BeatScheduler,
    context: ContextId,
    performer: PrincipalId,
) -> std::collections::HashMap<String, String> {
    let vars = scheduler.transport_env(context);
    let admission = server.kernel.admit_context(context).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    kaijutsu_kernel::rc::run(&server.kj_dispatcher, kaijutsu_kernel::rc::RcInvocation {
        vars: vars.clone(), ..kaijutsu_kernel::rc::RcInvocation::new("tick", &admission, &cancel)
    }, &kaijutsu_kernel::KjCaller {
        principal_id: performer, actor_id: performer, reviewer_id: None, context_id: Some(context),
        session_id: kaijutsu_types::SessionId::new(), confirmed: false, rc_depth: 0, privileged: false,
        cancel,
    }).await.unwrap();
    vars
}

/// Poll the timeline at its own playhead, without advancing it, until `work`
/// is ready: the model turn's output is prepared off the beat path.
async fn wait_until_ready(timeline: &kaijutsu_kernel::hyoushigi::SharedTimeline, work: kaijutsu_hyoushigi::WorkId) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            {
                let mut timeline = timeline.lock();
                let at = timeline.playhead();
                timeline.advance_to(at);
                if timeline.status(work).unwrap().readiness == kaijutsu_hyoushigi::Readiness::Ready { return; }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the phrase is prepared before its deadline");
}

/// A performer slower than one phrase is planned for by what it measured: after
/// a missed deadline the shipped tick script targets two phrases ahead and the
/// work shows the estimate, and a tick that fires while that performer's work is
/// still open admits nothing. Each phrase before the target gets a requested
/// repeat (`KJ_UNCOVERED` unset means `repeat`), which the transport plays as
/// a copy of the performer's last phrase.
#[test]
fn a_slow_performer_gets_a_later_target_and_no_second_open_phrase() {
    run_local(async {
        use kaijutsu_hyoushigi::{Disposition, FallbackReason, WorkStatus};
        use kaijutsu_kernel::{hyoushigi::{Attachment, BeatPolicy}, llm::{MockClient, Provider}};
        use kaijutsu_server::beat::BeatScheduler;
        use kaijutsu_types::{ContentType, TrackId};
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "slow-player").await.unwrap();
        let performer = PrincipalId::new();
        assign_turn_identity(&server, context, performer);
        kj.join_context(context, "slow-player").await.unwrap();
        let (callback, mut events) = turn_events_channel(64);
        kj.subscribe_turn_events(callback).await.unwrap();
        let abc = "X:1\nK:C\nCDEF|\n";
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(MockClient::new(abc))));
        let track = TrackId::new("slow-score").unwrap();
        let mut scheduler = BeatScheduler::new(server.kernel.clone(), server.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1), beats_per_phrase: 4,
        }).unwrap();
        let timeline = server.kernel.track_timeline(&track).unwrap();
        let base = tokio::time::Instant::now();
        scheduler.play(&track, base);
        common::perform_with_rc_dir(&server, context, "musician");
        let mut beat = 0u64;

        let commits = || server.documents.block_snapshots(context).unwrap().into_iter()
            .filter(|block| block.content.starts_with("Commit at tick ")).count();
        let score = server.kernel_db.lock().get_track(track.as_str()).unwrap().unwrap().score_context_id.unwrap();

        // A phrase the performer plays in time; a repeat later copies it.
        let warm = run_shipped_tick(&server, &scheduler, context, performer).await;
        assert_eq!(warm["KJ_COST_MS"], "0");
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));
        let warm_work = timeline.lock().statuses()[0].id;
        wait_until_ready(&timeline, warm_work).await;
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        assert!(matches!(timeline.lock().status(warm_work).unwrap().disposition, Some(Disposition::Committed { .. })));

        // A tick with a lead under one phrase targets one phrase ahead. The turn
        // waits on the conversation lock, so the work misses its deadline.
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        let first = run_shipped_tick(&server, &scheduler, context, performer).await;
        assert!(first["KJ_LEAD_BEATS"].parse::<i64>().unwrap() < 4, "{first:?}");
        assert_eq!(first["KJ_OPEN_WORK"], "0");
        let first_tick: i64 = first["KJ_TICK"].parse().unwrap();
        assert_eq!(timeline.lock().future_len(), 1);
        assert!(server.documents.block_snapshots(context).unwrap().iter()
            .any(|block| block.content.starts_with(&format!("Commit at tick {}.", first_tick + 4))));
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        drop(held);
        assert!(matches!(recv_turn_event(&mut events, context).await,
            ServerEvent::TurnCompleted { stop_reason: TurnCompletedStopReason::CancelledImmediate, .. }));
        assert!(timeline.lock().measured_cost(&kaijutsu_hyoushigi::ResolverId::new("model_output"), performer).is_some(),
            "the missed deadline left a measured lower bound");

        // The next tick plans for it: the target is at least two phrases ahead.
        let second = run_shipped_tick(&server, &scheduler, context, performer).await;
        let cost_ms: u64 = second["KJ_COST_MS"].parse().unwrap();
        let lead: i64 = second["KJ_LEAD_BEATS"].parse().unwrap();
        assert!(cost_ms > 0 && lead >= 4, "{second:?}");
        assert_eq!(second["KJ_OPEN_WORK"], "0");
        let second_tick: i64 = second["KJ_TICK"].parse().unwrap();
        let phrases = lead / 4 + 1;
        let target = second_tick + phrases * 4;
        assert!(target >= second_tick + 8);
        assert!(server.documents.block_snapshots(context).unwrap().iter()
            .any(|block| block.content.starts_with(&format!("Commit at tick {target}."))));
        assert_eq!(timeline.lock().future_len() as i64, phrases, "the model's work and one repeat per uncovered phrase");
        assert_eq!(timeline.lock().open_work(performer), 1, "a repeat is not open work");
        assert!(matches!(recv_turn_event(&mut events, context).await, ServerEvent::TurnCompleted { .. }));
        let result = kj.execute_kj_quiet(context, &[
            "transport".into(), "work".into(), "--track".into(), track.as_str().into(),
        ]).await.unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        let rows: Vec<WorkStatus> = serde_json::from_value(result.data.unwrap()).unwrap();
        let admitted = rows.iter().find(|row| row.start.get() == target).expect("the new work is listed");
        assert!(admitted.estimate_wall > Duration::ZERO, "{admitted:?}");
        assert!(admitted.estimate.get() > 0, "{admitted:?}");
        let uncovered = second_tick + 4;
        for at in (1..phrases).map(|i| second_tick + i * 4) {
            let repeat = rows.iter().find(|row| row.start.get() == at)
                .unwrap_or_else(|| panic!("a repeat is placed at the uncovered tick {at}: {rows:?}"));
            assert_eq!(repeat.played_by, performer);
            assert_eq!(repeat.disposition, None);
        }

        // While that work is open, a tick admits nothing.
        let (statuses, blocks) = (timeline.lock().statuses().len(), commits());
        let third = run_shipped_tick(&server, &scheduler, context, performer).await;
        assert_eq!(third["KJ_OPEN_WORK"], "1");
        assert_eq!(timeline.lock().statuses().len(), statuses, "no work admitted");
        assert_eq!(commits(), blocks, "no Commit block written");
        assert!(!server.kernel.turn_in_flight(context));

        // At the uncovered phrase the transport plays the performer's last phrase.
        while timeline.lock().playhead().get() < uncovered {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        let repeat = timeline.lock().statuses().into_iter().find(|s| s.start.get() == uncovered).unwrap();
        assert!(matches!(&repeat.disposition, Some(Disposition::Fallback { reason: FallbackReason::Requested, content: Some(_), .. })),
            "{repeat:?}");
        let copies: Vec<_> = server.documents.block_snapshots(score).unwrap().into_iter()
            .filter(|block| block.tick.map(|t| t.get()) == Some(uncovered) && block.content_type == ContentType::Abc)
            .collect();
        assert_eq!(copies.len(), 1, "one phrase at the uncovered tick");
        assert_eq!(copies[0].id.principal_id, PrincipalId::beat(), "the transport played it");
        assert_eq!(copies[0].content, abc, "a copy of the performer's last phrase");
        assert!(server.documents.block_snapshots(context).unwrap().iter().all(|b| b.kind != BlockKind::Error),
            "a requested repeat writes no Error block into the player's context");
    });
}

/// `KJ_LISTEN` names a track whose last phrase the tick renders and places in
/// the context before it drives. A track with nothing committed in that phrase
/// is skipped quietly and the drive still runs; an unknown track is reported
/// as a failed script after the drive is admitted; unset changes nothing. The rendering branch needs a
/// SoundFont: set KAIJUTSU_TEST_SOUNDFONT and run with XDG_DATA_HOME pointing
/// at a scratch directory, which receives a `default.sf2` link to it.
#[test]
fn kj_listen_renders_the_last_phrase_skips_a_silent_track_and_reports_an_unknown_one() {
    run_local(async {
        use kaijutsu_kernel::{hyoushigi::{Attachment, BeatPolicy}, llm::{MockClient, Provider}};
        use kaijutsu_server::beat::BeatScheduler;
        use kaijutsu_types::TrackId;
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "listening-player").await.unwrap();
        let performer = PrincipalId::new();
        assign_turn_identity(&server, context, performer);
        kj.join_context(context, "listening-player").await.unwrap();
        let (callback, mut events) = turn_events_channel(64);
        kj.subscribe_turn_events(callback).await.unwrap();
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n"))));
        let track = TrackId::new("listen-score").unwrap();
        let mut scheduler = BeatScheduler::new(server.kernel.clone(), server.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1), beats_per_phrase: 4,
        }).unwrap();
        let timeline = server.kernel.track_timeline(&track).unwrap();
        let base = tokio::time::Instant::now();
        scheduler.play(&track, base);
        common::perform_with_rc_dir(&server, context, "musician");
        // The track to listen to lives in the server's own scheduler, where the
        // kernel reaches live track state; the driven track above is this
        // test's private scheduler.
        let heard = create_context(&kj, "heard-player").await.unwrap();
        let attached = kj.execute_kj_quiet(heard, &["transport".into(), "attach".into(), "--track".into(), "heard-band".into()]).await.unwrap();
        assert_eq!(attached.exit_code, 0, "{}", attached.stderr);
        let heard_score = server.kernel_db.lock().get_track("heard-band").unwrap().unwrap().score_context_id.unwrap();
        let mut beat = 0u64;
        let assets = |server: &kaijutsu_server::SharedKernel| server.documents.block_snapshots(context).unwrap()
            .into_iter().filter(|b| b.role == Role::Asset).collect::<Vec<_>>();
        let errors = |server: &kaijutsu_server::SharedKernel| server.documents.block_snapshots(context).unwrap()
            .into_iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect::<Vec<_>>();

        // Nothing committed on the listened track: skip quietly, still drive.
        server.kernel_db.lock().set_context_env(context, "KJ_LISTEN", "heard-band").unwrap();
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        run_shipped_tick(&server, &scheduler, context, performer).await;
        assert_eq!(timeline.lock().future_len(), 1, "the drive still runs: {:?}", errors(&server));
        assert!(assets(&server).is_empty(), "no clip when nothing was committed");
        assert!(errors(&server).is_empty(), "a silent track is not an error: {:?}", errors(&server));
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        drop(held);
        recv_turn_event(&mut events, context).await;

        // Unset: the same drive, no listening.
        server.kernel_db.lock().set_context_env(context, "KJ_LISTEN", "").unwrap();
        let held = session.lock().await;
        run_shipped_tick(&server, &scheduler, context, performer).await;
        assert!(timeline.lock().future_len() >= 1, "the drive runs (plus any repeats): {:?}", errors(&server));
        assert!(assets(&server).is_empty(), "unset KJ_LISTEN places no clip");
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        drop(held);
        recv_turn_event(&mut events, context).await;

        // An unknown track does not stop the phrase: the drive is admitted,
        // and the script then fails with the cause.
        server.kernel_db.lock().set_context_env(context, "KJ_LISTEN", "ghost").unwrap();
        let held = session.lock().await;
        run_shipped_tick(&server, &scheduler, context, performer).await;
        assert!(errors(&server).iter().any(|e| e.contains("ghost")), "{:?}", errors(&server));
        assert!(timeline.lock().future_len() >= 1, "the drive still runs: {:?}", errors(&server));
        assert!(assets(&server).is_empty());
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        drop(held);
        recv_turn_event(&mut events, context).await;

        // A committed phrase in the window is rendered and placed as audio. The
        // heard track's playhead is 0, so tick -2 lies in its last phrase.
        let (Ok(font), Ok(xdg)) = (std::env::var("KAIJUTSU_TEST_SOUNDFONT"), std::env::var("XDG_DATA_HOME")) else {
            eprintln!("skipped rendering branch: set KAIJUTSU_TEST_SOUNDFONT and XDG_DATA_HOME");
            return;
        };
        let font_dir = std::path::Path::new(&xdg).join("kaijutsu/models/soundfont");
        std::fs::create_dir_all(&font_dir).unwrap();
        let link = font_dir.join("default.sf2");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&font, &link).unwrap();
        server.kernel_db.lock().set_context_env(context, "KJ_LISTEN", "heard-band").unwrap();
        let mut cell = kaijutsu_types::BlockSnapshotBuilder::new(
            kaijutsu_types::BlockId::new(heard_score, PrincipalId::new(), 1), BlockKind::Text,
        ).role(Role::Model).content("X:1\nQ:1/4=120\nK:C\nCDEF|\n").content_type(kaijutsu_types::ContentType::Abc)
            .tick(kaijutsu_types::Tick::new(-2)).track(TrackId::new("heard-band").unwrap()).build();
        cell.id = kaijutsu_types::BlockId::new(heard_score, PrincipalId::new(), 1);
        server.documents.insert_from_snapshot(heard_score, cell, None).unwrap();
        let held = session.lock().await;
        run_shipped_tick(&server, &scheduler, context, performer).await;
        let clips = assets(&server);
        assert_eq!(clips.len(), 1, "one clip placed: {:?}", errors(&server));
        let hash: kaijutsu_cas::ContentHash = clips[0].content.parse().expect("the block holds the clip hash");
        let reference = kaijutsu_cas::ContentStore::inspect(server.kernel.cas().as_ref(), &hash).unwrap().expect("clip in CAS");
        assert_eq!(reference.mime_type, "audio/wav");
        drop(held);
    });
}

/// `KJ_UNCOVERED=silence` leaves the phrases before a slow performer's target
/// silent: no repeat is placed. A value other than `repeat` or `silence` fails
/// the tick script loudly, naming both, even while work is open.
#[test]
fn kj_uncovered_silence_places_no_repeat_and_a_bad_value_fails_loudly() {
    run_local(async {
        use kaijutsu_kernel::{hyoushigi::{Attachment, BeatPolicy}, llm::{MockClient, Provider}};
        use kaijutsu_server::beat::BeatScheduler;
        use kaijutsu_types::TrackId;
        let (addr, server) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let context = create_context(&kj, "quiet-player").await.unwrap();
        let performer = PrincipalId::new();
        assign_turn_identity(&server, context, performer);
        kj.join_context(context, "quiet-player").await.unwrap();
        let (callback, mut events) = turn_events_channel(64);
        kj.subscribe_turn_events(callback).await.unwrap();
        server.kernel.llm().write().await.register("mock", std::sync::Arc::new(Provider::Mock(MockClient::new("X:1\nK:C\nCDEF|\n"))));
        let track = TrackId::new("quiet-score").unwrap();
        let mut scheduler = BeatScheduler::new(server.kernel.clone(), server.documents.clone());
        let mut attachment = Attachment::musician_default();
        attachment.ooda_armed = false;
        scheduler.attach(track.clone(), context, attachment, BeatPolicy {
            period: Duration::from_secs(1), beats_per_phrase: 4,
        }).unwrap();
        let timeline = server.kernel.track_timeline(&track).unwrap();
        let base = tokio::time::Instant::now();
        scheduler.play(&track, base);
        common::perform_with_rc_dir(&server, context, "musician");
        server.kernel_db.lock().set_context_env(context, "KJ_UNCOVERED", "silence").unwrap();
        let mut beat = 0u64;

        // A missed deadline leaves a measured lead of more than one phrase.
        let session = server.kernel.turns().conversations().get_or_create(context);
        let held = session.lock().await;
        run_shipped_tick(&server, &scheduler, context, performer).await;
        while timeline.lock().future_len() > 0 {
            beat += 1;
            scheduler.fire_due(base + Duration::from_secs(beat));
        }
        drop(held);
        recv_turn_event(&mut events, context).await;

        let vars = run_shipped_tick(&server, &scheduler, context, performer).await;
        assert!(vars["KJ_LEAD_BEATS"].parse::<i64>().unwrap() >= 4, "{vars:?}");
        assert_eq!(timeline.lock().future_len(), 1, "only the model's work; the uncovered phrase stays silent");
        let uncovered = vars["KJ_TICK"].parse::<i64>().unwrap() + 4;
        assert!(timeline.lock().statuses().iter().all(|s| s.start.get() != uncovered), "no repeat at {uncovered}");
        recv_turn_event(&mut events, context).await;

        server.kernel_db.lock().set_context_env(context, "KJ_UNCOVERED", "loop").unwrap();
        let statuses = timeline.lock().statuses().len();
        run_shipped_tick(&server, &scheduler, context, performer).await;
        let errors: Vec<String> = server.documents.block_snapshots(context).unwrap().into_iter()
            .filter(|b| b.kind == BlockKind::Error).map(|b| b.content).collect();
        assert!(errors.iter().any(|e| e.contains("loop") && e.contains("repeat") && e.contains("silence")),
            "a bad KJ_UNCOVERED names the value and the two accepted ones: {errors:?}");
        assert_eq!(timeline.lock().statuses().len(), statuses, "nothing admitted");
    });
}

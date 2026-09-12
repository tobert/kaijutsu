//! Wait subcommand: park until a context's turn finishes, and report what it
//! produced since the caller last looked.
//!
//! POSIX mental model, completing the quartet: `fork` snapshots, `drive` execs
//! the child, `wait` joins it. A player that delegates work drives a child and
//! then parks here until the child's turn reaches a terminal state.
//!
//! Two signals, because neither alone is sound:
//!
//! * The **flow bus** (`turn.completed` / `turn.failed`) is the fast wake, and
//!   it is lossy and un-journaled — a completion published before the
//!   subscription lands is gone with no catch-up. So the subscription is taken
//!   **before** the first state read, never after.
//! * Every quiet window re-reads the **block log** (did the turn produce a
//!   model block) against the kernel's **turn-liveness registry**
//!   (`Kernel::turn_in_flight` — process-lifetime, not block status; see its
//!   doc) and resolves the wait when the turn both ran and is no longer in
//!   flight. This is what makes a dropped completion cost a few seconds
//!   instead of the whole timeout.
//!
//! `.data.resolved_by` says which of the two ended the wait, so a lossy bus
//! shows up as an observation rather than a mystery.
//!
//! The tail is a **window over the durable log**, not a consumed queue: `--since`
//! advances a cursor the caller owns. Two waiters therefore never steal each
//! other's records, and nothing ages out under a capacity cap — a bounded
//! in-memory ring would lose both properties.

use clap::{Parser, ValueEnum};
use kaijutsu_types::{BlockKind, BlockSnapshot, ContentType, Role};

use super::effect::{Classify, Effect};
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

/// How long the bus must stay quiet before the durable log is re-read.
///
/// Long enough that an actively streaming turn never trips it (deltas land far
/// more often than this), short enough that a dropped completion resolves in
/// seconds rather than at the timeout. Polling is unconditional on quiet, not
/// gated on a subscriber-side lag signal: a drop inside the bus leaves the
/// subscription looking clean-with-a-hole, so a lag-gated recovery never arms.
const QUIET: std::time::Duration = std::time::Duration::from_secs(3);

/// What a tail entry may carry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum TailFilter {
    /// Prose and failures — what the turn said and what broke.
    Text,
    /// Adds tool calls and their results.
    Tools,
    /// Every block, thinking and trace included.
    All,
}

impl TailFilter {
    /// Whether a block of this kind belongs in the tail.
    fn admits(self, kind: BlockKind) -> bool {
        match self {
            Self::All => true,
            // Shared with the MCP progress relay's `progress_line`, so a
            // streamed turn and its tail admit the same kinds.
            Self::Tools => kind.narrates_turn(),
            Self::Text => matches!(kind, BlockKind::Text | BlockKind::Error),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "wait",
    about = "Wait for a turn, shell operation, ask decision, or kaish job.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct WaitArgs {
    /// Wait for this shell operation to finish, including any approval wait.
    #[arg(long, conflicts_with_all = ["ask", "job", "since"])]
    operation: Option<String>,
    /// Wait for this ask's decision. Approval may start work that is still running.
    #[arg(long, conflicts_with_all = ["operation", "job", "since"])]
    ask: Option<String>,
    /// Wait for this kaish job in the target context.
    #[arg(long, conflicts_with_all = ["operation", "ask", "since"])]
    job: Option<u64>,
    /// Report only blocks after this one. Pass the `cursor` from a previous
    /// `kj wait` to page through a long turn without re-reading what you have.
    #[arg(long)]
    since: Option<String>,
    /// Stop parking after this many seconds and report status `running` with
    /// whatever has landed so far. Waiting again resumes; nothing is lost.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// Keep at most this many blocks in the tail, newest kept.
    #[arg(long, default_value_t = 20)]
    max_blocks: usize,
    /// Truncate each block's text at this many bytes.
    #[arg(long, default_value_t = 2048)]
    max_bytes: usize,
    /// What the tail carries: prose and errors, plus tool traffic, or everything.
    #[arg(long, value_enum, default_value_t = TailFilter::Text)]
    include: TailFilter,
    /// Target context (label or id); defaults to the current context.
    target: Option<String>,
}

/// Whether a block is an *input* to a turn rather than something a turn
/// produced. A turn is seeded by a user or system block and answers with model
/// and tool blocks, so this is what anchors "the current turn".
fn is_turn_input(role: Role) -> bool {
    matches!(role, Role::User | Role::System)
}

/// Index of the block the current turn hangs off — the last turn input in the
/// log. `None` when the log holds no input at all, which reads as "anchor
/// before everything".
fn derive_anchor(blocks: &[BlockSnapshot]) -> Option<usize> {
    blocks.iter().rposition(|b| is_turn_input(b.role))
}

/// The durable verdict: the turn both **ran** (a model block sits after
/// `floor`) and **settled** (no turn in flight, per `Kernel::turn_in_flight` —
/// not block status; see that method's doc for why).
///
/// The ran-guard is what makes unconditional quiet-polling safe. Between the
/// seed landing and the model's first block the context is idle but the turn
/// has not happened yet; resolving there would report a finished turn before
/// the model ever spoke.
fn turn_ran_and_settled(blocks: &[BlockSnapshot], floor: Option<usize>, in_flight: bool) -> bool {
    if in_flight {
        return false;
    }
    let start = floor.map_or(0, |i| i + 1);
    blocks
        .get(start..)
        .is_some_and(|rest| rest.iter().any(|b| b.role == Role::Model))
}

/// Truncate at a UTF-8 character boundary at or below `max` bytes.
///
/// Returns the kept prefix and whether anything was dropped. Byte-slicing a
/// `String` at an arbitrary index panics mid-codepoint; flooring to a boundary
/// is what makes a byte budget safe on prose that is not ASCII.
fn truncate_at_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max)
        .last()
        .unwrap_or(0);
    (&s[..end], true)
}

/// One rendered tail entry.
fn tail_entry(b: &BlockSnapshot, max_bytes: usize) -> serde_json::Value {
    let (text, truncated) = truncate_at_bytes(&b.content, max_bytes);
    let mut entry = serde_json::json!({
        "id": b.id.to_key(),
        "role": format!("{:?}", b.role).to_lowercase(),
        "kind": format!("{:?}", b.kind).to_lowercase(),
        "status": format!("{:?}", b.status).to_lowercase(),
        "content": text,
        "truncated": truncated,
    });
    if let Some(name) = &b.tool_name {
        entry["tool_name"] = serde_json::Value::String(name.clone());
    }
    entry
}

/// The tail: blocks after `since`, filtered, capped to the newest `max_blocks`.
///
/// Returns the entries and how many admitted blocks were dropped off the front
/// to honor the cap, so a caller can tell a short turn from a trimmed one.
fn build_tail(
    blocks: &[BlockSnapshot],
    since: Option<usize>,
    filter: TailFilter,
    max_blocks: usize,
    max_bytes: usize,
) -> (Vec<serde_json::Value>, usize) {
    let start = since.map_or(0, |i| i + 1);
    let admitted: Vec<&BlockSnapshot> = blocks
        .get(start..)
        .unwrap_or_default()
        .iter()
        .filter(|b| filter.admits(b.kind))
        .collect();
    let omitted = admitted.len().saturating_sub(max_blocks);
    let kept = admitted
        .iter()
        .skip(omitted)
        .map(|b| tail_entry(b, max_bytes))
        .collect();
    (kept, omitted)
}

impl KjDispatcher {
    pub(crate) async fn dispatch_wait(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // A bare `kj wait` is valid — it waits on the current context — so an
        // empty argv is the all-default parse, never a help request.
        let parsed = match WaitArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj wait: {e}"));
            }
        };

        let target = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(parsed.target.as_deref(), caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj wait: {e}")),
            }
        };

        if parsed.operation.is_some() || parsed.ask.is_some() || parsed.job.is_some() {
            return self.wait_for_work(target, &parsed).await;
        }

        // Subscribe BEFORE the first read of the log. A turn can finish between
        // the read and the subscribe, and the bus has no catch-up — a
        // subscription taken second would park until the timeout on a turn that
        // was already over.
        let mut sub = self.kernel().turn_flows().subscribe("turn.*");

        let since_key = parsed.since.clone();
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(parsed.timeout);

        loop {
            let blocks = match self.blocks.block_snapshots(target) {
                Ok(b) => b,
                Err(e) => return KjResult::Err(format!("kj wait: {e}")),
            };

            // `--since` floors both the tail and the ran-guard. Flooring the
            // ran-guard is what makes `kj drive` without a seed waitable: with
            // no fresh input block the anchor stays on the previous turn's
            // seed, and the model blocks already after it would otherwise read
            // as "this turn ran".
            let since_idx = match &since_key {
                Some(key) => match self.resolve_block_id(key, parsed.target.as_deref(), caller) {
                    Ok(id) => match blocks.iter().position(|b| b.id == id) {
                        Some(i) => Some(i),
                        None => {
                            return KjResult::Err(format!(
                                "kj wait: --since block '{key}' is not in this context's log"
                            ));
                        }
                    },
                    Err(e) => return KjResult::Err(format!("kj wait: {e}")),
                },
                None => None,
            };
            let floor = match (derive_anchor(&blocks), since_idx) {
                (Some(a), Some(s)) => Some(a.max(s)),
                (a, s) => a.or(s),
            };

            // Liveness comes from the kernel's turn registry, not block
            // status: a block left `Running` by a killed or timed-out tool
            // call would otherwise poison every future wait on this context
            // (block status never returns to idle on its own), and the gap
            // between a tool result landing and the next model block is a
            // live LLM round trip during which nothing is `Running` even
            // though the turn is very much still going.
            let in_flight = self.kernel().turn_in_flight(target);
            if turn_ran_and_settled(&blocks, floor, in_flight) {
                return self.wait_report(
                    target, &blocks, since_idx, &parsed, "completed", None, None, "log", started,
                );
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return self.wait_report(
                    target, &blocks, since_idx, &parsed, "running", None, None, "timeout", started,
                );
            }

            // Park on the bus, waking at the quiet window so the durable log is
            // re-read even when nothing is published.
            let park = tokio::time::timeout(remaining.min(QUIET), sub.recv()).await;
            let Ok(Some(msg)) = park else {
                // Quiet window elapsed, or the bus closed. Either way the next
                // loop re-reads the log, which is the authority.
                continue;
            };
            match msg.payload {
                crate::flows::TurnFlow::Completed {
                    context_id,
                    output_block_id,
                    reason,
                    ..
                } if context_id == target => {
                    let blocks = self.blocks.block_snapshots(target).unwrap_or(blocks);
                    return self.wait_report(
                        target,
                        &blocks,
                        since_idx,
                        &parsed,
                        "completed",
                        Some(format!("{reason:?}")),
                        output_block_id.map(|b| b.to_key()),
                        "event",
                        started,
                    );
                }
                crate::flows::TurnFlow::Failed {
                    context_id, error, ..
                } if context_id == target => {
                    let blocks = self.blocks.block_snapshots(target).unwrap_or(blocks);
                    return self.wait_report(
                        target,
                        &blocks,
                        since_idx,
                        &parsed,
                        "failed",
                        Some(error),
                        None,
                        "event",
                        started,
                    );
                }
                _ => continue,
            }
        }
    }

    /// Render the outcome: a compact human line plus the structured payload a
    /// delegating player reads.
    async fn wait_for_work(&self, context_id: kaijutsu_types::ContextId, args: &WaitArgs) -> KjResult {
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(args.timeout);
        let mut ledger = self.kernel().ledger_flows().subscribe("ledger.changed");
        loop {
            let (kind, id, settled, state) = if let Some(id) = &args.ask {
                let row = match self.kernel_db().lock().get_approval(id) {
                    Ok(Some(row)) if row.context_id == context_id.as_bytes() => row,
                    Ok(_) => return KjResult::Err(format!("kj wait: ask {id} not found in context {context_id}")),
                    Err(e) => return KjResult::Err(format!("kj wait: {e}")),
                };
                ("ask", id.clone(), row.status.is_terminal(), serde_json::json!(row))
            } else if let Some(id) = &args.operation {
                let state = match self.kernel().shell_operations().get(id, context_id) {
                    Ok(Some(state)) => state,
                    Ok(None) => return KjResult::Err(format!("kj wait: operation {id} not found in context {context_id}")),
                    Err(e) => return KjResult::Err(format!("kj wait: {e}")),
                };
                ("operation", id.clone(), state.completed_at.is_some(), serde_json::json!(state))
            } else {
                let id = kaish_kernel::scheduler::JobId(args.job.expect("one work selector"));
                let manager = self.kernel().context_job_manager(context_id);
                let Some(job) = manager.get(id).await else {
                    return KjResult::Err(format!("kj wait: job {id} not found in context {context_id}"));
                };
                let settled = matches!(job.status,
                    kaish_kernel::scheduler::JobStatus::Done
                    | kaish_kernel::scheduler::JobStatus::Failed
                    | kaish_kernel::scheduler::JobStatus::Killed);
                ("job", id.to_string(), settled, serde_json::json!(job))
            };
            let timed_out = !settled && tokio::time::Instant::now() >= deadline;
            if settled || timed_out {
                let status = if settled { "done" } else { "running" };
                return KjResult::ok_with_data(
                    format!("{kind} {id}: {status}"),
                    serde_json::json!({
                        "kind": kind, "id": id, "context_id": context_id,
                        "status": status, "timed_out": timed_out, "state": state,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                    }),
                );
            }
            // State is authoritative. Quiet polling recovers missed events and
            // observes job completion without consuming its result or handle.
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = tokio::time::sleep(QUIET) => {}
                event = ledger.recv_event(), if args.ask.is_some() => {
                    if event.is_none() {
                        return KjResult::Err("kj wait: ledger notification stream closed".into());
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn wait_report(
        &self,
        target: kaijutsu_types::ContextId,
        blocks: &[BlockSnapshot],
        since_idx: Option<usize>,
        parsed: &WaitArgs,
        status: &str,
        detail: Option<String>,
        output_block_id: Option<String>,
        resolved_by: &str,
        started: std::time::Instant,
    ) -> KjResult {
        let (tail, omitted) = build_tail(
            blocks,
            since_idx,
            parsed.include,
            parsed.max_blocks,
            parsed.max_bytes,
        );
        // The cursor is the last block in the whole log, not the last one the
        // filter admitted: a caller paging with `--since` must not re-read the
        // blocks its filter skipped.
        let cursor = blocks.last().map(|b| b.id.to_key());
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let mut lines = vec![match &detail {
            Some(d) => format!(
                "context {} — {status} ({d}) in {:.1}s",
                target.short(),
                elapsed_ms as f64 / 1000.0
            ),
            None => format!(
                "context {} — {status} in {:.1}s",
                target.short(),
                elapsed_ms as f64 / 1000.0
            ),
        }];
        for entry in &tail {
            let kind = entry["kind"].as_str().unwrap_or("?");
            let text = entry["content"].as_str().unwrap_or("");
            let first = text.lines().next().unwrap_or("");
            let (clipped, _) = truncate_at_bytes(first, 160);
            lines.push(format!("  [{kind}] {clipped}"));
        }
        if omitted > 0 {
            lines.push(format!("  ({omitted} earlier blocks omitted)"));
        }
        if let Some(c) = &cursor {
            lines.push(format!("cursor: {c}"));
        }

        KjResult::Ok {
            message: lines.join("\n"),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": target.to_hex(),
                "status": status,
                "detail": detail,
                "output_block_id": output_block_id,
                "resolved_by": resolved_by,
                "cursor": cursor,
                "blocks": tail,
                "omitted": omitted,
                "elapsed_ms": elapsed_ms,
            })),
        }
    }
}

// Verb class: kj/effect.rs
impl Classify for WaitArgs {
    fn effect(&self) -> Effect {
        // Parks on the flow bus and re-reads the durable log; writes nothing.
        Effect::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_accepts_one_explicit_work_handle() {
        for selector in ["--operation", "--ask", "--job"] {
            assert!(WaitArgs::try_parse_from([selector, "1", "--timeout", "0"]).is_ok());
        }
        assert!(WaitArgs::try_parse_from(["--ask", "1", "--operation", "2"]).is_err());
        assert!(WaitArgs::try_parse_from(["--job", "1", "--since", "block"]).is_err());
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// The structured payload, or a panic naming what came back instead.
    fn data_of(r: &KjResult) -> serde_json::Value {
        match r {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("expected structured data, got: {}", other.message()),
        }
    }
    use kaijutsu_types::{BlockId, ContextId, PrincipalId, Status};

    /// A snapshot with just the fields the wait predicates read.
    fn snap(role: Role, kind: BlockKind, status: Status, content: &str) -> BlockSnapshot {
        let mut b = kaijutsu_types::BlockSnapshotBuilder::new(
            BlockId::new(ContextId::new(), PrincipalId::new(), 0),
            kind,
        )
        .build();
        b.role = role;
        b.status = status;
        b.content = content.to_string();
        b
    }

    fn user(content: &str) -> BlockSnapshot {
        snap(Role::User, BlockKind::Text, Status::Done, content)
    }
    fn model(content: &str) -> BlockSnapshot {
        snap(Role::Model, BlockKind::Text, Status::Done, content)
    }

    /// The anchor must skip the model AND tool blocks a turn produces — a tool
    /// result is not a new seed, and anchoring on one would let the very next
    /// model block resolve the wait mid-turn.
    #[test]
    fn anchor_is_the_last_turn_input_not_the_last_tool_block() {
        let blocks = vec![
            user("first"),
            model("answer"),
            user("second"),
            snap(Role::Tool, BlockKind::ToolResult, Status::Done, "out"),
        ];
        assert_eq!(derive_anchor(&blocks), Some(2));
    }

    #[test]
    fn a_seed_with_no_model_answer_has_not_run() {
        let blocks = vec![user("do the thing")];
        assert!(!turn_ran_and_settled(&blocks, derive_anchor(&blocks), false));
    }

    #[test]
    fn not_in_flight_with_a_model_block_after_the_floor_is_completed() {
        let blocks = vec![user("do the thing"), model("done")];
        assert!(turn_ran_and_settled(&blocks, derive_anchor(&blocks), false));
    }

    /// The ran-guard's whole job: a settled context whose model block predates
    /// the seed must not read as a finished turn.
    #[test]
    fn a_previous_turns_answer_does_not_satisfy_a_fresh_seed() {
        let blocks = vec![user("first"), model("answer"), user("second")];
        assert!(!turn_ran_and_settled(&blocks, derive_anchor(&blocks), false));
    }

    /// `--since` floors the ran-guard so a seedless `kj drive` is waitable:
    /// without the floor the previous turn's model block resolves it instantly.
    #[test]
    fn since_floors_the_ran_guard_when_no_fresh_seed_exists() {
        let blocks = vec![user("first"), model("answer")];
        assert!(
            turn_ran_and_settled(&blocks, derive_anchor(&blocks), false),
            "anchor alone sees the old answer"
        );
        assert!(
            !turn_ran_and_settled(&blocks, Some(1), false),
            "a cursor past the old answer must not resolve"
        );
    }

    /// The regression test for the actual live bug: a Model block already
    /// sits after the anchor and nothing in the log is `Running` — the old
    /// block-status inference (`turn_is_idle`, now deleted) read exactly this
    /// shape as a finished turn. It is the ordinary gap between a tool result
    /// landing and the model's next block appearing (an LLM round trip), and
    /// the turn is still in flight, so it must not settle.
    #[test]
    fn a_turn_in_flight_is_not_settled_even_with_nothing_running() {
        let blocks = vec![
            user("go"),
            model("Still compiling. Polling again:"),
            snap(Role::Tool, BlockKind::ToolCall, Status::Done, "call"),
            snap(Role::Tool, BlockKind::ToolResult, Status::Done, "result"),
        ];
        assert!(!turn_ran_and_settled(&blocks, derive_anchor(&blocks), true));
    }

    /// The orphaned-block fix, for free: a block left `Running` forever by a
    /// killed or timed-out tool call used to poison every future wait on this
    /// context (block status never returns to idle on its own). Liveness now
    /// comes from the turn registry, so once the turn is not in flight the
    /// stale `Running` block no longer blocks the verdict.
    #[test]
    fn an_orphaned_running_block_does_not_block_the_verdict_once_not_in_flight() {
        let blocks = vec![
            user("go"),
            model("partial"),
            snap(Role::Tool, BlockKind::ToolCall, Status::Running, ""),
        ];
        assert!(turn_ran_and_settled(&blocks, derive_anchor(&blocks), false));
    }

    #[test]
    fn the_text_filter_drops_tool_traffic_and_all_keeps_it() {
        let blocks = vec![
            user("go"),
            snap(Role::Tool, BlockKind::ToolCall, Status::Done, "call"),
            model("answer"),
        ];
        let (tail, _) = build_tail(&blocks, None, TailFilter::Text, 20, 2048);
        assert_eq!(tail.len(), 2, "the tool call is filtered out");
        let (tail, _) = build_tail(&blocks, None, TailFilter::All, 20, 2048);
        assert_eq!(tail.len(), 3);
    }

    /// The cap keeps the NEWEST blocks. Keeping the first N would hand a caller
    /// the opening of a turn and hide the conclusion it asked for.
    #[test]
    fn the_cap_keeps_the_newest_blocks_and_counts_what_it_dropped() {
        let blocks = vec![model("one"), model("two"), model("three")];
        let (tail, omitted) = build_tail(&blocks, None, TailFilter::Text, 2, 2048);
        assert_eq!(omitted, 1);
        assert_eq!(tail[0]["content"], "two");
        assert_eq!(tail[1]["content"], "three");
    }

    #[test]
    fn since_excludes_everything_at_or_before_the_cursor() {
        let blocks = vec![model("one"), model("two"), model("three")];
        let (tail, _) = build_tail(&blocks, Some(0), TailFilter::Text, 20, 2048);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0]["content"], "two");
    }

    /// A byte budget must never split a codepoint — the naive `&s[..max]` slice
    /// panics here.
    #[test]
    fn truncation_floors_to_a_character_boundary() {
        let s = "日本語のテキスト";
        let (kept, truncated) = truncate_at_bytes(s, 7);
        assert!(truncated);
        assert!(s.starts_with(kept));
        assert!(kept.len() <= 7);
        assert_eq!(kept, "日本");
    }

    // ---- dispatcher-level tests: the park loop itself ----

    use crate::kj::test_helpers::*;

    /// Seed a context with a document and the given blocks, in order.
    fn seed(
        d: &KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        principal: PrincipalId,
        blocks: &[(Role, BlockKind, Status, &str)],
    ) {
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        for (role, kind, status, content) in blocks {
            d.block_store()
                .insert_block_as(
                    ctx,
                    None,
                    None,
                    *role,
                    *kind,
                    content.to_string(),
                    *status,
                    ContentType::Plain,
                    Some(principal),
                )
                .unwrap();
        }
    }

    /// Park until `kj wait` has actually subscribed, so a test publish cannot
    /// race the subscription it is meant to be received by.
    async fn await_subscriber(d: &KjDispatcher, topic: &str) {
        for _ in 0..1000 {
            if d.kernel().turn_flows().topic_subscribers(topic) > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        panic!("kj wait never subscribed to {topic}");
    }

    #[tokio::test]
    async fn a_turn_that_already_finished_resolves_from_the_log_without_parking() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("child"), None, principal);
        seed(
            &d,
            ctx,
            principal,
            &[
                (Role::User, BlockKind::Text, Status::Done, "do the thing"),
                (Role::Model, BlockKind::Text, Status::Done, "did it"),
            ],
        );
        let c = caller_with_context(ctx);

        let r = d.dispatch(&[s("wait")], &c).await;
        assert!(r.is_ok(), "wait failed: {}", r.message());
        let data = data_of(&r);
        assert_eq!(data["status"], "completed");
        assert_eq!(
            data["resolved_by"], "log",
            "no event was published, so the durable log must be what resolved it"
        );
        assert_eq!(data["blocks"].as_array().unwrap().len(), 2);
    }

    /// The bus is the fast path and must resolve a wait the log alone never
    /// would — here the turn is marked in flight, exactly as
    /// `publish_turn_request`/`spawn_llm_for_prompt` mark a real one, so the
    /// log-poll leg can never resolve it on its own.
    #[tokio::test]
    async fn a_completion_event_resolves_a_wait_the_log_would_never_settle() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("child"), None, principal);
        seed(
            &d,
            ctx,
            principal,
            &[
                (Role::User, BlockKind::Text, Status::Done, "go"),
                (Role::Model, BlockKind::Text, Status::Running, "working"),
            ],
        );
        d.kernel().mark_turn_begun(ctx);
        let c = caller_with_context(ctx);

        let d2 = std::sync::Arc::clone(&d);
        let waiter = tokio::spawn(async move {
            d2.dispatch(&[s("wait"), s("--timeout"), s("30")], &c).await
        });

        await_subscriber(&d, "turn.completed").await;
        d.kernel()
            .turn_flows()
            .publish(crate::flows::TurnFlow::Completed {
                context_id: ctx,
                principal_id: principal,
                output_block_id: None,
                reason: crate::flows::TurnStopReason::EndTurn,
                origin: Default::default(),
            });
        d.kernel().mark_turn_ended(ctx);

        let r = waiter.await.unwrap();
        let data = data_of(&r);
        assert_eq!(data["status"], "completed");
        assert_eq!(data["resolved_by"], "event");
        assert_eq!(data["detail"], "EndTurn");
    }

    #[tokio::test]
    async fn a_failed_turn_is_reported_as_failed_with_its_error() {
        let d = std::sync::Arc::new(test_dispatcher().await);
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("child"), None, principal);
        seed(
            &d,
            ctx,
            principal,
            &[
                (Role::User, BlockKind::Text, Status::Done, "go"),
                (Role::Model, BlockKind::Text, Status::Running, ""),
            ],
        );
        d.kernel().mark_turn_begun(ctx);
        let c = caller_with_context(ctx);

        let d2 = std::sync::Arc::clone(&d);
        let waiter = tokio::spawn(async move {
            d2.dispatch(&[s("wait"), s("--timeout"), s("30")], &c).await
        });

        await_subscriber(&d, "turn.failed").await;
        d.kernel()
            .turn_flows()
            .publish(crate::flows::TurnFlow::Failed {
                context_id: ctx,
                principal_id: principal,
                error: "provider stream broke".to_string(),
                origin: Default::default(),
            });
        d.kernel().mark_turn_ended(ctx);

        let r = waiter.await.unwrap();
        let data = data_of(&r);
        assert_eq!(data["status"], "failed");
        assert_eq!(data["detail"], "provider stream broke");
    }

    /// A turn that never starts must return `running` at the deadline, not hang
    /// and not claim success.
    #[tokio::test]
    async fn an_unanswered_seed_times_out_to_running() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("child"), None, principal);
        seed(
            &d,
            ctx,
            principal,
            &[(Role::User, BlockKind::Text, Status::Done, "nobody answers")],
        );
        let c = caller_with_context(ctx);

        let r = d
            .dispatch(&[s("wait"), s("--timeout"), s("1")], &c)
            .await;
        let data = data_of(&r);
        assert_eq!(data["status"], "running");
        assert_eq!(data["resolved_by"], "timeout");
    }

    /// A cursor the caller invented must fail loudly rather than silently
    /// reporting the whole log as if `--since` had been honored.
    #[tokio::test]
    async fn a_since_cursor_that_is_not_in_the_log_is_an_error() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("child"), None, principal);
        seed(
            &d,
            ctx,
            principal,
            &[
                (Role::User, BlockKind::Text, Status::Done, "go"),
                (Role::Model, BlockKind::Text, Status::Done, "done"),
            ],
        );
        let c = caller_with_context(ctx);

        let stranger = kaijutsu_types::BlockId::new(ContextId::new(), PrincipalId::new(), 7);
        let r = d
            .dispatch(&[s("wait"), s("--since"), s(&stranger.to_key())], &c)
            .await;
        assert!(!r.is_ok(), "an unknown cursor must not succeed");
        assert!(
            r.message().contains("not in this context's log"),
            "error should name the real problem, got: {}",
            r.message()
        );
    }

    fn register_operation(
        d: &KjDispatcher,
        context: ContextId,
        principal: PrincipalId,
        source: &str,
    ) -> crate::shell_operations::ShellOperationReceipt {
        d.block_store()
            .create_document(context, crate::DocumentKind::Conversation, None)
            .unwrap();
        let command = d.block_store().insert_block_as(
            context, None, None, Role::Tool, BlockKind::ToolCall, source.to_string(),
            Status::Running, ContentType::Plain, Some(principal),
        ).unwrap();
        let output = d.block_store().insert_block_as(
            context, Some(&command), Some(&command), Role::Tool, BlockKind::ToolResult,
            String::new(), Status::Running, ContentType::Plain, Some(principal),
        ).unwrap();
        d.kernel().shell_operations().register(
            context, principal, principal, command, output, source, None,
        ).unwrap()
    }

    #[tokio::test]
    async fn operation_wait_timeout_does_not_cancel_the_registered_job() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("wait-timeout"), None, principal);
        let receipt = register_operation(&d, context, principal, "sleep 30");
        let manager = d.kernel().context_job_manager(context);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let job = manager.register("sleep 30".to_string(), receiver).await;
        manager.set_cancel_token(job, tokio_util::sync::CancellationToken::new()).await;
        d.kernel().shell_operations().attach_job(&receipt.operation_id, job, manager.clone()).unwrap();

        let result = d.dispatch(&[
            s("wait"), s("--operation"), receipt.operation_id.clone(), s("--timeout"), s("0"),
        ], &caller_with_context(context)).await;
        let data = data_of(&result);
        assert_eq!(data["status"], "running");
        assert_eq!(data["timed_out"], true);
        let job_state = manager.get(job).await.expect("wait must not remove or cancel the job");
        assert_eq!(job_state.status, kaish_kernel::scheduler::JobStatus::Running);

        sender.send(kaish_kernel::interpreter::ExecResult::success("settled"))
            .expect("the timeout must leave the job receiver intact");
        assert_eq!(manager.wait(job).await.expect("job still completes").text_out(), "settled");
    }

    #[tokio::test]
    async fn completed_operation_wait_is_repeatably_readable() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("wait-repeat"), None, principal);
        let receipt = register_operation(&d, context, principal, "echo complete");
        let mut envelope = kaijutsu_types::shell_envelope::ShellEnvelope::new(
            kaijutsu_types::shell_envelope::ShellStatus::Done,
        );
        envelope.stdout = "complete".into();
        envelope.exit_code = Some(0);
        d.kernel().shell_operations().complete(&receipt.operation_id, envelope).unwrap();
        let args = [s("wait"), s("--operation"), receipt.operation_id.clone(), s("--timeout"), s("0")];
        let caller = caller_with_context(context);

        let first = data_of(&d.dispatch(&args, &caller).await);
        let second = data_of(&d.dispatch(&args, &caller).await);
        assert_eq!(first["status"], "done");
        assert_eq!(second["status"], "done");
        assert_eq!(first["state"]["envelope"]["stdout"], "complete");
        assert_eq!(second["state"], first["state"], "wait reads a durable result; it must not consume it");
    }

    #[tokio::test]
    async fn operation_wait_cannot_read_another_contexts_receipt() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let owner_context = register_context(&d, Some("wait-owner"), None, principal);
        let other_context = register_context(&d, Some("wait-other"), None, principal);
        let receipt = register_operation(&d, owner_context, principal, "echo private");
        let result = d.dispatch(&[
            s("wait"), s("--operation"), receipt.operation_id, s("--timeout"), s("0"),
        ], &caller_with_context(other_context)).await;
        assert!(!result.is_ok(), "an operation receipt must remain context-scoped");
        assert!(result.message().contains("not found in context"), "wrong-context error: {}", result.message());
    }

    #[test]
    fn truncation_leaves_short_text_alone() {
        let (kept, truncated) = truncate_at_bytes("short", 2048);
        assert_eq!(kept, "short");
        assert!(!truncated);
    }
}

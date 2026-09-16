//! Approval delivery and execution of approved source in its captured context.
//!
//! The ledger owns claims and answers; shared command execution owns settlement.
//! Driver thread shutdown remains part of the runtime migration.

use std::sync::Arc;
use crate::{Kernel, KernelDb};
use kaijutsu_types::{BlockId, ContextId, PrincipalId, SessionId, Status};
use kaijutsu_types::ToolKind as TypesToolKind;
use super::embedded_kaish::EmbeddedKaish;
use super::context_shell::{ShellIdentity, ShellPolicy};
use super::command::CommandRunOptions;

/// Who a principal is, as a model should read it: the two sentinels, then
/// the character sheet, then the id's short form
/// (`KernelDb::name_for` — `docs/character.md`, "`auth.db` is a keyring").
/// `None` — a rule's auto-decision, or a row with no answerer — reads as
/// `a human`, which is all the ledger knows then.
fn answerer_name(
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
    decided_by: Option<&[u8]>,
) -> String {
    let Some(id) = decided_by.and_then(PrincipalId::try_from_slice) else {
        return "a human".to_string();
    };
    kernel_db.lock().name_for(id)
}

/// What acting on one answered ask leaves for the driver to do.
enum ExecAction {
    /// Handled to the end. The ask is redeemed and the blocks waiting on it
    /// carry the outcome, so the blocks settling IS the delivery and there
    /// is nobody left to wake.
    Settled,
    /// The action ran into a pair whose turn ended at the gate: one this
    /// driver authored for a caller with no pair, or a model turn's own
    /// linked pair (`PairOwner::Turn`). Either way nothing else will notice
    /// the filled blocks on its own, so the turn has to be told with this
    /// text.
    Tell(String),
    /// Not this branch's business: take the ordinary wake path, where the
    /// caller retries and its retry is what redeems.
    FallThrough,
    /// Nothing was done and nothing was consumed. Leave the answer
    /// outstanding and try again on the next ledger change.
    Deferred,
}

/// What the driver needs off an approval row to act on its answer, lifted
/// out of the row so nothing below has to name a ledger type.
///
/// `pair` is `None` only when the ask names no blocks. Partial or malformed
/// linkage is an error: it must never authorize a different output pair.
/// The `PairOwner` decides who a fill has to tell: `Turn` for a model
/// turn's own pair, `Session` for a connected session's.
struct ExecutableAsk {
    source: String,
    stdin: Option<String>,
    cwd: Option<String>,
    actor: PrincipalId,
    reviewer: PrincipalId,
    pair: Option<(BlockId, BlockId, crate::PairOwner)>,
    /// The denial as the model reads it on the output block's stderr. Short
    /// on purpose — `kj ledger show <id>` is where the whole ask lives.
    denial: String,
}

fn approval_pair(
    command: Option<&str>,
    output: Option<&str>,
    owner: Option<crate::PairOwner>,
) -> Result<Option<(BlockId, BlockId, crate::PairOwner)>, &'static str> {
    match (command, output, owner) {
        (None, None, None) => Ok(None),
        (Some(command), Some(output), Some(owner)) => {
            let command = BlockId::from_key(command).ok_or("invalid command block ID")?;
            let output = BlockId::from_key(output).ok_or("invalid output block ID")?;
            Ok(Some((command, output, owner)))
        }
        _ => Err("incomplete approval block linkage"),
    }
}

#[cfg(test)]
mod approval_pair_tests {
    use super::*;

    #[test]
    fn only_a_complete_pair_or_no_pair_can_be_replayed() {
        let context = ContextId::new();
        let actor = PrincipalId::new();
        let command = BlockId::new(context, actor, 1);
        let output = BlockId::new(context, PrincipalId::system(), 1);
        let owner = crate::PairOwner::Turn;
        assert_eq!(approval_pair(None, None, None).unwrap(), None);
        assert_eq!(approval_pair(Some(&command.to_key()), Some(&output.to_key()), Some(owner)).unwrap(), Some((command, output, owner)));
        assert!(approval_pair(Some(&command.to_key()), None, Some(owner)).is_err());
        assert!(approval_pair(Some(&command.to_key()), Some(&output.to_key()), None).is_err());
        assert!(approval_pair(Some("invalid"), Some(&output.to_key()), Some(owner)).is_err());
    }

    #[test]
    fn no_shell_turn_seed_says_the_approved_action_did_not_run() {
        let context = ContextId::new();
        let output = BlockId::new(context, PrincipalId::system(), 1);
        let seed = no_shell_turn_seed(
            "gate-approver",
            "write the release",
            Some(&output),
            &"shell unavailable",
        );

        assert!(seed.contains("approved the action"), "{seed}");
        assert!(seed.contains("It did NOT run: no shell could be built"), "{seed}");
        assert!(seed.contains(&output.to_key()), "{seed}");
    }

    #[test]
    fn no_shell_seed_routes_to_turns_and_unlinked_asks_but_not_sessions() {
        let context = ContextId::new();
        let command = BlockId::new(context, PrincipalId::new(), 1);
        let output = BlockId::new(context, PrincipalId::system(), 2);

        assert!(needs_no_shell_turn_seed(None));
        assert!(needs_no_shell_turn_seed(Some((
            command.clone(),
            output.clone(),
            crate::PairOwner::Turn,
        ))));
        assert!(!needs_no_shell_turn_seed(Some((
            command,
            output,
            crate::PairOwner::Session,
        ))));

        let seed = no_shell_turn_seed(
            "gate-approver",
            "write the release",
            None,
            &"shell unavailable",
        );
        assert!(seed.contains("No output block was created"), "{seed}");
    }
}

/// Resume only live contexts whose durable archive timestamp is unset.
fn context_row_is_live(row: &crate::kernel_db::ContextRow) -> bool {
    row.context_state == kaijutsu_types::ContextState::Live && !row.is_archived()
}

fn context_is_live(kernel: &Arc<Kernel>, context_id: ContextId) -> bool {
    matches!(
        kernel.kernel_db().lock().get_context(context_id),
        Ok(Some(row)) if context_row_is_live(&row)
    )
}

/// Seed the values an ask's free variables held when the human was asked,
/// so the approved text expands to what was reviewed and not to whatever
/// the context holds now. An ask that recorded nothing seeds nothing. A
/// failure here is a reason not to run: the approval was for those values.
async fn seed_ask_env(
    kaish: &EmbeddedKaish,
    request_id: &str,
    kernel: &Arc<Kernel>,
) -> Result<(), String> {
    let rows = kernel
        .kernel_db()
        .lock()
        .ask_env(request_id)
        .map_err(|e| format!("could not read the variable values this ask recorded: {e}"))?;
    kaish
        .apply_ask_env(&rows)
        .await
        .map_err(|e| format!("could not restore the variable values this ask recorded: {e:#}"))
}

/// Author the command/output pair for an ask that never had one — the MCP
/// `shell_write` path, whose ToolCall/ToolResult blocks belong to the layer
/// above it and were already settled when its turn ended.
fn author_pair_for_ask(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    principal_id: PrincipalId,
    source: &str,
) -> Result<(BlockId, BlockId), crate::block_store::BlockStoreError> {
    let documents = &kernel.blocks();
    let last_block = documents.last_block_id(context_id);
    let command_block_id = documents.insert_tool_call_as(
        context_id,
        None,
        last_block.as_ref(),
        "shell",
        serde_json::json!({"code": source}),
        Some(TypesToolKind::Shell),
        Some(principal_id),
        None,
        None,
    )?;
    let output_block_id = documents.insert_tool_result_as(
        context_id,
        &command_block_id,
        Some(&command_block_id),
        "",
        false,
        None,
        Some(TypesToolKind::Shell),
        Some(PrincipalId::system()),
        None,
    )?;
    if let Err(e) = documents.set_status(context_id, &output_block_id, Status::Running) {
        tracing::warn!("gate-resume: failed to set the new output block Running: {e}");
    }
    Ok((command_block_id, output_block_id))
}

/// Settle a pair to `Error` with `reason` on the output block's stderr —
/// the shape a refused `shellExecute` already uses, so a human reading the
/// conversation sees why nothing ran in the place the command would have
/// printed.
fn settle_pair_error(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    command_block_id: &BlockId,
    output_block_id: &BlockId,
    reason: String,
) {
    let mut outcome = crate::runtime::command_outcome::CommandOutcome::new(
        crate::runtime::command_outcome::CommandExecution::NotRun, 0);
    outcome.settlement_error = Some(reason);
    if let Err(error) = crate::runtime::command::settle_outcome(
        kernel, context_id, command_block_id, output_block_id, &outcome,
    ) {
        tracing::error!("could not settle refused shell operation: {error}");
    }
    // The pair may already be cached from an earlier turn as `Waiting`;
    // this settles it in place, so the next turn must hydrate cold to see
    // it (see `crate::runtime::turn_state::ConversationCache::evict`).
    kernel.turns().conversations().evict(context_id);
}

/// Tell a model whose own tool pair was settled without execution. A turn's
/// cached mailbox cannot observe the in-place error edit, so this is the
/// durable conversation-side receipt that prevents it from retrying blindly.
fn unrun_turn_seed(
    who: &str,
    status: crate::ApprovalStatus,
    description: &str,
    output_block_id: &BlockId,
) -> String {
    let decision = match status {
        crate::ApprovalStatus::Abandoned => "cancelled",
        _ => "denied",
    };
    format!(
        "{who} {decision} the action you were waiting on: {description}\n\n\
         It did NOT run. Block {} carries the same message. Do not retry the \
         {decision} action.",
        output_block_id.to_key()
    )
}

/// Tell a model that its approved tool action could not acquire a shell.
/// Unlike an ordinary session pair, a turn pair was edited in place and needs
/// this new block to learn that the command did not run.
fn no_shell_turn_seed(
    who: &str,
    description: &str,
    output_block_id: Option<&BlockId>,
    error: &dyn std::fmt::Display,
) -> String {
    let receipt = match output_block_id {
        Some(output_block_id) => format!("Block {} carries the same message.", output_block_id.to_key()),
        None => "No output block was created because the shell was unavailable.".to_string(),
    };
    format!(
        "{who} approved the action you were waiting on: {description}\n\n\
         It did NOT run: no shell could be built ({error}). {receipt} Ask \
         again after fixing the context."
    )
}

/// A newly authored pair belongs to a model turn, so a shell failure before
/// authoring it needs a seed too. Only a connected session's existing pair
/// observes its own settled error without one.
fn needs_no_shell_turn_seed(linked: Option<(BlockId, BlockId, crate::PairOwner)>) -> bool {
    !matches!(linked, Some((_, _, crate::PairOwner::Session)))
}

/// Run an answered ask that carries executable source, or settle the blocks
/// waiting on it when it was refused.
///
/// **Redeem before execute, always.** The redemption row is the
/// exactly-once claim (`approval_redemptions.request_id` is a primary key),
/// so claiming it first means a crash between the claim and the run loses
/// the action. That is the chosen side of the trade: an approved
/// destructive action that runs twice is much the worse outcome, and
/// nothing here is durable enough to resume — a pending ask is abandoned at
/// boot. See `docs/gate-shape-b.md`.
async fn act_on_executable_answer(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    principal_id: PrincipalId,
    answer: &crate::UndeliveredAnswer,
    ask: &ExecutableAsk,
    who: &str,
) -> ExecAction {
    let source = ask.source.as_str();
    let linked = ask.pair;

    if linked.is_some_and(|(_, _, owner)| owner == crate::PairOwner::Turn) {
        let assignment = {
            let db = kernel.kernel_db().lock();
            db.get_context(context_id).map(|row| row.and_then(|row| row.played_by))
        };
        if !matches!(assignment, Ok(Some(actor)) if actor == ask.actor) {
            let reason = "The context's performer changed after this ask was raised; nothing was run.".to_string();
            if let Some((command, output, _)) = linked {
                settle_pair_error(kernel, context_id, &command, &output, reason.clone());
            }
            if let Err(e) = kernel.kernel_db().lock().redeem_ask(&answer.request_id) {
                tracing::error!("gate-resume: could not consume stale ask {}: {e}", answer.request_id);
                return ExecAction::Deferred;
            }
            tracing::error!("gate-resume: ask {}: {reason}", answer.request_id);
            return ExecAction::Settled;
        }
    }

    // A denial runs nothing. A connected session sees its settled pair
    // directly. A model turn does not: its cached mailbox cannot observe an
    // in-place edit, so it also receives an explicit no-run seed.
    if !matches!(answer.status, crate::ApprovalStatus::Allowed) {
        let Some((command_block_id, output_block_id, owner)) = linked else {
            return ExecAction::FallThrough;
        };
        settle_pair_error(
            kernel,
            context_id,
            &command_block_id,
            &output_block_id,
            ask.denial.clone(),
        );
        // Redeem after the blocks carry the reason: the answer is not
        // delivered until there is something to read.
        if let Err(e) = kernel.kernel_db().lock().redeem_ask(&answer.request_id) {
            tracing::error!(
                "gate-resume: ask {} was refused and its blocks settled, but the \
                 redemption did not write ({e}); it may be delivered again",
                answer.request_id
            );
            return ExecAction::Deferred;
        }
        return if owner == crate::PairOwner::Turn {
            ExecAction::Tell(unrun_turn_seed(
                who,
                answer.status,
                &answer.description,
                &output_block_id,
            ))
        } else {
            ExecAction::Settled
        };
    }

    match kernel.kernel_db().lock().redeem_ask(&answer.request_id) {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                "gate-resume: ask {} was already redeemed by someone else; not running it",
                answer.request_id
            );
            return ExecAction::Settled;
        }
        Err(e) => {
            // Fail closed. Running without the claim is the one outcome
            // this whole ordering exists to prevent.
            tracing::error!(
                "gate-resume: could not claim ask {} ({e}); not running it",
                answer.request_id
            );
            return ExecAction::Deferred;
        }
    }

    // The second archived check (`docs/gate-shape-b.md`, "Archived contexts
    // are inert"). The driver checked before reading the row; a context can
    // be archived in the gap, and an archived context runs nothing.
    //
    // Reaching this after the claim spends the answer without running
    // anything. That is the narrow window the first check exists to keep
    // narrow, and it is the correct direction to fail in: the context is
    // archived, so the action can never run, and a spent answer is a
    // recorded one.
    if !context_is_live(kernel, context_id) {
        tracing::info!(
            "gate-resume: {context_id} stopped being Live before ask {} could run; \
             nothing was run",
            answer.request_id
        );
        return ExecAction::Settled;
    }

    // A synthetic session: the seat that raised this ask is gone (its turn
    // ended when the gate refused, or its connection closed), and a shell
    // needs a session id to key its context binding on. Nothing durable is
    // keyed by it.
    let session_id = SessionId::new();
    let name = format!("{}-gate-{}", kernel.id(), session_id.short());
    let kaish = match async {
        let dispatcher = kernel.broker().kj_dispatcher().await.ok_or_else(||
            anyhow::anyhow!("context shell requires a registered kj dispatcher"))?;
        EmbeddedKaish::for_context(
        &dispatcher,
        &name,
        ShellIdentity {
            requester: principal_id, performer: ask.actor, reviewer: Some(ask.reviewer),
            context: context_id, session: session_id,
        },
        ShellPolicy::Agent,
        dispatcher.semantic_index(),
        dispatcher.block_source(),
        ).await
    }.await {
        Ok(kaish) => kaish,
        Err(e) => {
            tracing::error!(
                "gate-resume: could not materialize a shell for ask {} ({e}); the \
                 approval is spent and nothing ran",
                answer.request_id
            );
            if let Some((command_block_id, output_block_id, _owner)) = linked {
                settle_pair_error(
                    kernel,
                    context_id,
                    &command_block_id,
                    &output_block_id,
                    format!("approved, but no shell could be built to run it: {e}"),
                );
            }
            if needs_no_shell_turn_seed(linked) {
                return ExecAction::Tell(no_shell_turn_seed(
                    who,
                    &answer.description,
                    linked.as_ref().map(|(_, output_block_id, _)| output_block_id),
                    &e,
                ));
            }
            return ExecAction::Settled;
        }
    };

    // `tell` is true for a pair whose turn ended at the gate: one this
    // driver authors fresh below, or a model turn's own linked pair
    // (`PairOwner::Turn`). A connected session's linked pair
    // (`PairOwner::Session`) watches its own blocks, so a fill tells
    // nobody.
    let (command_block_id, output_block_id, tell) = match linked {
        Some((command_block_id, output_block_id, owner)) => {
            (command_block_id, output_block_id, matches!(owner, crate::PairOwner::Turn))
        }
        None => match author_pair_for_ask(kernel, context_id, ask.actor, source) {
            Ok((command_block_id, output_block_id)) => (command_block_id, output_block_id, true),
            Err(e) => {
                tracing::error!(
                    "gate-resume: could not author blocks for ask {} ({e}); the approval \
                     is spent and nothing ran",
                    answer.request_id
                );
                return ExecAction::Settled;
            }
        },
    };

    // The directory the human was asked about, not wherever the context has
    // reached since. A cwd that no longer resolves stops the run: the
    // approved text was read against that directory, and running it
    // somewhere else is a different action.
    if let Some(cwd) = ask.cwd.as_deref()
        && !kaish.try_set_cwd(std::path::PathBuf::from(cwd)).await
    {
        let reason = format!(
            "approved, but not run: {cwd} — the directory this was raised in — no longer \
             resolves to a directory"
        );
        settle_pair_error(
            kernel,
            context_id,
            &command_block_id,
            &output_block_id,
            reason,
        );
        return if tell {
            ExecAction::Tell(format!(
                "{who} approved the action you were waiting on: {}\n\n\
                 It did NOT run: {cwd} — the directory it was raised in — no longer \
                 resolves. Block {} carries the same message. Ask again from a \
                 directory that exists.",
                answer.description,
                output_block_id.to_key()
            ))
        } else {
            ExecAction::Settled
        };
    }

    if let Err(why) = seed_ask_env(&kaish, &answer.request_id, kernel).await {
        let reason = format!("approved, but not run: {why}");
        settle_pair_error(
            kernel,
            context_id,
            &command_block_id,
            &output_block_id,
            reason.clone(),
        );
        return if tell {
            ExecAction::Tell(format!(
                "{who} approved the action you were waiting on: {}\n\n\
                 It did NOT run: {why}. Block {} carries the same message. Ask again.",
                answer.description,
                output_block_id.to_key()
            ))
        } else {
            ExecAction::Settled
        };
    }

    tracing::info!(
        "gate-resume: running approved ask {} in {context_id}",
        answer.request_id
    );
    if let Err(error) = crate::runtime::command::run_into_blocks(
        &kaish,
        source,
        context_id,
        &command_block_id,
        &output_block_id,
        kernel,
        &crate::mcp::CallContext::new(principal_id, context_id, session_id, kernel.id())
            .with_actor(ask.actor, Some(ask.reviewer)),
        CommandRunOptions { stdin: ask.stdin.clone(), ..Default::default() },
    )
    .await {
        kernel.turns().conversations().evict(context_id);
        return ExecAction::Tell(format!("Approved command settlement failed: {error}. Inspect operation output {} before retrying.", output_block_id.to_key()));
    }

    // `run_into_blocks` just settled the pair in place. When it was
    // already `Waiting` in a cached mailbox, that edit is invisible to
    // `catch_up`; evict so the next turn hydrates cold and reads the real
    // output instead of the stale "waiting" text.
    kernel.turns().conversations().evict(context_id);

    if tell {
        // Either the output blocks reach the model as new blocks in its
        // context (driver-authored), or the fill is an in-place edit to a
        // model turn's own pair that its cached mailbox will not re-read on
        // its own (`PairOwner::Turn`); either way the seed only has to say
        // who approved and that it ran.
        ExecAction::Tell(format!(
            "{who} approved the action you were waiting on: {}\n\nIt has run.",
            answer.description
        ))
    } else {
        ExecAction::Settled
    }
}

/// Deliver answered asks and run approved source in its captured context.
///
/// Claim an executable answer before running it. Fill its existing pair, or
/// author one when absent. Model-owned pairs receive a seed describing the
/// outcome; session-owned pairs are observed directly. Non-executable answers
/// wake a model to retry only when its continuation window is still open.
pub fn spawn_gate_resume_driver(kernel: Arc<Kernel>) {
    // An approved `kj context create` or `kj fork` runs its rc lifecycle on
    // THIS thread, re-entering kaish deeply; the default 2 MiB stack
    // overflows and aborts the whole server. Same reservation as the SSH
    // session and beat-scheduler threads — see `spawn_kaish_thread`.
    if let Err(e) = crate::spawn_kaish_thread("gate-resume", move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("gate-resume: failed to build runtime: {e}");
                return;
            }
        };
        // A LocalSet, like the runtime worker: executing an approved ask
        // materializes an `EmbeddedKaish` on this thread, and kaish's own
        // execution path is not `Send`.
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let kernel = &kernel;
            let mut sub = kernel.ledger_flows().subscribe("ledger.changed");
            tracing::info!("Gate-resume driver online");

            // Answers this driver has already woken someone for. The ledger
            // row does not clear until the caller actually retries, so
            // without this every later `ledger.changed` would wake the same
            // context again for the same answer.
            //
            // In memory on purpose. A restart clears it, and re-waking once
            // after a restart is the harmless side of the trade — the wake
            // costs a turn, never a duplicated action.
            let mut woken: std::collections::HashSet<String> = std::collections::HashSet::new();

            // Ignore answers already outstanding at startup. Their callers
            // did not survive restart; a new event must not wake the backlog.
            match kernel.kernel_db().lock().undelivered_answers() {
                Ok(rows) => {
                    let n = rows.len();
                    woken.extend(rows.into_iter().map(|a| a.request_id));
                    tracing::info!("gate-resume: {n} answer(s) already outstanding at start; not waking those");
                }
                Err(e) => {
                    // Fail loud and stay closed: an empty seed set would
                    // wake the whole backlog on the next event.
                    tracing::error!(
                        "gate-resume: could not read the outstanding answers at start ({e}); \
                         driver exiting rather than risk waking the backlog"
                    );
                    return;
                }
            }

            // A single ledger change should never wake more than a handful of
            // contexts. More than this means something is wrong with the
            // predicate, and a herd of LLM turns is the expensive way to find
            // out — stop at the cap and say so.
            const WAKE_CAP_PER_EVENT: usize = 4;

            while sub.recv().await.is_some() {
                // The event carries only a generation; the ledger is the
                // authority, so re-read it rather than trusting the number.
                // `ledger.changed` is on the timing lane (lossy by design):
                // dropping events is safe here because any later one
                // re-reads everything still outstanding.
                let answers = {
                    let db = kernel.kernel_db().lock();
                    match db.undelivered_answers() {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::error!("gate-resume: could not read the ledger: {e}");
                            continue;
                        }
                    }
                };

                let mut woken_this_event = 0usize;
                for answer in answers {
                    if woken.contains(&answer.request_id) {
                        continue;
                    }
                    if woken_this_event >= WAKE_CAP_PER_EVENT {
                        tracing::warn!(
                            "gate-resume: stopped at {WAKE_CAP_PER_EVENT} wakes for one ledger \
                             change; the rest wait for the next one"
                        );
                        break;
                    }
                    let Ok(bytes) = <[u8; 16]>::try_from(answer.context_id.as_slice()) else {
                        tracing::error!(
                            "gate-resume: ask {} has a context_id that is not 16 bytes; skipping",
                            answer.request_id
                        );
                        continue;
                    };
                    let context_id = ContextId::from(uuid::Uuid::from_bytes(bytes));

                    // Preserve the requester: redemption is principal-scoped.
                    let Ok(pbytes) = <[u8; 16]>::try_from(answer.principal_id.as_slice())
                    else {
                        tracing::error!(
                            "gate-resume: ask {} has a principal_id that is not 16 bytes; skipping",
                            answer.request_id
                        );
                        continue;
                    };
                    let principal_id =
                        kaijutsu_types::PrincipalId::from(uuid::Uuid::from_bytes(pbytes));


                    // Audit rows outlive their contexts. Only live contexts
                    // may receive a seed or spend another provider request.
                    match kernel.kernel_db().lock().get_context(context_id) {
                        Ok(Some(row)) if context_row_is_live(&row) => {}
                        Ok(Some(row)) => {
                            tracing::info!(
                                "gate-resume: {context_id} is {}; leaving ask {} uncollected",
                                if row.is_archived() {
                                    "archived".to_string()
                                } else {
                                    format!("{:?}, not Live", row.context_state)
                                },
                                answer.request_id
                            );
                            woken.insert(answer.request_id.clone());
                            continue;
                        }
                        Ok(None) => {
                            tracing::info!(
                                "gate-resume: {context_id} no longer exists; leaving ask {} \
                                 uncollected",
                                answer.request_id
                            );
                            woken.insert(answer.request_id.clone());
                            continue;
                        }
                        Err(e) => {
                            // Fail closed: an unreadable context is not a
                            // reason to write into it.
                            tracing::error!(
                                "gate-resume: could not read context {context_id} ({e}); not waking"
                            );
                            continue;
                        }
                    }

                    // The whole row, because the summary does not carry
                    // `exec_source` — and whether an answer runs here or
                    // sends its caller back to try again is exactly that
                    // field.
                    let row = match kernel.kernel_db().lock().get_approval(&answer.request_id) {
                        Ok(Some(row)) => row,
                        Ok(None) => {
                            tracing::error!(
                                "gate-resume: ask {} has an answer but no row; skipping",
                                answer.request_id
                            );
                            woken.insert(answer.request_id.clone());
                            continue;
                        }
                        Err(e) => {
                            // Fail closed: an unreadable row is not a reason
                            // to guess which branch it belongs in.
                            tracing::error!(
                                "gate-resume: could not read ask {} ({e}); not acting on it",
                                answer.request_id
                            );
                            continue;
                        }
                    };

                    // Executable asks run here; other answers wake the
                    // original caller to retry and redeem. Name the answerer
                    // in the seed using its current character sheet.
                    let who = answerer_name(kernel.kernel_db(), row.decided_by.as_deref());
                    let actor = row.actor_id.as_deref().and_then(PrincipalId::try_from_slice);
                    let reviewer = row.reviewer_id.as_deref().and_then(PrincipalId::try_from_slice);
                    let (Some(actor), Some(reviewer)) = (actor, reviewer) else {
                        tracing::error!("gate-resume: ask {} has no resolved actor/reviewer; nothing was run", answer.request_id);
                        woken.insert(answer.request_id.clone());
                        continue;
                    };
                    let pair = match approval_pair(
                        row.command_block_id.as_deref(), row.output_block_id.as_deref(), row.pair_owner,
                    ) {
                        Ok(pair) => pair,
                        Err(reason) => {
                            tracing::error!("gate-resume: ask {}: {reason}; nothing was run", answer.request_id);
                            woken.insert(answer.request_id.clone());
                            continue;
                        }
                    };
                    let executable = row.exec_source.clone().map(|source| {
                        let denial = match row.decided_option.as_deref() {
                            Some("cancel") => format!("cancelled by {who} — nothing was run"),
                            Some(option) => format!("denied by {who} ({option}) — nothing was run"),
                            None => format!("denied by {who} — nothing was run"),
                        };
                        ExecutableAsk {
                            source,
                            stdin: row.exec_stdin.clone(),
                            cwd: row.cwd.clone(),
                            actor,
                            reviewer,
                            pair,
                            denial,
                        }
                    });

                    let executed_seed = match executable {
                        None => None,
                        Some(ask) => {
                            match act_on_executable_answer(
                                kernel,
                                context_id,
                                principal_id,
                                &answer,
                                &ask,
                                &who,
                            )
                            .await
                            {
                                ExecAction::Settled => {
                                    woken.insert(answer.request_id.clone());
                                    woken_this_event += 1;
                                    continue;
                                }
                                ExecAction::Deferred => continue,
                                ExecAction::Tell(text) => Some(text),
                                ExecAction::FallThrough => None,
                            }
                        }
                    };

                    let turn_in_flight = kernel.turn_in_flight(context_id);

                    // A plain wake is skipped while a turn is already
                    // running: it will make its own next attempt, and if it
                    // ends without retrying, the next ledger change picks
                    // this up again. Not marked woken, so that retry stays
                    // possible.
                    //
                    // An executed seed is never skipped this way, in flight
                    // or not: filling a pair is an in-place edit, and a
                    // running turn's cached mailbox does not re-read an
                    // already-seen block on its own `catch_up`
                    // (`llm/mailbox.rs`) — the seed is the only trace of the
                    // fill that reaches it.
                    if executed_seed.is_none() && turn_in_flight {
                        continue;
                    }

                    let seed = executed_seed.unwrap_or_else(|| {
                        let (decision, next) = match answer.status {
                            crate::ApprovalStatus::Allowed => ("approved", "Try the same call again; this approval authorizes it once."),
                            crate::ApprovalStatus::Abandoned => ("cancelled", "Do not retry the cancelled action. Continue with the rest of your work."),
                            _ => ("denied", "Do not retry the denied action. Continue with the rest of your work."),
                        };
                        format!("{who} {decision} the action you were waiting on: {}\n\nNothing has run yet. {next}", answer.description)
                    });

                    let tail = kernel.blocks().last_block_id(context_id);
                    let seed_block = match kernel.blocks().insert_block_as(
                        context_id,
                        None,
                        tail.as_ref(),
                        kaijutsu_types::Role::User,
                        kaijutsu_types::BlockKind::Text,
                        seed.clone(),
                        kaijutsu_types::Status::Done,
                        kaijutsu_types::ContentType::Plain,
                        None,
                    ) {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!(
                                "gate-resume: failed to write the seed block for {context_id}: {e}"
                            );
                            continue;
                        }
                    };

                    if answer.status == crate::ApprovalStatus::Abandoned {
                        if let Err(e) = kernel.kernel_db().lock().redeem_ask(&answer.request_id) {
                            tracing::error!("gate-resume: cancellation delivery could not be recorded for {}: {e}", answer.request_id);
                            continue;
                        }
                    }

                    // A turn is already in flight: the seed just written is
                    // the trace of the fill, read on that turn's next
                    // `catch_up` or on the next drive, and there is no turn
                    // to request.
                    if turn_in_flight {
                        woken.insert(answer.request_id.clone());
                        woken_this_event += 1;
                        tracing::info!(
                            "gate-resume: left a seed for {context_id}'s in-flight turn, ask {}",
                            answer.request_id
                        );
                        continue;
                    }

                    let Some(continuation_epoch) = row.continuation_epoch else {
                        woken.insert(answer.request_id.clone());
                        woken_this_event += 1;
                        tracing::info!(
                            "gate-resume: delivered {} to {context_id} without automatic continuation; the ask predates continuation epochs",
                            answer.request_id
                        );
                        continue;
                    };
                    let window = match kernel.gate_resume_window().await {
                        Ok(window) => window,
                        Err(error) => {
                            woken.insert(answer.request_id.clone());
                            woken_this_event += 1;
                            tracing::error!(
                                "gate-resume: delivered {} but could not read continuation policy: {error}",
                                answer.request_id
                            );
                            continue;
                        }
                    };
                    let window_ms = match i64::try_from(window.as_millis()) {
                        Ok(window_ms) => window_ms,
                        Err(_) => {
                            woken.insert(answer.request_id.clone());
                            woken_this_event += 1;
                            tracing::error!(
                                "gate-resume: delivered {} but continuation window is too large",
                                answer.request_id
                            );
                            continue;
                        }
                    };
                    let automatic_resume_allowed = match kernel.kernel_db().lock()
                        .automatic_resume_allowed(
                            context_id,
                            continuation_epoch,
                            kaijutsu_types::now_millis() as i64,
                            window_ms,
                        )
                    {
                        Ok(allowed) => allowed,
                        Err(error) => {
                            tracing::error!(
                                "gate-resume: delivered {} but could not check continuation epoch: {error}",
                                answer.request_id
                            );
                            false
                        }
                    };
                    if !automatic_resume_allowed {
                        woken.insert(answer.request_id.clone());
                        woken_this_event += 1;
                        tracing::info!(
                            "gate-resume: delivered {} to {context_id}; its continuation window is closed",
                            answer.request_id
                        );
                        continue;
                    }

                    if let Err(error) = kernel.request_turn(super::turn_request::TurnRequest {
                        context_id, after_block_id: seed_block, content: seed,
                        principal_id, model: None, continuation_epoch: Some(continuation_epoch),
                    }) {
                        tracing::warn!("gate-resume: {context_id} was not admitted: {error}");
                        continue;
                    }
                    woken.insert(answer.request_id.clone());
                    woken_this_event += 1;
                    tracing::info!(
                        "gate-resume: woke {context_id} for {:?} ask {}",
                        answer.status,
                        answer.request_id
                    );
                }
            }
            tracing::warn!("gate-resume: ledger bus closed, driver exiting");
        });
    }) {
        tracing::error!("Failed to spawn gate-resume thread: {e}");
    }
}


#[cfg(test)]
mod answerer_tests {
    use super::*;
    /// The seed a model reads names its answerer: the character sheet's
    /// name, the short id for a principal with no sheet, and `a human` only
    /// when the row names nobody (`KernelDb::name_for` —
    /// `docs/character.md`, "`auth.db` is a keyring").
    #[test]
    fn answerer_name_prefers_the_character_sheet_then_short_id() {
        let kdb = KernelDb::temporary().unwrap();
        let amy = PrincipalId::new();
        kdb.insert_character(&crate::kernel_db::CharacterRow {
            principal_id: amy,
            name: "amy".to_string(),
            created_at: 1000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap();
        let kdb = std::sync::Arc::new(parking_lot::Mutex::new(kdb));
        assert_eq!(super::answerer_name(&kdb, Some(amy.as_bytes())), "amy");
        let stranger = PrincipalId::new();
        assert_eq!(super::answerer_name(&kdb, Some(stranger.as_bytes())), stranger.short());
        assert_eq!(super::answerer_name(&kdb, None), "a human");
        assert_eq!(super::answerer_name(&kdb, Some(&[1, 2, 3])), "a human", "a malformed id names nobody");
    }

}

//! Approval delivery and execution of approved source in its captured context.
//!
//! The ledger owns claims and answers; shared command execution owns settlement.
//! The kernel worker owns delivery, cancellation, and joined settlement.

use std::sync::Arc;
use crate::{Kernel, KernelDb};
use kaijutsu_types::{BlockId, ContextId, PrincipalId, SessionId, Status};
use kaijutsu_types::ToolKind as TypesToolKind;
use super::embedded_kaish::EmbeddedKaish;
use super::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
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

/// Owns a spent approval until shared command capture takes over.
struct ApprovalPreparation<'a> {
    kernel: &'a Arc<Kernel>,
    context: ContextId,
    actor: PrincipalId,
    pair: Option<(BlockId, BlockId)>,
    armed: bool,
}

impl Drop for ApprovalPreparation<'_> {
    fn drop(&mut self) {
        if !self.armed || !std::thread::panicking() { return; }
        let reason = "Approved action preparation panicked; nothing was run. The approval is spent.";
        if let Some((command, output)) = self.pair {
            settle_pair_error(self.kernel, self.context, &command, &output, reason.into());
        } else {
            let tail = self.kernel.blocks().last_block_id(self.context);
            if let Err(error) = self.kernel.blocks().insert_block_as(
                self.context, None, tail.as_ref(), kaijutsu_types::Role::System,
                kaijutsu_types::BlockKind::Error, reason, Status::Error,
                kaijutsu_types::ContentType::Plain, Some(self.actor),
            ) {
                tracing::error!("could not record approved preparation panic: {error}");
            }
        }
    }
}

/// Preparation may stop immediately: its caller still owns the claimed ask
/// and reports that no source ran. Execution uses cooperative command settlement.
async fn prepare_while_running<T, E: std::fmt::Display>(
    stop: &tokio_util::sync::CancellationToken,
    prepare: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => Err("kernel runtime shut down before approved execution".into()),
        result = prepare => result.map_err(|error| error.to_string()),
    }
}

/// Run an answered ask that carries executable source, or settle the blocks
/// waiting on it when it was refused.
///
/// Claim before preparation or execution. The redemption primary key grants
/// one owner; a spent claim never authorizes replay. A crash before execution
/// can lose the action. Startup does not resume approved source across restart.
/// See `docs/gate-shape-b.md`.
async fn act_on_executable_answer(
    kernel: &Arc<Kernel>,
    context_id: ContextId,
    principal_id: PrincipalId,
    answer: &crate::UndeliveredAnswer,
    ask: &ExecutableAsk,
    who: &str,
    stop: &tokio_util::sync::CancellationToken,
) -> ExecAction {
    if stop.is_cancelled() { return ExecAction::Deferred; }
    let source = ask.source.as_str();
    let linked = ask.pair;

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

    // Validate context state and claim under one database lock. Only the
    // winner may execute or settle a stale performer's pair. Read faults leave
    // the approval untouched so delivery can retry after storage recovers.
    let (claim, performer_changed) = {
        let db = kernel.kernel_db().lock();
        match db.get_context(context_id) {
            Ok(Some(row)) if context_row_is_live(&row) => {
                let performer_changed = linked.is_some_and(|(_, _, owner)| owner == crate::PairOwner::Turn)
                    && row.played_by != Some(ask.actor);
                (db.redeem_ask(&answer.request_id), performer_changed)
            }
            Ok(_) => {
                tracing::info!("gate-resume: {context_id} is no longer live; leaving ask {} unclaimed", answer.request_id);
                return ExecAction::Deferred;
            }
            Err(error) => {
                tracing::error!("gate-resume: could not read {context_id} before claiming ask {}: {error}", answer.request_id);
                return ExecAction::Deferred;
            }
        }
    };
    match claim {
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

    if performer_changed {
        let reason = "The context's performer changed after this ask was raised; nothing was run.".to_string();
        if let Some((command, output, _)) = linked {
            settle_pair_error(kernel, context_id, &command, &output, reason.clone());
        }
        tracing::error!("gate-resume: ask {}: {reason}", answer.request_id);
        return ExecAction::Settled;
    }

    let mut preparation = ApprovalPreparation {
        kernel, context: context_id, actor: ask.actor,
        pair: linked.map(|(command, output, _)| (command, output)), armed: true,
    };

    // A synthetic session: the seat that raised this ask is gone (its turn
    // ended when the gate refused, or its connection closed), and a shell
    // needs a session id to key its context binding on. Nothing durable is
    // keyed by it.
    let session_id = SessionId::new();
    let name = format!("{}-gate-{}", kernel.id(), session_id.short());
    let kaish = match prepare_while_running(stop, async {
        let dispatcher = kernel.broker().kj_dispatcher().await.ok_or_else(||
            anyhow::anyhow!("context shell requires a registered kj dispatcher"))?;
        EmbeddedKaish::for_context(
        &dispatcher,
        &name,
        ShellIdentity {
            requester: principal_id, performer: ask.actor, reviewer: Some(ask.reviewer),
            context: context_id, session: session_id,
        },
        ShellPolicy::Agent, ShellCwd::Captured(ask.cwd.as_ref().map(std::path::PathBuf::from)),
        dispatcher.semantic_index(),
        dispatcher.block_source(),
        ).await
    }).await {
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
    preparation.pair = Some((command_block_id, output_block_id));

    // Restore captured inputs before execution. Cancellation after redemption
    // spends the approval but settles its pair without running the source.
    if let Err(why) = prepare_while_running(stop, seed_ask_env(&kaish, &answer.request_id, kernel)).await {
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
    // Shared capture owns unwinding after source execution can begin. Never
    // replace its captured output with a preparation-only no-run result.
    preparation.armed = false;
    if let Err(error) = crate::runtime::command::run_into_blocks(
        &kaish,
        source,
        context_id,
        &command_block_id,
        &output_block_id,
        kernel,
        &crate::mcp::CallContext::new(principal_id, context_id, session_id, kernel.id())
            .with_actor(ask.actor, Some(ask.reviewer)),
        CommandRunOptions { stdin: ask.stdin.clone(), cancel: Some(stop.clone()), ..Default::default() },
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

/// Subscribe and snapshot old answers before accepting new delivery. Startup
/// failure reaches the host; the worker owns cancellation and joined settlement.
pub(crate) fn start(kernel: &Arc<Kernel>) -> Result<(), String> {
    let sub = kernel.ledger_flows().subscribe("ledger.changed");
    // Answers already outstanding belong to callers from the previous lifetime.
    // Never wake that backlog because an unrelated new answer arrives.
    let woken = kernel.kernel_db().lock().undelivered_answers()
        .map_err(|error| format!("could not read approval backlog: {error}"))?
        .into_iter().map(|answer| answer.request_id).collect();
    let owner = Arc::downgrade(kernel);
    kernel.spawn_runtime_task(move |stop| run_delivery(owner, sub, woken, stop))
}

async fn run_delivery(
    owner: std::sync::Weak<Kernel>,
    mut sub: crate::flows::Subscription<crate::flows::LedgerFlow>,
    mut woken: std::collections::HashSet<String>,
    stop: tokio_util::sync::CancellationToken,
) {
    // Bound provider spending from one event. Unhandled answers remain in the
    // ledger for a later event; each scan reads authoritative state again.
    const WAKE_CAP_PER_EVENT: usize = 4;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            event = sub.recv() => if event.is_none() { return; },
        }
        let Some(owner) = owner.upgrade() else { return; };
        let kernel = &owner;
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
            if stop.is_cancelled() { return; }
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
                        &stop,
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

            if stop.is_cancelled() && executed_seed.is_none() { return; }
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

            // The seed is part of settling work already claimed. Shutdown
            // retains that fact, but never spends another model request on it.
            if stop.is_cancelled() { return; }

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
            let window = match prepare_while_running(&stop, kernel.gate_resume_window()).await {
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

            // The durable seed delivered this answer. Turn admission cannot
            // undo that fact or authorize another copy on a later ledger event.
            woken.insert(answer.request_id.clone());
            woken_this_event += 1;
            if let Err(error) = kernel.request_turn(super::turn_request::TurnRequest {
                context_id, after_block_id: seed_block, content: seed,
                principal_id, model: None, continuation_epoch: Some(continuation_epoch),
            }) {
                tracing::warn!("gate-resume: {context_id} was not admitted: {error}");
                continue;
            }
            tracing::info!(
                "gate-resume: woke {context_id} for {:?} ask {}",
                answer.status,
                answer.request_id
            );
        }
    }
}


#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[tokio::test]
    async fn unreadable_context_preserves_approval_and_pair_for_retry() {
        use crate::kj::test_helpers::{test_dispatcher, register_context};
        use approval_ledger::{ask::create_ask, decide::{decide, Answerer, DecideInput}, types::{NewAsk, Origin}};
        for owner in [Some(crate::PairOwner::Turn), Some(crate::PairOwner::Session), None] {
            let dispatcher = Arc::new(test_dispatcher().await);
            dispatcher.set_self_arc();
            let kernel = dispatcher.kernel().clone();
            kernel.broker().set_kj_dispatcher(&dispatcher).await;
            let actor = PrincipalId::new();
            let reviewer = PrincipalId::new();
            let context = register_context(&dispatcher, Some("approval-fault"), None, actor);
            kernel.kernel_db().lock().update_context_review(context, Some(actor), Some(reviewer)).unwrap();
            kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
            let pair = owner.map(|owner| {
                let (command, output) = author_pair_for_ask(&kernel, context, actor, "echo approved-once").unwrap();
                (command, output, owner)
            });
            let answer = {
                let db = kernel.kernel_db().lock();
                let request_id = create_ask(db.conn_for_ledger(), &NewAsk {
                    context_id: context.as_bytes().to_vec(), actor_id: actor.as_bytes().to_vec(),
                    reviewer_id: reviewer.as_bytes().to_vec(), principal_id: actor.as_bytes().to_vec(),
                    origin: Origin::ShellGate, instance: None, tool: None, hook_id: None,
                    description: "run captured source".into(), statements: vec![], authorized_label: None,
                    rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
                    exec_source: Some("echo approved-once".into()), exec_stdin: None,
                    continuation_epoch: None, env: vec![],
                }).unwrap();
                decide(db.conn_for_ledger(), &request_id, DecideInput {
                    allow: true, decided_by: Some(Answerer { principal: reviewer.as_bytes(), context: None }),
                    ..Default::default()
                }).unwrap();
                db.undelivered_answers().unwrap().into_iter().find(|a| a.request_id == request_id).unwrap()
            };
            let ask = ExecutableAsk { source: "echo approved-once".into(), stdin: None, cwd: None,
                actor, reviewer, pair, denial: "not denied".into() };
            let before = kernel.blocks().block_snapshots(context).unwrap();
            kernel.kernel_db().lock().conn_for_ledger().execute_batch(
                "ALTER TABLE contexts RENAME TO unavailable_contexts"
            ).unwrap();
            let stop = tokio_util::sync::CancellationToken::new();
            let action = act_on_executable_answer(&kernel, context, actor, &answer, &ask, "reviewer", &stop).await;
            assert!(matches!(action, ExecAction::Deferred), "{owner:?}: an unreadable context is not reassignment or archive");
            let after = kernel.blocks().block_snapshots(context).unwrap();
            assert_eq!(after.len(), before.len());
            for (before, after) in before.iter().zip(after.iter()) {
                assert_eq!(after.status, before.status);
                assert_eq!(after.stderr, before.stderr);
                assert_eq!(after.content, before.content);
            }
            assert!(kernel.kernel_db().lock().undelivered_answers().unwrap().iter()
                .any(|a| a.request_id == answer.request_id), "read failure must not consume approval");
            kernel.kernel_db().lock().conn_for_ledger().execute_batch(
                "ALTER TABLE unavailable_contexts RENAME TO contexts"
            ).unwrap();
            let retried = act_on_executable_answer(&kernel, context, actor, &answer, &ask, "reviewer", &stop).await;
            assert!(matches!(retried, ExecAction::Tell(_) | ExecAction::Settled));
            let completed = kernel.blocks().block_snapshots(context).unwrap();
            assert_eq!(completed.iter().filter(|b| b.content == "approved-once\n").count(), 1,
                "{owner:?}: approved source must run after storage recovers: {completed:?}");
            kernel.kernel_db().lock().update_context_review(context, Some(PrincipalId::new()), Some(reviewer)).unwrap();
            assert!(matches!(act_on_executable_answer(&kernel, context, actor, &answer, &ask, "reviewer", &stop).await,
                ExecAction::Settled));
            let repeated = kernel.blocks().block_snapshots(context).unwrap();
            assert_eq!(repeated.len(), completed.len());
            for (completed, repeated) in completed.iter().zip(repeated.iter()) {
                assert_eq!(repeated.status, completed.status, "a spent approval cannot overwrite accepted output after reassignment");
                assert_eq!(repeated.stderr, completed.stderr);
                assert_eq!(repeated.content, completed.content);
            }
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejected_turn_admission_does_not_repeat_a_delivered_answer() {
        use crate::kj::test_helpers::{test_dispatcher, register_context};
        use approval_ledger::{ask::create_ask, decide::{decide, Answerer, DecideInput}, types::{NewAsk, Origin}};
        let dispatcher = test_dispatcher().await;
        let kernel = dispatcher.kernel().clone();
        let actor = PrincipalId::new();
        let reviewer = PrincipalId::new();
        let context = register_context(&dispatcher, Some("wake-admission"), None, actor);
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let request_id = {
            let db = kernel.kernel_db().lock();
            let now = kaijutsu_types::now_millis() as i64;
            let epoch = db.begin_continuation(context, now).unwrap().epoch;
            assert!(db.record_continuation_request(context, epoch, now).unwrap());
            assert!(db.record_continuation_yield(context, epoch, now).unwrap());
            assert!(db.automatic_resume_allowed(context, epoch, now, 1_800_000).unwrap());
            let request_id = create_ask(db.conn_for_ledger(), &NewAsk {
                context_id: context.as_bytes().to_vec(), actor_id: actor.as_bytes().to_vec(),
                reviewer_id: reviewer.as_bytes().to_vec(), principal_id: actor.as_bytes().to_vec(),
                origin: Origin::KjVerb, instance: None, tool: None, hook_id: None,
                description: "wake once after approval".into(), statements: vec![], authorized_label: None,
                rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
                exec_source: None, exec_stdin: None, continuation_epoch: Some(epoch), env: vec![],
            }).unwrap();
            decide(db.conn_for_ledger(), &request_id, DecideInput {
                allow: true, decided_by: Some(Answerer { principal: reviewer.as_bytes(), context: None }),
                ..Default::default()
            }).unwrap();
            request_id
        };
        // Drive delivery independently of the stopped worker so its attempt
        // to admit the automatic continuation reaches the rejection path.
        kernel.shutdown_runtime_worker().await.unwrap();
        let sub = kernel.ledger_flows().subscribe("ledger.changed");
        let stop = tokio_util::sync::CancellationToken::new();
        let delivery = run_delivery(Arc::downgrade(&kernel), sub, Default::default(), stop.clone());
        tokio::pin!(delivery);
        for generation in 1..=3 {
            // A later answer proves the scan passed admission for the first
            // answer, including any asynchronous configuration reads.
            let marker = format!("scan {generation} complete");
            {
                let db = kernel.kernel_db().lock();
                db.conn_for_ledger().execute("UPDATE approvals SET created_at = 0 WHERE request_id = ?1", [&request_id]).unwrap();
                let marker_id = create_ask(db.conn_for_ledger(), &NewAsk {
                    context_id: context.as_bytes().to_vec(), actor_id: actor.as_bytes().to_vec(),
                    reviewer_id: reviewer.as_bytes().to_vec(), principal_id: actor.as_bytes().to_vec(),
                    origin: Origin::KjVerb, instance: None, tool: None, hook_id: None,
                    description: marker.clone(), statements: vec![], authorized_label: None,
                    rc_run_id: None, expires_at: None, options: vec![], signals: vec![], cwd: None,
                    exec_source: None, exec_stdin: None, continuation_epoch: None, env: vec![],
                }).unwrap();
                decide(db.conn_for_ledger(), &marker_id, DecideInput {
                    allow: true, decided_by: Some(Answerer { principal: reviewer.as_bytes(), context: None }),
                    ..Default::default()
                }).unwrap();
            }
            kernel.ledger_flows().publish(crate::flows::LedgerFlow::Changed { generation });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::select! {
                    _ = &mut delivery => panic!("delivery stopped before processing the scan"),
                    _ = async {
                        loop {
                            if kernel.blocks().block_snapshots(context).unwrap().iter()
                                .any(|block| block.content.contains(&marker)) { break; }
                            tokio::task::yield_now().await;
                        }
                    } => {}
                }
            }).await.expect("approval scan did not reach its final answer");
            let blocks = kernel.blocks().block_snapshots(context).unwrap();
            assert_eq!(blocks.iter().filter(|block| block.content.contains("wake once after approval")).count(), 1,
                "ledger change {generation} repeated an already durable delivery: {blocks:?}");
            assert!(!kernel.turn_in_flight(context));
            assert!(kernel.kernel_db().lock().undelivered_answers().unwrap().iter()
                .any(|answer| answer.request_id == request_id), "delivery must leave the answer redeemable by its caller");
        }
        stop.cancel();
        delivery.await;
    }

    #[tokio::test]
    async fn preparation_unwind_settles_only_its_owned_pair() {
        use futures::FutureExt;
        let kernel = Arc::new(Kernel::new_ephemeral("approval-preparation-panic").await);
        let context = ContextId::new();
        let actor = PrincipalId::new();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let owned = author_pair_for_ask(&kernel, context, actor, "echo approved").unwrap();
        let other = author_pair_for_ask(&kernel, context, actor, "echo unrelated").unwrap();
        let snapshot = |id: &BlockId| kernel.blocks().get_block_snapshot(context, id).unwrap().unwrap();
        let before = snapshot(&other.1);
        let panic = std::panic::AssertUnwindSafe(async {
            let _preparation = ApprovalPreparation {
                kernel: &kernel, context, actor, pair: Some(owned), armed: true,
            };
            tokio::task::yield_now().await;
            panic!("preparation panic sentinel");
        }).catch_unwind().await.unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"preparation panic sentinel"));
        assert_eq!(snapshot(&owned.0).status, Status::Error);
        let output = snapshot(&owned.1);
        assert_eq!(output.status, Status::Error);
        assert!(output.stderr.unwrap_or_default().contains("preparation panicked"));
        assert_eq!(snapshot(&other.1).status, before.status);
        assert_eq!(snapshot(&other.1).content, before.content);
    }

    #[tokio::test]
    async fn preparation_unwind_without_a_pair_records_a_no_run_fact() {
        let kernel = Arc::new(Kernel::new_ephemeral("approval-unlinked-panic").await);
        let context = ContextId::new();
        let actor = PrincipalId::new();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _preparation = ApprovalPreparation {
                kernel: &kernel, context, actor, pair: None, armed: true,
            };
            panic!("unlinked preparation panic");
        })).is_err());
        let blocks = kernel.blocks().block_snapshots(context).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, kaijutsu_types::BlockKind::Error);
        assert!(blocks[0].content.contains("nothing was run"));
        assert!(blocks[0].content.contains("approval is spent"));
    }

    #[tokio::test]
    async fn preparation_releases_ownership_before_command_capture() {
        let kernel = Arc::new(Kernel::new_ephemeral("approval-capture-owner").await);
        let context = ContextId::new();
        let actor = PrincipalId::new();
        kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
        let pair = author_pair_for_ask(&kernel, context, actor, "echo captured").unwrap();
        kernel.blocks().set_status(context, &pair.1, Status::Done).unwrap();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut preparation = ApprovalPreparation {
                kernel: &kernel, context, actor, pair: Some(pair), armed: true,
            };
            preparation.armed = false;
            panic!("command capture owns this panic");
        })).is_err());
        let output = kernel.blocks().get_block_snapshot(context, &pair.1).unwrap().unwrap();
        assert_eq!(output.status, Status::Done);
        assert!(output.stderr.is_none());
    }

    #[tokio::test]
    async fn worker_shutdown_removes_the_approval_subscription() {
        let kernel = Arc::new(Kernel::new_ephemeral("approval-shutdown").await);
        kernel.start_approval_delivery().unwrap();
        assert_eq!(kernel.ledger_flows().subscriber_count(), 1,
            "startup must install the subscription before returning");
        kernel.start_approval_delivery().unwrap();
        assert_eq!(kernel.ledger_flows().subscriber_count(), 1,
            "repeated startup must not create another delivery owner");
        kernel.shutdown_runtime_worker().await.unwrap();
        assert_eq!(kernel.ledger_flows().subscriber_count(), 0,
            "shutdown returned while approval delivery was still listening");
        assert!(kernel.start_approval_delivery().unwrap_err().contains("shut down"));
    }

    #[tokio::test]
    async fn unreadable_backlog_refuses_startup_without_a_subscription() {
        let kernel = Arc::new(Kernel::new_ephemeral("approval-startup-failure").await);
        kernel.kernel_db().lock().conn_for_ledger()
            .execute_batch("ALTER TABLE approvals RENAME TO inaccessible_approvals").unwrap();
        let error = kernel.start_approval_delivery().unwrap_err();
        assert!(error.contains("could not read approval backlog"), "{error}");
        assert_eq!(kernel.ledger_flows().subscriber_count(), 0);
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    #[tokio::test]
    async fn idle_delivery_does_not_keep_its_kernel_alive() {
        let kernel = Arc::new(Kernel::new_ephemeral("approval-weak-owner").await);
        kernel.start_approval_delivery().unwrap();
        let owner = Arc::downgrade(&kernel);
        drop(kernel);
        assert!(owner.upgrade().is_none(), "idle delivery must not own the kernel");
    }

    #[tokio::test]
    async fn shutdown_interrupts_pending_preparation() {
        let stop = tokio_util::sync::CancellationToken::new();
        let preparation = prepare_while_running(&stop,
            std::future::pending::<Result<(), String>>());
        tokio::pin!(preparation);
        assert!(futures::poll!(&mut preparation).is_pending());
        stop.cancel();
        assert!(preparation.await.unwrap_err().contains("shut down before approved execution"));
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

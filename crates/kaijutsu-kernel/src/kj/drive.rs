//! Drive subcommand: clock one autonomous turn on a context.
//!
//! Admit a turn to the kernel runtime, optionally writing a durable seed first.
//! Admission failure returns directly to the caller; accepted work reports its
//! outcome through turn events and the context log.

use clap::Parser;
use kaijutsu_types::ContentType;

use super::effect::{Classify, Effect};
use super::refs;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "drive",
    about = "Admit one autonomous model turn and return its turn ID.",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct DriveArgs {
    /// Seed the turn with this text; when omitted the turn runs against
    /// whatever is already in the context's block log.
    #[arg(long)]
    prompt: Option<String>,
    /// Commit this turn's complete ABC output at this absolute track tick.
    #[arg(long, requires = "track")]
    score_at: Option<i64>,
    /// Track for the intended score commitment; must already be armed.
    #[arg(long, requires = "score_at")]
    track: Option<String>,
    /// Missed or invalid score output uses last-good (default) or skip.
    #[arg(long, requires = "score_at", value_parser = ["last-good", "skip"])]
    fallback: Option<String>,
    /// Target context to drive (label or id); defaults to the current context.
    target: Option<String>,
}

impl KjDispatcher {
    pub(crate) async fn dispatch_drive(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Bare `kj drive` drives this context without a score commitment.
        // Only --help/-h requests help.
        let parsed = match DriveArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj drive: {e}"));
            }
        };

        let score = match parsed.score_at {
            Some(start) => {
                let track = match kaijutsu_types::TrackId::new(parsed.track.as_deref().expect("clap requires track")) {
                    Ok(track) => track,
                    Err(error) => return KjResult::Err(format!("kj drive: {error}")),
                };
                Some(crate::hyoushigi::model::ScoreIntent {
                    track, start: kaijutsu_types::Tick::new(start),
                    fallback: if parsed.fallback.as_deref() == Some("skip") {
                        kaijutsu_hyoushigi::Fallback::Skip
                    } else { kaijutsu_hyoushigi::Fallback::UseLastGood },
                })
            }
            None => None,
        };

        // Self-driving is gated: the caller's loadout must hold `drive`. This is
        // what makes narrowing a musician's binding actually stop its OODA tick.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Drive, "drive") {
            return denied;
        }

        // The positional target context; default to the caller's current
        // context (".") when omitted. `kj drive` drives here; `kj drive
        // <label-or-id>` drives another context.
        let target_ref = parsed.target.as_deref();

        let target = {
            let db = self.kernel_db().lock();
            match refs::resolve_context_arg(target_ref, caller, &db) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj drive: {e}")),
            }
        };

        // Reserve, then admit, before the seed block write below
        // (`docs/resource-admission.md`, rule 1): a refused drive leaves no
        // seed, and archive cannot refuse the turn after the seed lands.
        let slot = match self.kernel().reserve_runtime_slot() {
            Ok(slot) => slot,
            Err(error) => return KjResult::Err(format!("kj drive: turn was not admitted: {error}")),
        };

        // Only Live contexts accept turns. Check archived_at first because it
        // is authoritative; concluded and archived work must be forked to resume.
        let admission = {
            let db = self.kernel_db().lock();
            let row = match db.get_context(target) {
                Ok(Some(row)) => row,
                Ok(None) => return KjResult::Err(format!("kj drive: context {} not found", target.short())),
                Err(e) => return KjResult::Err(format!("kj drive: could not read context {}: {e}", target.short())),
            };
            let refusal = if row.archived_at.is_some() {
                Some("archived — archived contexts are retained work; fork it to continue")
            } else {
                match row.context_state {
                    kaijutsu_types::ContextState::Live => None,
                    kaijutsu_types::ContextState::Staging => Some(
                        "staging — post-fork curation blocks LLM invocation; \
                         finish curating first",
                    ),
                    kaijutsu_types::ContextState::Concluded => {
                        Some("concluded — fork it to continue the work")
                    }
                    kaijutsu_types::ContextState::Archived => Some(
                        "archived — archived contexts are retained work; fork it to continue",
                    ),
                }
            };
            if let Some(why) = refusal {
                let name = row.label.clone().unwrap_or_else(|| target.short());
                return KjResult::Err(format!("kj drive: context '{name}' is {why}"));
            }
            match crate::runtime::admission::ContextAdmission::acquire(&db, target) {
                Ok(admission) => admission,
                Err(error) => return KjResult::Err(format!("kj drive: turn was not admitted: {error}")),
            }
        };

        // A context with no blocks has nothing to anchor a turn after — there's
        // no document/history to act on. Crash loudly rather than publish a
        // turn request with no valid anchor.
        let Some(tail) = self.block_store().last_block_id(target) else {
            return KjResult::Err(format!(
                "kj drive: context '{}' has no blocks to anchor a turn after; \
                 there is nothing to drive",
                target.to_hex()
            ));
        };

        // A prompt becomes a durable User/Text block authored by the caller.
        // Without one, anchor at the existing tail. The model reads the log;
        // event content is an observation, not another copy to insert.
        let seed: String = parsed.prompt.clone().unwrap_or_default();
        let after = match parsed.prompt.as_deref() {
            Some(prompt) => {
                match self.block_store().insert_block_as(
                    target,
                    None,
                    Some(&tail),
                    kaijutsu_types::Role::User,
                    kaijutsu_types::BlockKind::Text,
                    prompt.to_string(),
                    kaijutsu_types::Status::Done,
                    ContentType::Plain,
                    Some(caller.principal_id),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return KjResult::Err(format!(
                            "kj drive: failed to write seed block: {e}"
                        ));
                    }
                }
            }
            None => tail,
        };

        let (turn_id, work_id) = match self.kernel().request_turn_admitted(crate::runtime::turn_request::TurnRequest {
            score,
            context_id: target, after_block_id: after, content: seed,
            principal_id: caller.principal_id, model: None, continuation_epoch: None,
        }, slot, admission) {
            Ok(crate::runtime::turn_request::TurnAdmission::Accepted { turn_id, work_id }) => (turn_id, work_id),
            Ok(crate::runtime::turn_request::TurnAdmission::AlreadyActive) => unreachable!("explicit drive admits a turn"),
            Err(error) => return KjResult::Err(format!("kj drive: turn was not admitted: {error}")),
        };

        // Identify the driven context by a compact handle in the message.
        let display = target.short();
        KjResult::Ok {
            message: format!("driving turn in '{display}'"),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(serde_json::json!({
                "context_id": target.to_hex(),
                "accepted": true,
                "turn_id": turn_id,
                "work_id": work_id,
            })),
        }
    }
}

// Verb class: kj/effect.rs
impl Classify for DriveArgs {
    fn effect(&self) -> Effect {
        // May write a seed block and always publishes a turn request.
        Effect::Write
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjCaller;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// Seed a context with a document and one block so `last_block_id` resolves
    /// — a turn needs an anchor.
    fn seed_with_block(
        d: &super::super::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        principal: PrincipalId,
    ) {
        d.block_store()
            .create_document(ctx, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "seed".to_string(),
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn drive_current_context_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("kj drive should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                ..
            } => {
                assert_eq!(context_id, ctx, "the turn targets the current context");
                assert_eq!(principal_id, c.principal_id);
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn turn_observers_cannot_authorize_admission_after_shutdown() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("stopped"), None, principal);
        seed_with_block(&d, ctx, principal);
        let mut observer = d.kernel().turn_flows().subscribe("turn.requested");
        d.kernel().shutdown_runtime_worker().await.unwrap();
        let result = d.dispatch(&[s("drive")], &caller_with_context(ctx)).await;
        assert!(!result.is_ok(), "an observer accepted work for a stopped executor");
        assert!(result.message().contains("shut down"), "{}", result.message());
        assert!(observer.try_recv().is_none(), "rejected work is not announced as accepted");
        assert!(!d.kernel().turn_in_flight(ctx));
    }

    #[tokio::test]
    async fn headless_admission_does_not_require_event_observers() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("unobserved"), None, principal);
        seed_with_block(&d, ctx, principal);
        let result = d.dispatch(&[s("drive")], &caller_with_context(ctx)).await;
        assert!(result.is_ok(), "runtime admission should not need observers: {}", result.message());
        d.kernel().shutdown_runtime_worker().await.unwrap();
        assert!(!d.kernel().turn_in_flight(ctx));
    }

    /// An archived context must not be drivable. Archived contexts are
    /// *retained work* — kept for referential integrity, later search, and
    /// research — so driving one mutates the record we are preserving.
    ///
    /// This is reachable in practice because label resolution filters
    /// archived rows but `KernelDb::resolve_context` parses a full UUID
    /// first through `get_context`, which does not. It became live the
    /// moment `session.end` started archiving contexts.
    #[tokio::test]
    async fn drive_refuses_an_archived_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let gone = register_context(&d, Some("gone"), None, principal);
        seed_with_block(&d, here, principal);
        seed_with_block(&d, gone, principal);
        d.kernel_db().lock().archive_context(gone).unwrap();

        let c = caller_with_context(here);
        // Address it by full UUID — the path that bypasses the active-context
        // filter and is therefore the one that must be gated here.
        let result = d.dispatch(&[s("drive"), gone.to_hex()], &c).await;
        assert!(!result.is_ok(), "archived context must not be drivable");
        let msg = result.message();
        assert!(msg.contains("archived"), "error should say why: {msg}");
    }

    /// `Concluded` means "done"; its documented recovery is `fork`, not a
    /// turn. Driving one resurrects work its owner explicitly finished.
    #[tokio::test]
    async fn drive_refuses_a_concluded_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let done = register_context(&d, Some("done"), None, principal);
        seed_with_block(&d, here, principal);
        seed_with_block(&d, done, principal);
        d.kernel_db().lock().conclude_context(done).unwrap();

        let c = caller_with_context(here);
        let result = d.dispatch(&[s("drive"), s("done")], &c).await;
        assert!(!result.is_ok(), "concluded context must not be drivable");
        assert!(
            result.message().contains("concluded"),
            "error should say why: {}",
            result.message()
        );
    }

    /// `ContextState::Staging` is documented as "LLM blocked" — post-fork
    /// curation, where the user is still toggling `excluded`. Nothing
    /// enforced that on the drive path before this.
    #[tokio::test]
    async fn drive_refuses_a_staging_context() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let curating = register_context(&d, Some("curating"), None, principal);
        seed_with_block(&d, here, principal);
        seed_with_block(&d, curating, principal);
        d.kernel_db()
            .lock()
            .update_context_state(curating, kaijutsu_types::ContextState::Staging)
            .unwrap();

        let c = caller_with_context(here);
        let result = d.dispatch(&[s("drive"), s("curating")], &c).await;
        assert!(!result.is_ok(), "staging context must not be drivable");
        assert!(
            result.message().contains("staging"),
            "error should say why: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_named_context_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let here = register_context(&d, Some("here"), None, principal);
        let other = register_context(&d, Some("other"), None, principal);
        seed_with_block(&d, here, principal);
        seed_with_block(&d, other, principal);
        let c = caller_with_context(here);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive"), s("other")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("kj drive <ctx> should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested { context_id, .. } => {
                assert_eq!(context_id, other, "the turn targets the named context");
                assert_ne!(context_id, here, "not the caller's context");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_with_prompt_sets_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive"), s("--prompt"), s("go")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub.try_recv().expect("drive --prompt should publish");
        match msg.payload {
            crate::flows::TurnFlow::Requested { content, .. } => {
                assert_eq!(content, "go");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_with_prompt_writes_seed_block_and_anchors_turn() {
        // The prompt must be a durable User/Text block authored by the caller,
        // and the requested turn must anchor after that block.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let before = d.block_store().block_snapshots(ctx).unwrap().len();
        let result = d.dispatch(&[s("drive"), s("--prompt"), s("go")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let blocks = d.block_store().block_snapshots(ctx).unwrap();
        assert_eq!(blocks.len(), before + 1, "one seed block appended");
        let seed = blocks.last().unwrap();
        assert_eq!(seed.content, "go", "seed block carries the prompt");
        assert_eq!(seed.role, kaijutsu_types::Role::User, "seed is the user turn");
        assert_eq!(seed.kind, kaijutsu_types::BlockKind::Text);
        assert_eq!(
            seed.id.principal_id, c.principal_id,
            "seed authored by the driving caller"
        );

        let msg = sub.try_recv().expect("drive --prompt should publish");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                after_block_id, ..
            } => {
                assert_eq!(
                    after_block_id, seed.id,
                    "the turn anchors AFTER the seed block, not the prior tail"
                );
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_without_prompt_writes_no_block() {
        // The drift-then-drive path: bare `kj drive` runs against whatever is
        // already in the log and must NOT append a seed block.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let before = d.block_store().block_snapshots(ctx).unwrap().len();
        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());
        let after = d.block_store().block_snapshots(ctx).unwrap().len();
        assert_eq!(after, before, "no --prompt means no seed block appended");
    }

    #[tokio::test]
    async fn drive_without_prompt_empty_content() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive failed: {}", result.message());

        let msg = sub.try_recv().expect("drive should publish");
        match msg.payload {
            crate::flows::TurnFlow::Requested { content, .. } => {
                assert_eq!(
                    content, "",
                    "no --prompt means empty seed; the turn runs against the log"
                );
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_no_blocks_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        // Context with no document/blocks at all.
        let ctx = register_context(&d, Some("empty"), None, principal);
        let c = caller_with_context(ctx);
        // A subscriber exists, so failure must come from the no-blocks guard,
        // not from the no-driver path.
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            !result.is_ok(),
            "driving a context with no blocks must error, got: {}",
            result.message()
        );
        assert!(
            result.message().contains("no blocks"),
            "error should explain the missing anchor: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_stopped_runtime_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        seed_with_block(&d, ctx, principal);
        let c = caller_with_context(ctx);
        d.kernel().shutdown_runtime_worker().await.unwrap();

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            !result.is_ok(),
            "drive with a stopped runtime must error, got: {}",
            result.message()
        );
        assert!(
            result.message().contains("shut down"),
            "error should explain stopped admission: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_denied_without_drive_capability() {
        // The gate that makes narrowing a musician's binding actually stop its
        // OODA tick: a non-privileged caller whose loadout lacks `drive` is
        // refused before any turn is requested.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("nodrive"), None, principal);
        seed_with_block(&d, ctx, principal);
        // Replace the broad test loadout with everything EXCEPT drive, to prove
        // the denial is specific to the missing `drive` authority. Written to
        // the DB — the authoritative store require_cap reads (the broker's DB
        // handle is unset in test_dispatcher, so broker.set_binding alone would
        // only touch the cache require_cap doesn't consult).
        let mut binding = crate::mcp::ContextToolBinding::new();
        binding.grant(crate::mcp::Capability::AllInstances);
        binding.grant(crate::mcp::Capability::AllFacades);
        binding.grant(crate::mcp::Capability::Operator);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();

        let c = caller_with_context(ctx);
        // A subscriber exists, so a pass would actually publish — isolate the gate.
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(!result.is_ok(), "drive without the `drive` cap must be denied");
        assert!(
            result.message().contains("denied") && result.message().contains("drive"),
            "denial should name the missing capability: {}",
            result.message()
        );

        // Granting `drive` lets the same caller through.
        binding.grant(crate::mcp::Capability::Drive);
        d.kernel_db().lock().upsert_context_binding(ctx, &binding).unwrap();
        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(result.is_ok(), "drive with the `drive` cap should pass: {}", result.message());
    }

    #[tokio::test]
    async fn drive_privileged_caller_bypasses_gate() {
        // The rc lifecycle (privileged kaish) drives without holding `drive` —
        // the control plane exercises loadouts it assigns.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("rc"), None, principal);
        seed_with_block(&d, ctx, principal);
        // Deny-all in the DB (the source require_cap reads) to prove the
        // privileged bypass holds even with zero granted capabilities.
        d.kernel_db()
            .lock()
            .upsert_context_binding(ctx, &crate::mcp::ContextToolBinding::new())
            .unwrap();
        let _sub = d.kernel().turn_flows().subscribe("turn.requested");

        let c = KjCaller {
            privileged: true,
            ..caller_with_context(ctx)
        };
        let result = d.dispatch(&[s("drive")], &c).await;
        assert!(
            result.is_ok(),
            "privileged caller should bypass the drive gate: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn drive_help() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        let c = caller_with_context(ctx);

        // `--help` routes through clap's DisplayHelp (the bare `help` word is no
        // longer special — it would parse as a target context ref).
        let result = d.dispatch(&[s("drive"), s("--help")], &c).await;
        assert!(result.is_ok(), "help failed: {}", result.message());
        assert!(
            result.message().contains("Usage") && result.message().contains("--prompt"),
            "help should carry clap usage + the --prompt flag: {}",
            result.message()
        );
    }
}

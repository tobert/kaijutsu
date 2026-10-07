//! Stage subcommands: commit, status, include, exclude.
//!
//! `commit` and `status` manage the staging state a `kj fork --stage` child
//! starts in. `include` and `exclude` curate blocks in a live or staging
//! context; [`crate::drift::DriftRouter::require_curatable`] owns that rule
//! for `kj` and the `setBlockExcluded` RPC alike.
//! Verb aliases: commit↔go, status↔st, include↔in, exclude↔ex.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, ContextState};

use super::effect::{Classify, Effect};
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "stage",
    about = "Curate a context's blocks, and commit a staged fork",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct StageArgs {
    #[command(subcommand)]
    command: StageCommand,
}

#[derive(Subcommand, Debug)]
enum StageCommand {
    /// Transition from Staging to Live. Merges the staged child live (a
    /// cross-context write) — same authority as `drift merge`.
    #[command(alias = "go")]
    Commit,
    /// Show staging state and block counts.
    #[command(alias = "st")]
    Status,
    /// Put an excluded block back. Live and staging contexts only.
    #[command(alias = "in")]
    Include {
        /// Block key (suffix match on the full id key)
        block_key: String,
    },
    /// Leave a block out of what models read.
    ///
    /// Live and staging contexts only. Instruction blocks and council contexts change on the next turn
    /// or council decision. History a model has already read stays in its
    /// live conversation until the next fork.
    #[command(alias = "ex")]
    Exclude {
        /// Block key (suffix match on the full id key)
        block_key: String,
    },
}

impl KjDispatcher {
    pub(crate) async fn dispatch_stage(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Bare `kj stage` (empty sub-args) shows status — a valid default
        // operation, NOT a help request (unlike subcommand-required tools like
        // cas). `--help`/`-h` still route to clap's DisplayHelp below.
        let command = if argv.is_empty() {
            StageCommand::Status
        } else {
            match StageArgs::try_parse_from(argv) {
                Ok(p) => p.command,
                Err(e) => {
                    // `--help` / `-h` requests come through as DisplayHelp
                    // errors; route them to ok-ephemeral so kaish prints them.
                    if matches!(
                        e.kind(),
                        clap::error::ErrorKind::DisplayHelp
                            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    ) {
                        return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                    }
                    return KjResult::Err(format!("kj stage: {e}"));
                }
            }
        };

        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // `commit` merges the staged child live (a cross-context write) — same
        // risk class as `drift merge`, so it shares the `drift` authority. The
        // status/include/exclude curation verbs stay ungated.
        if matches!(command, StageCommand::Commit)
            && let Err(denied) =
                self.require_cap(caller, crate::mcp::Capability::Drift, "stage commit")
        {
            return denied;
        }

        match command {
            StageCommand::Commit => self.stage_commit(context_id).await,
            StageCommand::Status => self.stage_status(context_id),
            StageCommand::Include { block_key } => self.stage_toggle(&block_key, context_id, false),
            StageCommand::Exclude { block_key } => self.stage_toggle(&block_key, context_id, true),
        }
    }

    fn require_staging(&self, context_id: ContextId) -> Result<(), KjResult> {
        let state = {
            let drift = self.drift_router().read();
            drift.context_state(context_id)
        };
        match state {
            Some(ContextState::Staging) => Ok(()),
            Some(other) => Err(KjResult::Err(format!(
                "kj stage: context is in {other} state, not staging"
            ))),
            None => Err(KjResult::Err(
                "kj stage: context not found in drift router".to_string(),
            )),
        }
    }

    async fn stage_commit(&self, context_id: ContextId) -> KjResult {
        if let Err(result) = self.require_staging(context_id) {
            return result;
        }

        // Transition DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let Err(e) = drift.set_state(context_id, ContextState::Live) {
                return KjResult::Err(format!("kj stage commit: {e}"));
            }
        }

        // Persist to KernelDb
        {
            let db = self.kernel_db().lock();
            if let Err(e) = db.update_context_state(context_id, ContextState::Live) {
                tracing::warn!("kj stage commit: KernelDb update failed: {e}");
            }
        }

        KjResult::ok(format!(
            "committed — context {} is now live",
            context_id.short()
        ))
    }

    fn stage_status(&self, context_id: ContextId) -> KjResult {
        let state = {
            let drift = self.drift_router().read();
            drift
                .context_state(context_id)
                .unwrap_or(ContextState::Live)
        };

        let (total, excluded_count, by_kind) = match self.block_store().block_snapshots(context_id)
        {
            Ok(blocks) => {
                let total = blocks.len();
                let excluded = blocks.iter().filter(|b| b.excluded).count();
                let mut kinds = std::collections::BTreeMap::<String, (usize, usize)>::new();
                for b in &blocks {
                    let key = format!("{:?}/{:?}", b.role, b.kind);
                    let entry = kinds.entry(key).or_default();
                    entry.0 += 1;
                    if b.excluded {
                        entry.1 += 1;
                    }
                }
                (total, excluded, kinds)
            }
            Err(_) => (0, 0, std::collections::BTreeMap::new()),
        };

        let mut lines = vec![format!(
            "**state:** {} | **blocks:** {} ({} excluded)",
            state, total, excluded_count
        )];

        if !by_kind.is_empty() {
            lines.push(String::new());
            for (kind, (count, ex)) in &by_kind {
                if *ex > 0 {
                    lines.push(format!("  {kind}: {count} ({ex} excluded)"));
                } else {
                    lines.push(format!("  {kind}: {count}"));
                }
            }
        }

        KjResult::ok_ephemeral(lines.join("\n"), ContentType::Markdown)
    }

    fn stage_toggle(&self, block_key: &str, context_id: ContextId, excluded: bool) -> KjResult {
        if let Err(e) = self.drift_router().read().require_curatable(context_id) {
            return KjResult::Err(format!("kj stage: {e}"));
        }
        // Find the block by suffix match on the key
        let blocks = match self.block_store().block_snapshots(context_id) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj stage: {e}")),
        };

        let matching: Vec<_> = blocks
            .iter()
            .filter(|b| {
                let key = b.id.to_key();
                // Suffix/full match on the `to_key()` form, plus the short
                // `<principal8>#<seq>` form `kj block list`'s table prints
                // (same addressing accepted by `kj block read` et al.).
                key.ends_with(block_key)
                    || key == block_key
                    || super::block::short_key(&b.id) == block_key
            })
            .collect();

        match matching.len() {
            0 => KjResult::Err(format!("kj stage: no block matching '{block_key}'")),
            1 => {
                let block_id = matching[0].id;
                if let Err(e) = self.block_store().set_excluded(context_id, &block_id, excluded) {
                    return KjResult::Err(format!("kj stage: {e}"));
                }
                let verb = if excluded { "excluded" } else { "included" };
                KjResult::ok(format!("{verb} block {}", block_id.to_key()))
            }
            n => KjResult::Err(format!(
                "kj stage: '{block_key}' matches {n} blocks — be more specific"
            )),
        }
    }
}

// Verb class: kj/effect.rs
impl Classify for StageArgs {
    fn effect(&self) -> Effect {
        self.command.effect()
    }
}

impl Classify for StageCommand {
    fn effect(&self) -> Effect {
        match self {
            StageCommand::Status => Effect::Read,
            StageCommand::Commit | StageCommand::Include { .. } | StageCommand::Exclude { .. } => {
                Effect::Write
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::KjResult;
    use kaijutsu_types::{BlockKind, ContentType, ContextState, DocKind, PrincipalId, Role, Status};

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// A context with one user text block, in `state`. Returns the context
    /// and the block's short key.
    fn context_with_block(
        d: &crate::kj::KjDispatcher,
        state: ContextState,
    ) -> (kaijutsu_types::ContextId, kaijutsu_types::BlockId) {
        let ctx = register_context(d, Some("c"), None, PrincipalId::new());
        d.block_store().create_document(ctx, DocKind::Conversation, None).expect("create_document");
        let block = d
            .block_store()
            .insert_block_as(ctx, None, None, Role::User, BlockKind::Text, "old guidance", Status::Done, ContentType::Plain, None)
            .expect("insert_block_as");
        d.drift_router().write().set_state(ctx, state).expect("set_state");
        (ctx, block)
    }

    fn excluded(d: &crate::kj::KjDispatcher, ctx: kaijutsu_types::ContextId, block: &kaijutsu_types::BlockId) -> bool {
        d.block_store().block_snapshots(ctx).unwrap().iter().find(|b| b.id == *block).unwrap().excluded
    }

    #[tokio::test]
    async fn exclude_and_include_curate_a_live_context() {
        let d = test_dispatcher().await;
        let (ctx, block) = context_with_block(&d, ContextState::Live);
        let c = caller_with_context(ctx);
        let key = super::super::block::short_key(&block);

        let result = d.dispatch(&[s("stage"), s("exclude"), key.clone()], &c).await;
        assert!(result.is_ok(), "exclude on a live context: {}", result.message());
        assert!(excluded(&d, ctx, &block));

        let result = d.dispatch(&[s("stage"), s("include"), key], &c).await;
        assert!(result.is_ok(), "include on a live context: {}", result.message());
        assert!(!excluded(&d, ctx, &block));
    }

    #[tokio::test]
    async fn exclude_curates_a_staging_context() {
        let d = test_dispatcher().await;
        let (ctx, block) = context_with_block(&d, ContextState::Staging);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("stage"), s("ex"), super::super::block::short_key(&block)], &c).await;
        assert!(result.is_ok(), "exclude on a staging context: {}", result.message());
        assert!(excluded(&d, ctx, &block));
    }

    #[tokio::test]
    async fn exclude_refuses_a_concluded_context() {
        let d = test_dispatcher().await;
        let (ctx, block) = context_with_block(&d, ContextState::Concluded);
        let c = caller_with_context(ctx);

        let result = d.dispatch(&[s("stage"), s("exclude"), super::super::block::short_key(&block)], &c).await;
        match &result {
            KjResult::Err(msg) => assert!(msg.contains("concluded"), "names the state: {msg}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!excluded(&d, ctx, &block));
    }
}

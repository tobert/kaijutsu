//! Stage subcommands: commit, status, include, exclude.
//!
//! Manages the liminal staging state during fork curation.
//! Blocks can be toggled in/out before the conversation goes live.
//!
//! Migrated to clap_derive following the `cas`/`block` pattern: one
//! `StageArgs` struct + a `StageCommand` enum at the top, `dispatch_stage`
//! parses argv via `try_parse_from`, then matches the variant to the per-verb
//! function. The handler bodies stayed intact — only argv extraction moved
//! into the derive. Verb aliases (commit↔go, status↔st, include↔in,
//! exclude↔ex) are modeled with `#[command(alias = "...")]`.

use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId, ContextState};

use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "stage",
    about = "Manage liminal staging state for fork curation",
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
    /// Set excluded=false on a block.
    #[command(alias = "in")]
    Include {
        /// Block key (suffix match on the full id key)
        block_key: String,
    },
    /// Set excluded=true on a block.
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
            StageCommand::Include { block_key } => {
                self.stage_include(&block_key, context_id)
            }
            StageCommand::Exclude { block_key } => {
                self.stage_exclude(&block_key, context_id)
            }
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

    fn stage_include(&self, block_key: &str, context_id: ContextId) -> KjResult {
        if let Err(result) = self.require_staging(context_id) {
            return result;
        }
        self.stage_toggle(block_key, context_id, false)
    }

    fn stage_exclude(&self, block_key: &str, context_id: ContextId) -> KjResult {
        if let Err(result) = self.require_staging(context_id) {
            return result;
        }
        self.stage_toggle(block_key, context_id, true)
    }

    fn stage_toggle(&self, block_key: &str, context_id: ContextId, excluded: bool) -> KjResult {
        // Find the block by suffix match on the key
        let blocks = match self.block_store().block_snapshots(context_id) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj stage: {e}")),
        };

        let matching: Vec<_> = blocks
            .iter()
            .filter(|b| {
                let key = b.id.to_key();
                key.ends_with(block_key) || key == block_key
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

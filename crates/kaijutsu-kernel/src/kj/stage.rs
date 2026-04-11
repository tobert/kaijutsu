//! Stage subcommands: commit, status, include, exclude.
//!
//! Manages the liminal staging state during fork curation.
//! Blocks can be toggled in/out before the conversation goes live.

use kaijutsu_types::{ContentType, ContextId, ContextState};

use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_stage(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Help doesn't need a context, dispatch it before the guard.
        if matches!(argv.first().map(|s| s.as_str()), Some("help" | "--help" | "-h")) {
            return KjResult::ok_ephemeral(self.stage_help(), ContentType::Markdown);
        }

        let context_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        if argv.is_empty() {
            return self.stage_status(context_id);
        }

        match argv[0].as_str() {
            "commit" | "go" => self.stage_commit(context_id).await,
            "status" | "st" => self.stage_status(context_id),
            "include" | "in" => self.stage_include(&argv[1..], context_id),
            "exclude" | "ex" => self.stage_exclude(&argv[1..], context_id),
            other => KjResult::Err(format!(
                "kj stage: unknown subcommand '{}'\n\n{}",
                other,
                self.stage_help()
            )),
        }
    }

    fn stage_help(&self) -> String {
        [
            "## kj stage",
            "",
            "Manage liminal staging state for fork curation.",
            "",
            "**Subcommands:**",
            "- `commit` / `go` — transition from Staging to Live",
            "- `status` / `st` — show staging state and block counts",
            "- `include <block-id>` / `in` — set excluded=false on a block",
            "- `exclude <block-id>` / `ex` — set excluded=true on a block",
        ]
        .join("\n")
    }

    fn require_staging(&self, context_id: ContextId) -> Result<(), KjResult> {
        let state = {
            let drift = self.drift_router().blocking_read();
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
            let mut drift = self.drift_router().write().await;
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
            let drift = self.drift_router().blocking_read();
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

    fn stage_include(&self, argv: &[String], context_id: ContextId) -> KjResult {
        if let Err(result) = self.require_staging(context_id) {
            return result;
        }
        self.stage_toggle(argv, context_id, false)
    }

    fn stage_exclude(&self, argv: &[String], context_id: ContextId) -> KjResult {
        if let Err(result) = self.require_staging(context_id) {
            return result;
        }
        self.stage_toggle(argv, context_id, true)
    }

    fn stage_toggle(&self, argv: &[String], context_id: ContextId, excluded: bool) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err("kj stage: missing block ID".to_string());
        }

        let block_key = &argv[0];

        // Find the block by suffix match on the key
        let blocks = match self.block_store().block_snapshots(context_id) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj stage: {e}")),
        };

        let matching: Vec<_> = blocks
            .iter()
            .filter(|b| {
                let key = b.id.to_key();
                key.ends_with(block_key.as_str()) || key == *block_key
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

//! Fork subcommand: spawn a child context from the current one.
//!
//! Fork strategy is KV-cache strategy (copy cost is a non-issue — storage is
//! cheap). Every shape is an **interval selection** over the parent's ordered
//! block log (`docs/fork-filters.md`): a preset supplies the `base`, `--include`
//! / `--exclude` ranges refine it, and the result drives the copy.
//! - **Full fork** (default, no narrowing) — the whole context into a fresh
//!   lineage = a NEW KV cache (resume-as-another-model, orchestrator repair,
//!   drift-a-summary-back). Preserves DTE history (the plain `fork_document`).
//! - **Filtered fork** — any narrowing selection (`--preset window|spawn`, a
//!   user patch, or ad-hoc `--include`/`--exclude` ranges; `--exclude <block>`
//!   exact-key drops still ride here). Routes through `fork_document_filtered`.
//!   Last-N is spelled `--include end-N:` (the retired `--shallow`/`--depth`).
//! - **Compact fork** (`--compact`) — distill a summary seed (`fork_compact`).
//! - **Subtree fork** (`--as`) — clone a template subtree (`fork_subtree`).

use std::collections::HashMap;

use clap::Parser;
use kaijutsu_types::{ConsentMode, ContentType, ContextId, ContextState, EdgeKind, ForkKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow, ContextShellRow};

use super::parse::resolve_model_choice;
use super::{KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug, Default)]
#[command(name = "fork", about = "Fork the current context into a child", disable_help_subcommand = true, no_binary_name = true)]
pub(crate) struct ForkArgs {
    /// Label for the child (--name/-n)
    #[arg(long, short = 'n')]
    name: Option<String>,
    /// Seed prompt; drives the child's turn
    #[arg(long)]
    prompt: Option<String>,
    /// Preset to apply to the child
    #[arg(long)]
    preset: Option<String>,
    /// Override cwd on the forked context
    #[arg(long)]
    pwd: Option<String>,
    /// Model spec provider/model (or bare model)
    #[arg(long, short = 'm')]
    model: Option<String>,
    /// Distillation model for compact forks
    #[arg(long = "distill-model")]
    distill_model: Option<String>,
    /// Subtree template context ref; presence selects subtree mode
    #[arg(long = "as")]
    as_template: Option<String>,
    /// Start the child in liminal staging state
    #[arg(long, visible_alias = "staging")]
    stage: bool,
    /// Move the session to the child after forking
    #[arg(long)]
    switch: bool,
    /// Compact (distill) fork
    #[arg(long)]
    compact: bool,
    /// Include only these ranges (repeatable). A range is `[lo]:[hi]`,
    /// half-open `[lo, hi)`, endpoints `int | end | end-N` (e.g. `0:5`,
    /// `end-10:`, `:`). Narrows the selection; every explicit `--include` must
    /// survive the resolved keep-set or the fork refuses (no silent winner).
    #[arg(long)]
    include: Vec<String>,
    /// Exclude from the fork (repeatable). Either a range (`10:20`, `end-3:` —
    /// same grammar as `--include`) or an exact block key in `context:agent:seq`
    /// form (the orchestrator-repair path, "fork X without the block that blew
    /// it up"). A value with the wrong colon count for a range is taken as a
    /// block key and must exist in the source.
    #[arg(long)]
    exclude: Vec<String>,
}

/// Resolved provider+model for a fork.
struct ResolvedModel {
    provider: Option<String>,
    model: Option<String>,
    /// True when `--model` was explicitly given (needs `configure_llm` call).
    explicit: bool,
}

/// The fork-time interval selection, resolved at the fork instant from the
/// recalled preset patch (`base` + stored include/exclude rows) composed with
/// the on-this-command `--include` / `--exclude` ranges, plus the dedicated
/// `--exclude <block-key>` exact drops. See `docs/fork-filters.md`.
struct ForkSelection {
    /// Positional keep-set over the parent's fork-instant ordered snapshot.
    /// `None` only on the plain full-copy fast path (base `full`, nothing
    /// narrowing) — which keeps the history-preserving `fork_document` copy
    /// instead of the snapshot-rebuilding filtered copy.
    selection: Option<kaijutsu_crdt::IntervalSet>,
    /// CLI `--exclude <block-key>` exact-form drops, validated present in the
    /// source. Composed as a predicate exclusion on top of the positional
    /// selection (the orchestrator-repair path).
    exclude_block_ids: std::collections::HashSet<String>,
    /// True when the fork narrows the parent → `ForkKind::Filtered` + the
    /// filtered copy path. False = a plain full copy (`ForkKind::Full`).
    filtered: bool,
}

/// Parse a list of range specs into a canonical [`IntervalSet`], surfacing the
/// offending spec alongside the [`RangeError`] for a quotable message.
fn parse_range_specs(
    specs: &[String],
    len: usize,
) -> Result<kaijutsu_crdt::IntervalSet, (String, kaijutsu_crdt::RangeError)> {
    let mut ranges = Vec::with_capacity(specs.len());
    for s in specs {
        let r = kaijutsu_crdt::parse_range(s, len).map_err(|e| (s.clone(), e))?;
        ranges.push(r);
    }
    Ok(kaijutsu_crdt::IntervalSet::from_ranges(ranges))
}

/// Render canonical runs as `lo:hi, …` for an error message naming the
/// positions that violated the include invariant.
fn fmt_runs(runs: &[std::ops::Range<usize>]) -> String {
    runs.iter()
        .map(|r| format!("{}:{}", r.start, r.end))
        .collect::<Vec<_>>()
        .join(", ")
}

impl KjDispatcher {
    /// Resolve the model for a fork: parse `--model`, validate provider, or inherit from parent.
    async fn resolve_fork_model(
        &self,
        model_spec: Option<&str>,
        source_id: ContextId,
    ) -> Result<ResolvedModel, String> {
        // Read parent's provider+model from DriftRouter (before any mutations)
        let (parent_provider, parent_model) = {
            let router = self.drift_router().read();
            router
                .get(source_id)
                .map(|h| (h.provider.clone(), h.model.clone()))
                .unwrap_or((None, None))
        };

        match model_spec {
            Some(spec) => {
                // Same resolver as `kj context set` — resolves `models.toml`
                // aliases (`deepseek-lite`), validates an explicit provider, and
                // fails loud on the `provider:model` colon footgun. Before this,
                // fork skipped alias resolution and silently pinned a bare alias
                // to the default provider (anthropic), shipping the literal name
                // → turn-time `not_found_error: model: <alias>`.
                let registry = self.kernel().llm().read().await;
                let (provider, model) = resolve_model_choice(&registry, spec)?;
                Ok(ResolvedModel {
                    provider,
                    model,
                    explicit: true,
                })
            }
            None => Ok(ResolvedModel {
                provider: parent_provider,
                model: parent_model,
                explicit: false,
            }),
        }
    }

    /// Recall a preset patch and compose it with the CLI ranges into the
    /// fork-time keep-set (`kept = (base ∩ ∪cli_inc) \ ∪exc`), resolved against
    /// the parent's fork-instant ordered snapshot.
    ///
    /// Composition (Amy 2026-06-12 #2): a preset's stored includes WIDEN the
    /// recalled base (a patch may resurrect a section) and are NOT
    /// invariant-checked — only on-this-command CLI `--include`s are sacred and
    /// get the loud include invariant. Excludes union across layers (preset rows
    /// ∪ CLI). `--exclude` values that aren't ranges are taken as exact block
    /// keys (validated present) and ride as a predicate exclusion.
    async fn resolve_fork_selection(
        &self,
        source_id: ContextId,
        preset_label: Option<&str>,
        cli_includes: &[String],
        cli_excludes: &[String],
        before_timestamp: u64,
    ) -> Result<ForkSelection, String> {
        use kaijutsu_crdt::{IntervalSet, RangeError, SelectionError};

        // The fork-instant ordered snapshot — the universe positions address
        // (order_key / BlockId order, the same `fork_filtered` rebuilds). MUST be
        // filtered by the SAME `before_timestamp` the copy uses: a block appended
        // between this read and the copy would shift every tail position by one,
        // so the resolved selection would index the wrong blocks. One timestamp,
        // captured by the caller, threads through both.
        let snapshots: Vec<_> = self
            .block_store()
            .block_snapshots(source_id)
            .map_err(|e| format!("could not read source blocks: {e}"))?
            .into_iter()
            .filter(|s| s.created_at <= before_timestamp)
            .collect();
        let len = snapshots.len();

        // ── Recall the preset patch (a snapshot — later edits don't reach an
        // already-forked context). Factory `full`/`window`/`spawn` carry only a
        // `base` row; a user patch may add `include`/`exclude` rows. Other arg
        // names (model knobs) are applied via `apply_preset`, not here.
        let mut base_selector = "full".to_string();
        let mut preset_inc_specs: Vec<String> = Vec::new();
        let mut preset_exc_specs: Vec<String> = Vec::new();
        if let Some(label) = preset_label {
            let preset_id = {
                let db = self.kernel_db().lock();
                db.get_preset_by_label(label)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("preset '{label}' not found"))?
                    .preset_id
            };
            let args = {
                let db = self.kernel_db().lock();
                db.get_preset_args(preset_id, "fork").map_err(|e| e.to_string())?
            };
            for a in args {
                match a.arg_name.as_str() {
                    "base" => base_selector = a.arg_value,
                    "include" => preset_inc_specs.push(a.arg_value),
                    "exclude" => preset_exc_specs.push(a.arg_value),
                    _ => {}
                }
            }
        }

        // ── Resolve the base selector → IntervalSet over [0, len) ───────────
        let base_is_full = base_selector == "full";
        let base = match base_selector.as_str() {
            "full" => IntervalSet::full(len),
            "spawn" => IntervalSet::empty(),
            "window" => {
                // The `window` shape reads the PARENT's hydration policy. No row
                // = a configuration mistake ("no notch is defined here"), loud per
                // docs/fork-filters.md — not a degenerate full.
                let policy = {
                    let db = self.kernel_db().lock();
                    db.get_hydration_policy(source_id).map_err(|e| e.to_string())?
                };
                let (marker, window) = policy.ok_or_else(|| {
                    "preset 'window' needs a hydration policy on the parent, but none is set \
                     — mark one with `kj context hydrate --mark`, or pick a different preset"
                        .to_string()
                })?;
                // A marker absent from the snapshot is anomalous (markers point at
                // durable blocks) — fail-safe to the whole log rather than hide
                // context behind a stale marker (matches the hydrate side).
                let marker_idx = snapshots.iter().position(|b| b.id == marker);
                if marker_idx.is_none() {
                    tracing::warn!(
                        context_id = %source_id,
                        "kj fork --preset window: hydration marker not in snapshot; \
                         carrying the whole log"
                    );
                }
                kaijutsu_crdt::window_base(len, marker_idx, window as usize)
            }
            other => {
                // Forward-looking: a user patch may store a literal range as base.
                let r = kaijutsu_crdt::parse_range(other, len)
                    .map_err(|e| format!("preset base '{other}': {e}"))?;
                IntervalSet::from_ranges([r])
            }
        };

        // Preset includes WIDEN the base; preset excludes union into the
        // subtraction set.
        let preset_inc = parse_range_specs(&preset_inc_specs, len)
            .map_err(|(s, e)| format!("preset include '{s}': {e}"))?;
        let preset_exc = parse_range_specs(&preset_exc_specs, len)
            .map_err(|(s, e)| format!("preset exclude '{s}': {e}"))?;
        let effective_base = base.union(&preset_inc);

        // CLI includes — ranges only, sacred (the loud include invariant).
        let cli_inc = parse_range_specs(cli_includes, len)
            .map_err(|(s, e)| format!("--include '{s}': {e}"))?;
        let cli_inc_opt = if cli_includes.is_empty() { None } else { Some(cli_inc) };

        // CLI excludes — a range, or (NotARange = the wrong colon count) the exact
        // `--exclude <block-key>` form. A typo'd/absent key fails LOUD: a silent
        // no-op would leave the offending block in a "repaired" child.
        let mut cli_exc_ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut exclude_block_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for spec in cli_excludes {
            match kaijutsu_crdt::parse_range(spec, len) {
                Ok(r) => cli_exc_ranges.push(r),
                Err(RangeError::NotARange(_)) => {
                    let id = kaijutsu_types::BlockId::from_key(spec).ok_or_else(|| {
                        format!("--exclude '{spec}': not a range and not a valid block key")
                    })?;
                    if !snapshots.iter().any(|b| b.id == id) {
                        return Err(format!("--exclude block '{spec}' is not in this context"));
                    }
                    exclude_block_ids.insert(id.to_key());
                }
                Err(e) => return Err(format!("--exclude '{spec}': {e}")),
            }
        }
        let excludes = preset_exc.union(&IntervalSet::from_ranges(cli_exc_ranges));

        // ── Compose: kept = (effective_base ∩ ∪cli_inc) \ ∪exc ──────────────
        // The include invariant names the culprit: the preset's shape (when one
        // was recalled) or an exclude. No silent winner.
        let kept = kaijutsu_crdt::resolve_keep_set(&effective_base, cli_inc_opt.as_ref(), &excludes)
            .map_err(|SelectionError::IncludeViolation { missing }| {
                let culprit = match preset_label {
                    Some(label) => format!("preset '{label}' or an exclude"),
                    None => "an exclude".to_string(),
                };
                format!(
                    "--include conflicts with the selection: positions {} fall outside the kept \
                     set ({culprit} removed them). Drop the preset, adjust the range, or exclude \
                     explicitly.",
                    fmt_runs(&missing)
                )
            })?;

        // Block-key excludes are a PREDICATE applied during the copy, not part of
        // the positional `excludes` set — so `resolve_keep_set` can't see them eat
        // an explicit include. Close that hole here: an exact `--exclude <key>`
        // landing on a CLI-`--include`d position is the same loud contradiction
        // (`--include 0:5 --exclude <block@2>` must refuse, no silent winner).
        if let Some(inc) = &cli_inc_opt
            && !exclude_block_ids.is_empty()
        {
            let clobbered: Vec<String> = snapshots
                .iter()
                .enumerate()
                .filter(|(pos, snap)| {
                    inc.contains_position(*pos) && exclude_block_ids.contains(&snap.id.to_key())
                })
                .map(|(pos, snap)| format!("{} (position {pos})", snap.id.to_key()))
                .collect();
            if !clobbered.is_empty() {
                return Err(format!(
                    "--include conflicts with --exclude: the block-key exclude(s) {} sit inside an \
                     explicit --include range. Drop the --exclude or narrow the --include.",
                    clobbered.join(", ")
                ));
            }
        }

        // Plain full-copy fast path: base `full`, nothing narrowing → keep the
        // history-preserving `fork_document` copy + ForkKind::Full. Any preset
        // base other than full, any include, or any exclusion narrows.
        let narrows = !base_is_full
            || cli_inc_opt.is_some()
            || !excludes.is_empty()
            || !exclude_block_ids.is_empty();

        Ok(ForkSelection {
            selection: if narrows { Some(kept) } else { None },
            exclude_block_ids,
            filtered: narrows,
        })
    }

    /// Fail fast when a fork's requested `--name` is already taken — BEFORE any
    /// context/document/LLM work — so a conflict can't strand a half-built child
    /// (an orphan distilled document) and the caller gets an actionable message
    /// (the existing context's full id + how to reach it) instead of a bare
    /// unique-constraint bounce from deep inside the insert. The DB's unique
    /// index stays the real guard: a label that wins the race between this check
    /// and the insert still fails loud there. `None`/free label → `Ok`.
    fn ensure_label_available(&self, label: Option<&str>) -> Result<(), String> {
        let Some(label) = label else {
            return Ok(());
        };
        let existing = {
            let db = self.kernel_db().lock();
            db.find_context_by_label(label).map_err(|e| e.to_string())?
        };
        if let Some(row) = existing {
            let id = row.context_id.to_hex();
            return Err(format!(
                "label '{label}' is already in use by context {id} — switch to it with \
                 `kj context switch {id}`, or fork under a different --name"
            ));
        }
        Ok(())
    }

    pub(crate) async fn dispatch_fork(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        let args = match ForkArgs::try_parse_from(argv) {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj fork: {e}"));
            }
        };

        // All fork variants snapshot a context into a child — gated on `fork`.
        if let Err(denied) = self.require_cap(caller, crate::mcp::Capability::Fork, "fork") {
            return denied;
        }

        if args.compact {
            return self.fork_compact(&args, caller).await;
        }
        if args.as_template.is_some() {
            return self.fork_subtree(&args, caller).await;
        }

        self.fork_full(&args, caller).await
    }

    /// Apply MCP fork mode exclusions to a newly forked context.
    ///
    /// Servers with `McpForkMode::Exclude` have their tools denied via ToolFilter.
    /// Called after drift.register_fork() so the context handle exists.
    async fn apply_fork_mcp_exclusions(&self, _new_id: ContextId) {
        // MCP fork-mode exclusions were removed alongside the legacy MCP
        // pool in Phase 1 M5. A Phase 2+ replacement will live against
        // ExternalMcpServer health/policy.
    }

    async fn fork_full(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let label = args.name.clone();
        let prompt = args.prompt.clone();
        let preset_label = args.preset.clone();
        let pwd_override = args.pwd.clone();
        let staging = args.stage;

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Reject a taken label up front — before copying the document — so a
        // conflict can't strand an orphan copy and the caller gets an
        // actionable message instead of a late unique-constraint bounce.
        if let Err(e) = self.ensure_label_available(label.as_deref()) {
            return KjResult::Err(format!("kj fork: {e}"));
        }

        let new_id = ContextId::new();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(args.model.as_deref(), source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork: {e}")),
        };

        // One fork-instant timestamp threads through selection resolution AND the
        // copy, so both address the SAME block universe (no off-by-one from a
        // concurrent append between the two reads).
        let fork_ts = kaijutsu_types::now_millis();

        // Recall the preset patch + compose the CLI ranges into the fork-time
        // keep-set, BEFORE any mutations. A range/preset/key error (including the
        // loud include invariant, and a typo'd `--exclude` block key) fails here.
        let selection = match self
            .resolve_fork_selection(
                source_id,
                preset_label.as_deref(),
                &args.include,
                &args.exclude,
                fork_ts,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj fork: {e}")),
        };
        // A narrowing selection is a Filtered fork; a plain full copy stays Full.
        let fork_kind = if selection.filtered {
            ForkKind::Filtered
        } else {
            ForkKind::Full
        };

        // Deep-copy the BlockStore document. Plain full copy by default (the
        // history-preserving path); a narrowing selection routes through the
        // filtered copy (snapshot-rebuilt with the positional keep-set +
        // block-key drops).
        let copy = if selection.filtered {
            let filter = kaijutsu_crdt::ForkBlockFilter {
                selection: selection.selection.clone(),
                exclude_block_ids: selection.exclude_block_ids.clone(),
                ..Default::default()
            };
            self.block_store()
                .fork_document_filtered(source_id, new_id, fork_ts, &filter)
        } else {
            self.block_store().fork_document(source_id, new_id)
        };
        if let Err(e) = copy {
            return KjResult::Err(format!("kj fork: failed to copy document: {e}"));
        }

        // 3d — hydration policy travel. The policy row travels iff the marked
        // block survived the selection. The marker remap is mechanical: fork
        // preserves `(principal, seq)` and rewrites only the context part, so
        // `P_child = BlockId::new(child_ctx, P.principal_id, P.seq)` (see
        // `docs/fork-filters.md`). This ONE rule yields all the documented
        // cases — full: marker always copied → travels; window: marker is the
        // prefix end, in the kept set by construction → travels; spawn: nothing
        // copied → marker absent → dropped (child rc re-marks); ad-hoc: iff the
        // marker survived. A *corrupt* parent policy fails the fork loud (same
        // posture as the hydrate side), not a silent drop.
        let parent_policy = match self.kernel_db().lock().get_hydration_policy(source_id) {
            Ok(p) => p,
            Err(e) => return KjResult::Err(format!("kj fork: {e}")),
        };
        let child_marker = parent_policy
            .as_ref()
            .map(|(m, _)| kaijutsu_types::BlockId::new(new_id, m.principal_id, m.seq));
        let policy_travels = child_marker.as_ref().is_some_and(|cm| {
            self.block_store()
                .get_block_snapshot(new_id, cm)
                .ok()
                .flatten()
                .is_some()
        });

        // Write-through: KernelDb then DriftRouter
        {
            let mut db = self.kernel_db().lock();

            // Inherit workspace from source
            let source_ws = db
                .get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: if staging {
                    ContextState::Staging
                } else {
                    ContextState::Live
                },
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(fork_kind),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
            };
            let default_ws =
                match db.get_or_create_default_workspace(caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork: {e}")),
                };
            // Context row + shell/env/binding copy land in one transaction, so
            // a failure can't strand a committed-but-misconfigured context.
            if let Err(e) = db.insert_forked_context(&row, default_ws, source_id) {
                return KjResult::Err(format!("kj fork: {e}"));
            }

            // Apply --pwd override
            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj fork: failed to set --pwd: {e}"));
                }
            }

            // Structural edge: source → new
            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                return KjResult::Err(format!("kj fork: failed to insert structural edge: {e}"));
            }

            // Carry the hydration policy when its marker survived (3d). The
            // child row exists now, so the `context_hydration` FK is satisfied.
            if policy_travels
                && let (Some((_, window)), Some(cm)) = (&parent_policy, &child_marker)
                && let Err(e) = db.set_hydration_policy(new_id, *cm, *window)
            {
                return KjResult::Err(format!("kj fork: failed to carry hydration policy: {e}"));
            }
        }

        // Register in DriftRouter (inherits parent's model)
        {
            let mut drift = self.drift_router().write();
            if let Err(e) =
                drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id)
            {
                return KjResult::Err(format!("kj fork: parent context not in router: {e}"));
            }
            // Set staging state if --stage flag was given
            if staging
                && let Err(e) = drift.set_state(new_id, ContextState::Staging)
            {
                return KjResult::Err(format!("kj fork: failed to set staging state: {e}"));
            }
            // If --model was explicit, override the inherited model
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!(
                                "kj fork: failed to configure model: {e}"
                            ));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork: --model resolved without both provider and model".to_string(),
                        );
                    }
                }
            }
        }

        // Apply preset if requested
        if let Some(ref preset) = preset_label
            && let Err(e) = self.apply_preset(new_id, preset).await
        {
            return KjResult::Err(format!("kj fork: failed to apply preset '{preset}': {e}"));
        }

        // If --prompt given, inject a Drift block
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_id, source_id, note)
        {
            return KjResult::Err(format!("kj fork: failed to inject fork note: {e}"));
        }

        self.apply_fork_mcp_exclusions(new_id).await;

        // Fork marker: get source label + block count for the summary
        let source_label = {
            let db = self.kernel_db().lock();
            db.get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
        };
        let block_count = self
            .block_store()
            .block_snapshots(new_id)
            .map(|b| b.len())
            .unwrap_or(0);
        // When the parent had a policy but the marker fell outside the
        // selection, the drop is visible in the marker (not silent) — softer
        // than the hydrate-side fail-loud, because a fresh fork's rc lifecycle
        // re-marks downstream.
        let policy_note = if parent_policy.is_some() && !policy_travels {
            Some("hydration policy not carried (marker fell outside the selection)")
        } else {
            None
        };
        if let Err(e) = self.inject_fork_marker(
            new_id,
            source_id,
            fork_kind,
            block_count,
            source_label.as_deref(),
            staging,
            policy_note,
        ) {
            tracing::warn!("kj fork: failed to inject fork marker: {e}");
        }

        // Inherit parent's context_type so the new context's fork-side
        // rc scripts dispatch correctly. Done post-commit because the
        // original ContextRow construction defaulted to "default".
        inherit_parent_context_type(self, new_id, source_id);

        // Run rc fork-lifecycle scripts. Failures surface as Error
        // blocks in the new context — they don't abort the fork.
        if let Err(e) = self
            .run_rc_lifecycle("fork", new_id, Some(source_id), Some(fork_kind), None, caller)
            .await
        {
            tracing::warn!("rc fork lifecycle: {e}");
        }

        let switch = args.switch;
        self.request_child_turn(new_id, prompt.as_deref(), staging, caller);
        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        let message = format!("forked to '{}' ({})", display, short);
        self.fork_outcome(new_id, label.as_deref(), switch, message)
    }

    async fn fork_compact(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let label = args.name.clone();
        let prompt = args.prompt.clone();
        let pwd_override = args.pwd.clone();
        let staging = args.stage;
        // M5-F5: optional cheaper model for the distillation step.
        // Distillation is a one-shot summary — using Opus to summarize for
        // a Haiku follow-up is wasteful. Fall through to the source
        // context's chat model when not specified.
        let distill_model = args.distill_model.clone();

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Reject a taken label up front — BEFORE the (slow, billed) distill and
        // before any document is created — so a conflict can't strand an orphan
        // distilled document and the caller gets an actionable message instead
        // of a bare unique-constraint bounce after the summary was already paid
        // for.
        if let Err(e) = self.ensure_label_available(label.as_deref()) {
            return KjResult::Err(format!("kj fork --compact: {e}"));
        }

        let new_id = ContextId::new();

        // Validate --model BEFORE any mutations
        let resolved = match self.resolve_fork_model(args.model.as_deref(), source_id).await {
            Ok(r) => r,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Summarize source context via LLM (use --distill-model when set).
        let summary = match self
            .summarize_with_model(source_id, None, distill_model.as_deref())
            .await
        {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
        };

        // Create empty document for the new context
        if let Err(e) =
            self.block_store()
                .create_document(new_id, crate::DocumentKind::Conversation, None)
        {
            return KjResult::Err(format!("kj fork --compact: failed to create document: {e}"));
        }

        // Seed with distilled summary as a Drift block
        {
            let source_model = {
                let router = self.drift_router().read();
                router.get(source_id).and_then(|h| h.model.clone())
            };
            if let Err(e) = self.block_store().insert_drift_block(
                new_id,
                None,
                None,
                &summary,
                source_id,
                source_model,
                kaijutsu_crdt::DriftKind::Distill,
            ) {
                return KjResult::Err(format!("kj fork --compact: failed to insert summary: {e}"));
            }
        }

        // If --prompt given, inject a fork note after the summary
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_id, source_id, note)
        {
            tracing::warn!("failed to inject fork note: {e}");
        }

        // Write-through: KernelDb then DriftRouter
        {
            let mut db = self.kernel_db().lock();

            let source_ws = db
                .get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.workspace_id);

            let row = ContextRow {
                context_id: new_id,
                                label: label.clone(),
                provider: resolved.provider.clone(),
                model: resolved.model.clone(),
                system_prompt: None,
                consent_mode: ConsentMode::Collaborative,
                context_state: if staging { ContextState::Staging } else { ContextState::Live },
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Compact),
                archived_at: None,
                workspace_id: source_ws,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
            };
            let default_ws =
                match db.get_or_create_default_workspace(caller.principal_id) {
                    Ok(id) => id,
                    Err(e) => return KjResult::Err(format!("kj fork --compact: {e}")),
                };
            // Context row + shell/env/binding copy land in one transaction, so
            // a failure can't strand a committed-but-misconfigured context.
            if let Err(e) = db.insert_forked_context(&row, default_ws, source_id) {
                return KjResult::Err(format!("kj fork --compact: {e}"));
            }

            if let Some(ref pwd) = pwd_override {
                let shell = ContextShellRow {
                    context_id: new_id,
                    cwd: Some(pwd.clone()),
                    updated_at: kaijutsu_types::now_millis() as i64,
                };
                if let Err(e) = db.upsert_context_shell(&shell) {
                    return KjResult::Err(format!("kj fork --compact: failed to set --pwd: {e}"));
                }
            }

            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                return KjResult::Err(format!(
                    "kj fork --compact: failed to insert structural edge: {e}"
                ));
            }
        }

        {
            let mut drift = self.drift_router().write();
            if let Err(e) =
                drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id)
            {
                return KjResult::Err(format!(
                    "kj fork --compact: parent context not in router: {e}"
                ));
            }
            if staging
                && let Err(e) = drift.set_state(new_id, ContextState::Staging)
            {
                return KjResult::Err(format!("kj fork --compact: failed to set staging state: {e}"));
            }
            if resolved.explicit {
                match (&resolved.provider, &resolved.model) {
                    (Some(p), Some(m)) => {
                        if let Err(e) = drift.configure_llm(new_id, p, m) {
                            return KjResult::Err(format!(
                                "kj fork --compact: failed to configure model: {e}"
                            ));
                        }
                    }
                    _ => {
                        return KjResult::Err(
                            "kj fork --compact: --model resolved without both provider and model"
                                .to_string(),
                        );
                    }
                }
            }
        }

        self.apply_fork_mcp_exclusions(new_id).await;

        let source_label = {
            let db = self.kernel_db().lock();
            db.get_context(source_id)
                .ok()
                .flatten()
                .and_then(|r| r.label)
        };
        let block_count = self
            .block_store()
            .block_snapshots(new_id)
            .map(|b| b.len())
            .unwrap_or(0);
        if let Err(e) = self.inject_fork_marker(
            new_id,
            source_id,
            ForkKind::Compact,
            block_count,
            source_label.as_deref(),
            staging,
            None,
        ) {
            tracing::warn!("kj fork --compact: failed to inject fork marker: {e}");
        }

        inherit_parent_context_type(self, new_id, source_id);
        if let Err(e) = self
            .run_rc_lifecycle(
                "fork",
                new_id,
                Some(source_id),
                Some(ForkKind::Compact),
                None,
                caller,
            )
            .await
        {
            tracing::warn!("rc fork lifecycle (compact): {e}");
        }

        // POSIX-style: drive the child's autonomous turn (if --prompt) after all
        // fork-time block injections + rc lifecycle, then honor stay-on-parent
        // default / --switch via fork_outcome.
        let switch = args.switch;
        self.request_child_turn(new_id, prompt.as_deref(), staging, caller);
        let short = new_id.short();
        let display = label.as_deref().unwrap_or(&short);
        let message = format!("compact-forked to '{}' ({})", display, new_id.short());
        self.fork_outcome(new_id, label.as_deref(), switch, message)
    }

    async fn fork_subtree(&self, args: &ForkArgs, caller: &KjCaller) -> KjResult {
        let template_ref = match args.as_template.clone() {
            Some(r) => r,
            None => {
                return KjResult::Err(
                    "kj fork --as: requires a template context reference".to_string(),
                );
            }
        };
        let name = match args.name.clone() {
            Some(n) => n,
            None => {
                return KjResult::Err(
                    "kj fork --as: requires --name for the new subtree".to_string(),
                );
            }
        };
        let staging = args.stage;
        let prompt = args.prompt.clone();

        let source_id = match caller.require_context() {
            Ok(id) => id,
            Err(e) => return e,
        };


        // Resolve template root
        let template_root_id = {
            let db = self.kernel_db().lock();
            match db.resolve_context(&template_ref) {
                Ok(id) => id,
                Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
            }
        };

        // Get the template subtree shape
        let template_nodes = {
            let db = self.kernel_db().lock();
            match db.subtree_snapshot(template_root_id) {
                Ok(nodes) => nodes,
                Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
            }
        };

        if template_nodes.is_empty() {
            return KjResult::Err("kj fork --as: template context not found".to_string());
        }

        // Validate all template node providers BEFORE any mutations
        {
            let registry = self.kernel().llm().read().await;
            for (row, _depth) in &template_nodes {
                if let Some(ref p) = row.provider
                    && registry.get(p).is_none()
                {
                    return KjResult::Err(format!(
                        "kj fork --as: template node '{}' references unknown provider '{}'",
                        row.label.as_deref().unwrap_or("(unnamed)"),
                        p,
                    ));
                }
            }
        }

        // Build ID mapping: old → new
        let mut id_map: HashMap<ContextId, ContextId> = HashMap::new();
        for (row, _depth) in &template_nodes {
            id_map.insert(row.context_id, ContextId::new());
        }

        let new_root_id = id_map[&template_root_id];

        // Create new contexts (BFS order — template_nodes is already ordered by depth)
        {
            let mut db = self.kernel_db().lock();

            for (row, _depth) in &template_nodes {
                let new_id = id_map[&row.context_id];
                let is_root = row.context_id == template_root_id;

                let new_label = if is_root {
                    Some(name.clone())
                } else {
                    row.label.as_ref().map(|l| format!("{name}/{l}"))
                };

                // Map forked_from to the new parent (if it's in the subtree),
                // otherwise point to caller's context
                let new_forked_from = row
                    .forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(caller.context_id);

                let new_row = ContextRow {
                    context_id: new_id,
                                        label: new_label,
                    provider: row.provider.clone(),
                    model: row.model.clone(),
                    system_prompt: row.system_prompt.clone(),
                    consent_mode: row.consent_mode,
                    context_state: if staging { ContextState::Staging } else { ContextState::Live },
                    context_type: "default".to_string(),
                    created_at: kaijutsu_types::now_millis() as i64,
                    created_by: caller.principal_id,
                    forked_from: new_forked_from,
                    fork_kind: Some(ForkKind::Subtree),
                    archived_at: None,
                    workspace_id: row.workspace_id,
                    preset_id: row.preset_id,
                    // A fresh fork is live, never inherits concluded status.
                    concluded_at: None,
                    last_activity_at: None,
                    promoted_at: None,
                    demoted_at: None,
                    paused_at: None,
                };
                let default_ws =
                    match db.get_or_create_default_workspace(caller.principal_id) {
                        Ok(id) => id,
                        Err(e) => return KjResult::Err(format!("kj fork --as: {e}")),
                    };
                // Context row + shell/env/binding copy (from the template
                // context) land in one transaction, so a failure can't strand a
                // committed-but-misconfigured context.
                if let Err(e) = db.insert_forked_context(&new_row, default_ws, row.context_id) {
                    return KjResult::Err(format!("kj fork --as: failed to create context: {e}"));
                }

                // Create empty document for each new context
                if let Err(e) = self.block_store().create_document(
                    new_id,
                    crate::DocumentKind::Conversation,
                    None,
                ) {
                    return KjResult::Err(format!("kj fork --as: failed to create document: {e}"));
                }
            }

            // Insert structural edges mirroring the template
            for (row, _depth) in &template_nodes {
                let old_parent = row.context_id;
                let new_parent = id_map[&old_parent];

                // Get template's structural children
                let children = match db.structural_children(old_parent) {
                    Ok(c) => c,
                    Err(e) => {
                        return KjResult::Err(format!(
                            "kj fork --as: failed to read template edges: {e}"
                        ));
                    }
                };
                for child in children {
                    if let Some(&new_child) = id_map.get(&child.context_id) {
                        let edge = ContextEdgeRow {
                            edge_id: uuid::Uuid::now_v7(),
                            source_id: new_parent,
                            target_id: new_child,
                            kind: EdgeKind::Structural,
                            metadata: None,
                            created_at: kaijutsu_types::now_millis() as i64,
                        };
                        if let Err(e) = db.insert_edge(&edge) {
                            return KjResult::Err(format!(
                                "kj fork --as: failed to insert subtree edge: {e}"
                            ));
                        }
                    }
                }
            }

            // Edge from caller's context to the new root
            let root_edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_root_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&root_edge) {
                return KjResult::Err(format!("kj fork --as: failed to insert root edge: {e}"));
            }
        }

        // Register all new contexts in DriftRouter
        {
            let mut drift = self.drift_router().write();
            for (row, _depth) in &template_nodes {
                let new_id = id_map[&row.context_id];
                let is_root = row.context_id == template_root_id;
                let label = if is_root {
                    Some(name.as_str())
                } else {
                    row.label.as_deref()
                };
                let forked_from = row
                    .forked_from
                    .and_then(|fid| id_map.get(&fid).copied())
                    .or(caller.context_id);
                if let Some(parent) = forked_from {
                    if let Err(e) = drift.register_fork(new_id, label, parent, caller.principal_id)
                    {
                        return KjResult::Err(format!(
                            "kj fork --as: parent context not in router: {e}"
                        ));
                    }
                } else if let Err(e) = drift.register(new_id, label, None, caller.principal_id) {
                    return KjResult::Err(format!("kj fork --as: {e}"));
                }
                if staging
                    && let Err(e) = drift.set_state(new_id, ContextState::Staging)
                {
                    return KjResult::Err(format!("kj fork --as: failed to set staging state: {e}"));
                }
            }
        }

        self.apply_fork_mcp_exclusions(new_root_id).await;

        // If --prompt given, inject the fork note on the subtree root before the
        // fork marker — matching fork_full's placement so the autonomous turn's
        // anchor lands at the true tail.
        if let Some(note) = &prompt
            && let Err(e) = self.inject_fork_note(new_root_id, source_id, note)
        {
            return KjResult::Err(format!("kj fork --as: failed to inject fork note: {e}"));
        }

        if let Err(e) = self.inject_fork_marker(
            new_root_id,
            source_id,
            ForkKind::Subtree,
            template_nodes.len(),
            Some(&template_ref),
            staging,
            None,
        ) {
            tracing::warn!("kj fork --as: failed to inject fork marker: {e}");
        }

        inherit_parent_context_type(self, new_root_id, source_id);
        if let Err(e) = self
            .run_rc_lifecycle(
                "fork",
                new_root_id,
                Some(source_id),
                Some(ForkKind::Subtree),
                None,
                caller,
            )
            .await
        {
            tracing::warn!("rc fork lifecycle (subtree): {e}");
        }

        // POSIX-style: the prompt/turn targets the subtree root. Drive it after
        // all fork-time block injections + rc lifecycle, then honor the
        // stay-on-parent default / --switch via fork_outcome.
        let switch = args.switch;
        self.request_child_turn(new_root_id, prompt.as_deref(), staging, caller);
        let message = format!(
            "subtree-forked '{}' ({} contexts) from template '{}'",
            name,
            template_nodes.len(),
            template_ref
        );
        self.fork_outcome(new_root_id, Some(name.as_str()), switch, message)
    }

    /// Apply a preset's settings to a context (post-fork).
    async fn apply_preset(&self, context_id: ContextId, preset_label: &str) -> Result<(), String> {
        let preset = {
            let db = self.kernel_db().lock();
            db.get_preset_by_label(preset_label)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("preset '{}' not found", preset_label))?
        };

        // Update DB
        {
            let db = self.kernel_db().lock();
            if preset.provider.is_some() || preset.model.is_some() {
                db.update_model(
                    context_id,
                    preset.provider.as_deref(),
                    preset.model.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            }
            db.update_settings(
                context_id,
                preset.system_prompt.as_deref(),
                preset.consent_mode,
            )
            .map_err(|e| e.to_string())?;
        }

        // Update DriftRouter
        {
            let mut drift = self.drift_router().write();
            if let (Some(p), Some(m)) = (&preset.provider, &preset.model) {
                let _ = drift.configure_llm(context_id, p, m);
            }
        }

        Ok(())
    }

    /// Build the terminal result for a completed fork.
    ///
    /// POSIX semantics: by default the caller stays on the parent and keeps
    /// running — the child id is returned in `data` so `for x in $(kj fork …)`
    /// and `kaish-last` can pick it up. `--switch` opts into moving the caller
    /// into the child (the old unconditional behaviour).
    fn fork_outcome(
        &self,
        new_id: ContextId,
        label: Option<&str>,
        switch: bool,
        message: String,
    ) -> KjResult {
        if switch {
            KjResult::Switch(new_id, message)
        } else {
            KjResult::Ok {
                message,
                content_type: ContentType::Plain,
                ephemeral: false,
                data: Some(serde_json::json!({
                    "context_id": new_id.to_hex(),
                    "label": label,
                })),
            }
        }
    }

    /// Publish a single `TurnFlow::Requested` and return how many subscribers
    /// received it (the turn-driver count). This is the one shared bridge from
    /// kernel-side commands (`kj fork --prompt`, `kj drive`) to the server's
    /// turn driver — the kernel can't call the server directly, so it clocks a
    /// turn by publishing on the FlowBus. A `delivered == 0` return means no
    /// driver is listening; callers decide how to surface that (fork writes an
    /// Error block; `kj drive` returns an error to the user directly).
    pub(crate) fn publish_turn_request(
        &self,
        context_id: ContextId,
        after_block_id: kaijutsu_types::BlockId,
        content: &str,
        principal_id: kaijutsu_types::PrincipalId,
    ) -> usize {
        self.kernel()
            .turn_flows()
            .publish(crate::flows::TurnFlow::Requested {
                context_id,
                after_block_id,
                content: content.to_string(),
                principal_id,
                model: None,
            })
    }

    /// Ask the server to drive one autonomous turn in the freshly forked child,
    /// so a `kj fork --prompt "…"` child starts acting immediately while the
    /// parent's fork call returns and keeps running (POSIX fork()).
    ///
    /// No-op when there's no seed (a bare fork is an inert snapshot) or when the
    /// child is staged (it's awaiting human curation). The seed already lives in
    /// the child's block log as the fork note, so this only publishes the
    /// request — it does not re-insert the seed. Must run after all fork-time
    /// block injections so `after_block_id` anchors at the true tail.
    fn request_child_turn(
        &self,
        new_id: ContextId,
        prompt: Option<&str>,
        staging: bool,
        caller: &KjCaller,
    ) {
        let Some(note) = prompt else { return };
        if staging {
            return;
        }
        let Some(after) = self.block_store().last_block_id(new_id) else {
            tracing::warn!(
                context_id = %new_id,
                "kj fork --prompt: child has no blocks to anchor an autonomous turn"
            );
            return;
        };
        let delivered =
            self.publish_turn_request(new_id, after, note, caller.principal_id);

        // Zero subscribers means no turn driver is listening — the autonomous
        // turn was requested but will never run. Don't silently no-op: warn and
        // surface a visible Error block in the child (same API rc lifecycle uses)
        // so the inert child is explained rather than mysterious.
        if delivered == 0 {
            tracing::warn!(
                context_id = %new_id,
                "kj fork --prompt: no turn driver subscribed; autonomous turn will not run"
            );
            let summary = "kj fork --prompt: no turn driver is active, so the requested \
                           autonomous turn will not run. This child was seeded but will \
                           stay idle until a turn is driven."
                .to_string();
            // Same BlockKind::Error / insert_block_as idiom rc lifecycle uses
            // (see kj/lifecycle.rs insert_rc_failure_block): a plain Error block
            // anchored at the tail, no structured ErrorPayload parent required.
            let after = self.block_store().last_block_id(new_id);
            if let Err(insert_err) = self.block_store().insert_block_as(
                new_id,
                None,
                after.as_ref(),
                kaijutsu_crdt::Role::System,
                kaijutsu_crdt::BlockKind::Error,
                summary,
                kaijutsu_crdt::Status::Error,
                kaijutsu_crdt::ContentType::Plain,
                Some(caller.principal_id),
            ) {
                tracing::warn!(
                    context_id = %new_id,
                    "kj fork --prompt: failed to insert no-driver error block: {insert_err}"
                );
            }
        }
    }

    fn inject_fork_note(
        &self,
        target_id: ContextId,
        source_id: ContextId,
        note: &str,
    ) -> Result<(), String> {
        use kaijutsu_crdt::DriftKind;

        let after = self.block_store().last_block_id(target_id);
        self.block_store()
            .insert_drift_block(
                target_id,
                None,
                after.as_ref(),
                note,
                source_id,
                None,
                DriftKind::Push,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Insert an ephemeral fork marker block at the end of the forked document.
    ///
    /// The marker summarizes the fork operation (source, kind, block count) and is
    /// excluded from LLM hydration so it doesn't waste model context.
    #[allow(clippy::too_many_arguments)] // a marker is summarized from many facets
    fn inject_fork_marker(
        &self,
        target_id: ContextId,
        source_id: ContextId,
        fork_kind: ForkKind,
        block_count: usize,
        source_label: Option<&str>,
        staging: bool,
        // A visible note appended to the marker — e.g. a dropped hydration
        // policy (3d). `None` for the plain marker.
        note: Option<&str>,
    ) -> Result<(), String> {
        use kaijutsu_crdt::DriftKind;

        let source_short = source_id.short();
        let source_display = source_label.unwrap_or(&source_short);
        let mut content = format!(
            "forked from '{}' ({}) — {} copy, {} blocks",
            source_display,
            source_short,
            fork_kind.as_str(),
            block_count,
        );
        if let Some(note) = note {
            content.push_str(" — ");
            content.push_str(note);
        }

        let after = self.block_store().last_block_id(target_id);
        let block_id = self
            .block_store()
            .insert_drift_block(
                target_id,
                None,
                after.as_ref(),
                &content,
                source_id,
                None,
                DriftKind::Fork,
            )
            .map_err(|e| e.to_string())?;

        self.block_store()
            .set_ephemeral(target_id, &block_id, true)
            .map_err(|e| e.to_string())?;

        // In staging mode, fork marker starts excluded (user opts in)
        if staging {
            self.block_store()
                .set_excluded(target_id, &block_id, true)
                .map_err(|e| e.to_string())?;
        }

        Ok(())
    }

}

/// Copy the parent's `context_type` onto the freshly-forked child so the
/// child's fork-side rc lifecycle dispatches against the parent's type.
/// All four fork variants commit their child with `context_type='default'`
/// at insert time, so this is a post-commit fixup.
///
/// On any error (parent missing, update fails) we leave the child as
/// 'default' and log — failure here would corrupt fewer guarantees than
/// aborting a successful fork.
fn inherit_parent_context_type(
    dispatcher: &KjDispatcher,
    child_id: ContextId,
    parent_id: ContextId,
) {
    let parent_type = {
        let db = dispatcher.kernel_db().lock();
        match db.get_context(parent_id) {
            Ok(Some(row)) => row.context_type,
            Ok(None) => {
                tracing::warn!(
                    "rc fork: parent context {} not found; child {} stays 'default'",
                    parent_id.short(),
                    child_id.short()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("rc fork: cannot read parent context_type: {e}");
                return;
            }
        }
    };
    if parent_type == "default" {
        return; // already the default
    }
    let db = dispatcher.kernel_db().lock();
    if let Err(e) = db.update_context_type(child_id, &parent_type) {
        tracing::warn!(
            "rc fork: failed to set context_type='{}' on child {}: {e}",
            parent_type,
            child_id.short()
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ForkKind, PrincipalId};

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn fork_basic() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);

        // Create a document for the source context
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("branch")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            result.message().contains("branch"),
            "msg: {}",
            result.message()
        );

        // Verify new context exists in DB
        let db = d.kernel_db().lock();
        let contexts = db.list_active_contexts().unwrap();
        assert!(
            contexts
                .iter()
                .any(|r| r.label.as_deref() == Some("branch"))
        );
    }

    #[tokio::test]
    async fn fork_model_bare_alias_resolves_to_provider() {
        // Regression: `kj fork --model <alias>` must resolve the alias to its
        // real provider, not silently pin the literal alias on the default
        // provider — the old fork bug where `deepseek-lite` landed on anthropic
        // and only failed at the first turn.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // anthropic registered first → default provider; deepseek + alias added.
        {
            use crate::llm::{MockClient, ModelAlias, Provider};
            use std::collections::HashMap;
            use std::sync::Arc;
            let mut reg = d.kernel().llm().write().await;
            reg.register("anthropic", Arc::new(Provider::Mock(MockClient::new("a"))));
            reg.register("deepseek", Arc::new(Provider::Mock(MockClient::new("d"))));
            let mut aliases = HashMap::new();
            aliases.insert(
                s("deepseek-lite"),
                ModelAlias {
                    provider: s("deepseek"),
                    model: s("deepseek-v4-flash"),
                },
            );
            reg.set_model_aliases(aliases);
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--model"),
                    s("deepseek-lite"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // The child must route to deepseek (the alias target), not the default.
        let child = {
            let db = d.kernel_db().lock();
            db.list_active_contexts()
                .unwrap()
                .into_iter()
                .find(|r| r.label.as_deref() == Some("child"))
                .expect("child context exists")
                .context_id
        };
        let router = d.drift_router().read();
        let handle = router.get(child).expect("child has a drift handle");
        assert_eq!(handle.provider.as_deref(), Some("deepseek"));
        assert_eq!(handle.model.as_deref(), Some("deepseek-v4-flash"));
    }

    #[tokio::test]
    async fn fork_model_colon_footgun_errors() {
        // The `provider:model` colon form fails loud on the fork path too, not
        // just `kj context set` — both share one resolver.
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        {
            use crate::llm::{MockClient, Provider};
            use std::sync::Arc;
            let mut reg = d.kernel().llm().write().await;
            reg.register("deepseek", Arc::new(Provider::Mock(MockClient::new("d"))));
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--model"),
                    s("deepseek:deepseek-v4-flash"),
                ],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "colon form should fail: {}",
            result.message()
        );
        assert!(
            result.message().contains("provider:model"),
            "expected slash hint, got: {}",
            result.message()
        );
    }

    /// Insert a Text block into `ctx` and return its id — for exercising
    /// `--exclude` against a known block.
    fn insert_text(
        d: &crate::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
        principal: PrincipalId,
        body: &str,
    ) -> kaijutsu_crdt::BlockId {
        d.block_store()
            .insert_block_as(
                ctx,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                body.to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap()
    }

    /// Full fork's power path: `--exclude <block>` drops that block from the
    /// child (the orchestrator-repair case — "fork X without the huge block that
    /// blew it up") while copying everything else. Today full fork copies
    /// everything; this wires the existing ForkBlockFilter onto it.
    #[tokio::test]
    async fn fork_exclude_drops_named_block_keeps_rest() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "keep1");
        let drop_id = insert_text(&d, source, principal, "DROPME");
        insert_text(&d, source, principal, "keep2");

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--name"), s("repaired"), s("--exclude"), s(&drop_id.to_key())],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork --exclude failed: {}", result.message());

        let child = d
            .kernel_db()
            .lock()
            .find_context_by_label("repaired")
            .unwrap()
            .unwrap()
            .context_id;
        let contents: Vec<String> = d
            .block_store()
            .block_snapshots(child)
            .unwrap()
            .iter()
            .map(|b| b.content.clone())
            .collect();
        assert!(contents.iter().any(|c| c.contains("keep1")), "kept blocks: {contents:?}");
        assert!(contents.iter().any(|c| c.contains("keep2")), "kept blocks: {contents:?}");
        assert!(
            !contents.iter().any(|c| c.contains("DROPME")),
            "the excluded block must not be copied into the child: {contents:?}"
        );
    }

    /// Fail-loud (consistent with `kj context hydrate --mark`): a `--exclude`
    /// block id that doesn't exist in the source is a typo, not a silent no-op
    /// (which would leave the offending block in the repaired child).
    #[tokio::test]
    async fn fork_exclude_rejects_block_not_in_source() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "real");
        let phantom = kaijutsu_crdt::BlockId::new(source, PrincipalId::new(), 9999).to_key();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--exclude"), s(&phantom)], &c)
            .await;
        assert!(!result.is_ok(), "a --exclude block not in the source must error");
        assert!(
            result.message().contains("not in") || result.message().contains("not found"),
            "msg: {}",
            result.message()
        );
    }

    // ── slice 3c: preset recall + range composition at fork ──────────────

    /// Seed the factory presets (full/window/spawn) — production does this at
    /// rpc init; the test dispatcher doesn't, so recall tests do it explicitly.
    fn seed_factory_presets(d: &crate::KjDispatcher) {
        let mut db = d.kernel_db().lock();
        crate::seed_presets::ensure_factory_presets(&mut db, PrincipalId::system()).unwrap();
    }

    /// Ordered conversation contents of a context (document order).
    fn ordered_contents(d: &crate::KjDispatcher, ctx: kaijutsu_types::ContextId) -> Vec<String> {
        d.block_store()
            .block_snapshots(ctx)
            .unwrap()
            .iter()
            .map(|b| b.content.clone())
            .collect()
    }

    fn child_id(d: &crate::KjDispatcher, label: &str) -> kaijutsu_types::ContextId {
        d.kernel_db()
            .lock()
            .find_context_by_label(label)
            .unwrap()
            .unwrap()
            .context_id
    }

    /// A source context with five distinct, position-tagged text blocks.
    async fn source_with_five(
        d: &crate::KjDispatcher,
        principal: PrincipalId,
    ) -> (kaijutsu_types::ContextId, Vec<String>) {
        let source = register_context(d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        for body in ["alpha", "bravo", "charlie", "delta", "echo"] {
            insert_text(d, source, principal, body);
        }
        let pc = ordered_contents(d, source);
        assert_eq!(pc.len(), 5, "source ordering: {pc:?}");
        (source, pc)
    }

    fn fork_kind_of(d: &crate::KjDispatcher, ctx: kaijutsu_types::ContextId) -> Option<ForkKind> {
        d.kernel_db().lock().get_context(ctx).unwrap().unwrap().fork_kind
    }

    /// `--preset spawn` copies ~nothing: the player-birth shape. None of the
    /// parent's conversation blocks reach the child; the fork is `Filtered`.
    #[tokio::test]
    async fn fork_preset_spawn_copies_no_parent_blocks() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("born"), s("--preset"), s("spawn")], &c)
            .await;
        assert!(result.is_ok(), "spawn fork failed: {}", result.message());

        let child = child_id(&d, "born");
        let kid = ordered_contents(&d, child);
        for body in &pc {
            assert!(
                !kid.iter().any(|c| c.contains(body.as_str())),
                "spawn must copy no parent block; found {body:?} in {kid:?}"
            );
        }
        assert_eq!(fork_kind_of(&d, child), Some(ForkKind::Filtered));
    }

    /// `--preset full` is the all-pass base: every parent block survives, and
    /// the fork stays `Full` (the history-preserving plain copy).
    #[tokio::test]
    async fn fork_preset_full_copies_everything() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("whole"), s("--preset"), s("full")], &c)
            .await;
        assert!(result.is_ok(), "full fork failed: {}", result.message());

        let child = child_id(&d, "whole");
        let kid = ordered_contents(&d, child);
        for body in &pc {
            assert!(kid.iter().any(|c| c.contains(body.as_str())), "missing {body:?} in {kid:?}");
        }
        assert_eq!(fork_kind_of(&d, child), Some(ForkKind::Full));
    }

    /// `--include 0:2` narrows to the first two positions; the rest are dropped.
    #[tokio::test]
    async fn fork_include_range_keeps_only_that_window() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("head"), s("--include"), s("0:2")], &c)
            .await;
        assert!(result.is_ok(), "include fork failed: {}", result.message());

        let child = child_id(&d, "head");
        let kid = ordered_contents(&d, child);
        assert!(kid.iter().any(|c| c.contains(&pc[0])), "kept pos0 {:?}: {kid:?}", pc[0]);
        assert!(kid.iter().any(|c| c.contains(&pc[1])), "kept pos1 {:?}: {kid:?}", pc[1]);
        for body in &pc[2..] {
            assert!(!kid.iter().any(|c| c.contains(body.as_str())), "dropped {body:?}: {kid:?}");
        }
        assert_eq!(fork_kind_of(&d, child), Some(ForkKind::Filtered));
    }

    /// `--exclude 1:4` carves a middle notch: positions 0 and 4 survive.
    #[tokio::test]
    async fn fork_exclude_range_carves_middle() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("notched"), s("--exclude"), s("1:4")], &c)
            .await;
        assert!(result.is_ok(), "exclude-range fork failed: {}", result.message());

        let child = child_id(&d, "notched");
        let kid = ordered_contents(&d, child);
        assert!(kid.iter().any(|c| c.contains(&pc[0])), "kept pos0: {kid:?}");
        assert!(kid.iter().any(|c| c.contains(&pc[4])), "kept pos4: {kid:?}");
        for body in &pc[1..4] {
            assert!(!kid.iter().any(|c| c.contains(body.as_str())), "dropped {body:?}: {kid:?}");
        }
        assert_eq!(fork_kind_of(&d, child), Some(ForkKind::Filtered));
    }

    /// The loud include invariant: `--include 0:3 --exclude 1:2` contradict on
    /// one line — no silent excludes-win, the fork refuses naming the positions.
    #[tokio::test]
    async fn fork_include_exclude_contradiction_is_loud() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--include"), s("0:3"), s("--exclude"), s("1:2")],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "contradiction must error");
        assert!(
            result.message().contains("conflicts") && result.message().contains("1:2"),
            "should name the offending positions: {}",
            result.message()
        );
    }

    /// The include invariant covers block-key excludes too: an exact
    /// `--exclude <key>` landing inside an explicit `--include` range must
    /// refuse loud (no silent winner), even though block-key drops are a
    /// predicate applied during the copy, not a positional subtraction.
    #[tokio::test]
    async fn fork_include_with_blockkey_exclude_inside_is_loud() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;
        // The block at position 2 is inside --include 0:4; excluding it by key
        // must contradict the include.
        let key2 = d.block_store().block_snapshots(source).unwrap()[2].id.to_key();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--include"), s("0:4"), s("--exclude"), s(&key2)],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "block-key exclude inside an include must error");
        assert!(
            result.message().contains("conflicts") && result.message().contains("position 2"),
            "should name the clobbered block/position: {}",
            result.message()
        );
    }

    /// A block-key exclude OUTSIDE the include range is fine (the repair case
    /// still composes with a narrowing include).
    #[tokio::test]
    async fn fork_include_with_blockkey_exclude_outside_is_ok() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;
        let key4 = d.block_store().block_snapshots(source).unwrap()[4].id.to_key();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--name"), s("ok"), s("--include"), s("0:3"), s("--exclude"), s(&key4)],
                &c,
            )
            .await;
        assert!(result.is_ok(), "exclude outside the include must compose: {}", result.message());
        let kid = ordered_contents(&d, child_id(&d, "ok"));
        for body in &pc[0..3] {
            assert!(kid.iter().any(|c| c.contains(body.as_str())), "kept {body:?}: {kid:?}");
        }
    }

    /// `--preset window` reads the parent's hydration policy row; absent = a
    /// configuration mistake, loud per docs/fork-filters.md (not a degenerate
    /// full copy).
    #[tokio::test]
    async fn fork_preset_window_without_policy_errors() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--preset"), s("window")], &c)
            .await;
        assert!(!result.is_ok(), "window with no policy must error");
        assert!(
            result.message().contains("hydration policy"),
            "msg should explain the missing policy: {}",
            result.message()
        );
    }

    /// `--preset window` with a policy in place recalls the `[0,marker] ∪ tail`
    /// shape — the prefix and tail survive, the middle is notched out.
    #[tokio::test]
    async fn fork_preset_window_recalls_prefix_and_tail() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, pc) = source_with_five(&d, principal).await;

        // Mark position 0 as the pinned prefix end, window=1 → keep [0,0] ∪ last1
        // = positions 0 and 4; 1..4 notched.
        let marker = d.block_store().block_snapshots(source).unwrap()[0].id;
        d.kernel_db().lock().set_hydration_policy(source, marker, 1).unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("win"), s("--preset"), s("window")], &c)
            .await;
        assert!(result.is_ok(), "window fork failed: {}", result.message());

        let child = child_id(&d, "win");
        let kid = ordered_contents(&d, child);
        assert!(kid.iter().any(|c| c.contains(&pc[0])), "kept prefix pos0: {kid:?}");
        assert!(kid.iter().any(|c| c.contains(&pc[4])), "kept tail pos4: {kid:?}");
        for body in &pc[1..4] {
            assert!(!kid.iter().any(|c| c.contains(body.as_str())), "notched {body:?}: {kid:?}");
        }
        assert_eq!(fork_kind_of(&d, child), Some(ForkKind::Filtered));
    }

    // ── slice 3d: hydration policy travel (marker-survives rule) ─────────

    fn child_policy(
        d: &crate::KjDispatcher,
        ctx: kaijutsu_types::ContextId,
    ) -> Option<(kaijutsu_crdt::BlockId, u32)> {
        d.kernel_db().lock().get_hydration_policy(ctx).unwrap()
    }

    /// A full fork carries the parent's hydration policy: the marker remaps by
    /// `(principal, seq)` onto the child context (resolves the "fork drops the
    /// policy" backlog entry by construction).
    #[tokio::test]
    async fn fork_full_carries_hydration_policy() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;
        let marker = d.block_store().block_snapshots(source).unwrap()[2].id;
        d.kernel_db().lock().set_hydration_policy(source, marker, 3).unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("kid")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let child = child_id(&d, "kid");
        let (cm, w) = child_policy(&d, child).expect("policy carried");
        assert_eq!(w, 3);
        // Marker remapped: same (principal, seq), child's context part.
        assert_eq!(cm.principal_id, marker.principal_id);
        assert_eq!(cm.seq, marker.seq);
        assert_eq!(cm.context_id, child);
        // And the carried marker actually points at a surviving child block.
        assert!(
            d.block_store().get_block_snapshot(child, &cm).unwrap().is_some(),
            "carried marker must resolve in the child"
        );
    }

    /// A `spawn` fork copies nothing, so the marker can't survive: the policy is
    /// dropped (the child rc re-marks) and the fork marker says so visibly.
    #[tokio::test]
    async fn fork_spawn_drops_policy_with_visible_note() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;
        let marker = d.block_store().block_snapshots(source).unwrap()[0].id;
        d.kernel_db().lock().set_hydration_policy(source, marker, 2).unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("born"), s("--preset"), s("spawn")], &c)
            .await;
        assert!(result.is_ok(), "spawn fork failed: {}", result.message());

        let child = child_id(&d, "born");
        assert!(child_policy(&d, child).is_none(), "spawn must not carry the policy");
        let kid = ordered_contents(&d, child);
        assert!(
            kid.iter().any(|c| c.contains("hydration policy not carried")),
            "the drop must be visible in the fork marker: {kid:?}"
        );
    }

    /// A `window` fork keeps the marker (the prefix end) by construction, so the
    /// policy travels — no drop note.
    #[tokio::test]
    async fn fork_window_preset_carries_policy() {
        let d = test_dispatcher().await;
        seed_factory_presets(&d);
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;
        let marker = d.block_store().block_snapshots(source).unwrap()[1].id;
        d.kernel_db().lock().set_hydration_policy(source, marker, 2).unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("win"), s("--preset"), s("window")], &c)
            .await;
        assert!(result.is_ok(), "window fork failed: {}", result.message());

        let child = child_id(&d, "win");
        let (cm, w) = child_policy(&d, child).expect("window carries the policy");
        assert_eq!((cm.principal_id, cm.seq, w), (marker.principal_id, marker.seq, 2));
        let kid = ordered_contents(&d, child);
        assert!(
            !kid.iter().any(|c| c.contains("not carried")),
            "no drop note when the marker survives: {kid:?}"
        );
    }

    /// An ad-hoc range that excludes the marked block drops the policy (visible
    /// note) — the "iff the marker survived" case.
    #[tokio::test]
    async fn fork_adhoc_range_dropping_marker_drops_policy() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let (source, _pc) = source_with_five(&d, principal).await;
        // Mark position 0, then keep only the tail (2:end) — the marker is
        // outside the selection, so the policy must not travel.
        let marker = d.block_store().block_snapshots(source).unwrap()[0].id;
        d.kernel_db().lock().set_hydration_policy(source, marker, 2).unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("tailonly"), s("--include"), s("2:end")], &c)
            .await;
        assert!(result.is_ok(), "ad-hoc fork failed: {}", result.message());

        let child = child_id(&d, "tailonly");
        assert!(child_policy(&d, child).is_none(), "dropped-marker fork must not carry policy");
        let kid = ordered_contents(&d, child);
        assert!(
            kid.iter().any(|c| c.contains("hydration policy not carried")),
            "drop must be visible: {kid:?}"
        );
    }

    #[tokio::test]
    async fn fork_no_name() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(result.message().contains("forked to"));
    }

    #[tokio::test]
    async fn fork_with_prompt() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("noted"),
                    s("--prompt"),
                    s("explore auth bug"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
    }

    #[tokio::test]
    async fn fork_help() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("fork"), s("--help")], &c).await;
        assert!(result.is_ok());
        assert!(
            result.message().contains("Usage") && result.message().contains("--prompt"),
            "clap help should list usage + flags: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn fork_compact_empty_source_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("empty-src"), None, principal);

        // Create empty document
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--compact"), s("--name"), s("compacted")],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "should fail on empty source: {}",
            result.message()
        );
        assert!(
            result.message().contains("no blocks"),
            "msg: {}",
            result.message()
        );
    }

    #[tokio::test]
    async fn fork_inherits_config() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set shell config and env on source
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
            db.set_context_env(source, "RUST_LOG", "debug").unwrap();
            db.set_context_env(source, "EDITOR", "vim").unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Find the new context and verify config was copied
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/project".into()));
        let env = db.get_context_env(child.context_id).unwrap();
        assert_eq!(env.len(), 2);
    }

    #[tokio::test]
    async fn fork_pwd_override() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set cwd on source
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: source,
                cwd: Some("/home/user/project".into()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("research"),
                    s("--pwd"),
                    s("/home/user/src/myproject"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("research")
            .unwrap()
            .unwrap();
        let shell = db.get_context_shell(child.context_id).unwrap().unwrap();
        assert_eq!(shell.cwd, Some("/home/user/src/myproject".into()));
    }

    /// Register a mock LLM provider on the kernel so --model validation passes.
    async fn register_mock_provider(d: &super::super::KjDispatcher) {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;
        let mock = Arc::new(Provider::Mock(MockClient::new("mock response")));
        let mut registry = d.kernel().llm().write().await;
        registry.register("mock", mock);
    }

    /// Configure provider+model on a context in DriftRouter.
    async fn configure_context_model(
        d: &super::super::KjDispatcher,
        id: kaijutsu_types::ContextId,
        provider: &str,
        model: &str,
    ) {
        let mut drift = d.drift_router().write();
        let _ = drift.configure_llm(id, provider, model);
    }

    #[tokio::test]
    async fn fork_inherits_parent_model_in_db() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Set parent's model in DriftRouter
        register_mock_provider(&d).await;
        configure_context_model(&d, source, "mock", "mock-model").await;

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify child inherited provider+model in DB
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        assert_eq!(
            child.provider.as_deref(),
            Some("mock"),
            "child should inherit parent provider"
        );
        assert_eq!(
            child.model.as_deref(),
            Some("mock-model"),
            "child should inherit parent model"
        );
    }

    #[tokio::test]
    async fn fork_model_flag_overrides_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        register_mock_provider(&d).await;
        configure_context_model(&d, source, "mock", "mock-model").await;

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("override"),
                    s("--model"),
                    s("mock/custom-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify child has overridden model in DB
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("override")
            .unwrap()
            .unwrap();
        assert_eq!(child.provider.as_deref(), Some("mock"));
        assert_eq!(child.model.as_deref(), Some("custom-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read();
        let handle = drift
            .get(child.context_id)
            .expect("child should be in DriftRouter");
        assert_eq!(handle.provider.as_deref(), Some("mock"));
        assert_eq!(handle.model.as_deref(), Some("custom-model"));
    }

    #[tokio::test]
    async fn fork_invalid_provider_errors() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("bad"),
                    s("--model"),
                    s("nonexistent/foo"),
                ],
                &c,
            )
            .await;
        assert!(!result.is_ok(), "should have failed: {}", result.message());
        assert!(
            result.message().contains("unknown provider"),
            "expected 'unknown provider' error, got: {}",
            result.message()
        );

        // Verify no context was created (mutation didn't happen)
        let db = d.kernel_db().lock();
        let found = db.find_context_by_label("bad").unwrap();
        assert!(
            found.is_none(),
            "no context should have been created for invalid provider"
        );
    }

    /// Bare model name (no provider/ prefix) should resolve provider from registry.
    /// This is the bug that caused `kj fork --model claude-sonnet-4-6` to silently
    /// keep the parent's model.
    #[tokio::test]
    async fn fork_bare_model_resolves_provider() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        register_mock_provider(&d).await;
        // Set mock as default so bare model names resolve to it
        {
            let mut registry = d.kernel().llm().write().await;
            registry.set_default("mock");
        }
        configure_context_model(&d, source, "mock", "old-model").await;

        let c = caller_with_context(source);
        // Bare model name — no "mock/" prefix
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("bare"),
                    s("--model"),
                    s("new-model"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // Verify provider was resolved from registry default
        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("bare")
            .unwrap()
            .unwrap();
        assert_eq!(
            child.provider.as_deref(),
            Some("mock"),
            "provider should be resolved from registry"
        );
        assert_eq!(child.model.as_deref(), Some("new-model"));

        // And in DriftRouter
        drop(db);
        let drift = d.drift_router().read();
        let handle = drift
            .get(child.context_id)
            .expect("child should be in DriftRouter");
        assert_eq!(
            handle.provider.as_deref(),
            Some("mock"),
            "DriftRouter provider should match"
        );
        assert_eq!(
            handle.model.as_deref(),
            Some("new-model"),
            "DriftRouter model should match"
        );
    }

    /// Default fork is POSIX-style: the caller stays on the parent. The child
    /// id is surfaced via `data`, not by switching.
    #[tokio::test]
    async fn fork_default_stays_on_parent() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        match &result {
            super::super::KjResult::Ok { data, message, .. } => {
                assert!(message.contains("child"), "msg: {message}");
                let ctx = data
                    .as_ref()
                    .and_then(|d| d.get("context_id"))
                    .and_then(|v| v.as_str())
                    .expect("fork should surface child context_id in data");
                assert_ne!(ctx, source.to_hex(), "child must be a new context");
            }
            other => panic!("expected Ok (stay on parent), got: {}", other.message()),
        }
    }

    /// `--switch` opts back into the old behaviour: move the caller to the child.
    #[tokio::test]
    async fn fork_switch_flag_moves_to_child() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("child"), s("--switch")], &c)
            .await;
        match &result {
            super::super::KjResult::Switch(id, msg) => {
                assert_ne!(*id, source, "should switch to new context");
                assert!(msg.contains("child"), "msg: {msg}");
            }
            other => panic!("expected Switch with --switch, got: {}", other.message()),
        }
    }

    #[tokio::test]
    async fn fork_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--prompt"),
                    s("go explore"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("fork --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the child, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "go explore");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare fork is an inert snapshot — it must not request a turn"
        );
    }

    #[tokio::test]
    async fn fork_staged_with_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--name"),
                    s("child"),
                    s("--prompt"),
                    s("go explore"),
                    s("--stage"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a staged child is awaiting curation — no autonomous turn yet"
        );
    }

    #[tokio::test]
    async fn fork_inherits_workspace() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Bind a workspace to source
        let ws_id = kaijutsu_types::WorkspaceId::new();
        {
            let db = d.kernel_db().lock();
            db.insert_workspace(&crate::kernel_db::WorkspaceRow {
                workspace_id: ws_id,
                                label: "test-ws".into(),
                description: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: principal,
                archived_at: None,
            })
            .unwrap();
            db.update_workspace(source, Some(ws_id)).unwrap();
        }

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("child")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let db = d.kernel_db().lock();
        let child = db
            .find_context_by_label("child")
            .unwrap()
            .unwrap();
        assert_eq!(child.workspace_id, Some(ws_id));
    }

    // ====================================================================
    // POSIX parity across fork kinds: --prompt drives a turn, --switch moves
    // the caller, bare fork drives nothing. Mirrors the fork_full reference
    // tests above (fork_with_prompt_requests_turn / _without_ / _switch_).
    // ====================================================================

    /// Register a mock LLM provider (set as default), configure it on `source`,
    /// and seed `source` with a block so compact's distillation step (an LLM
    /// call over non-empty content) can run in tests.
    async fn setup_compact_source(
        d: &super::super::KjDispatcher,
        source: kaijutsu_types::ContextId,
        principal: PrincipalId,
    ) {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;
        {
            let mut registry = d.kernel().llm().write().await;
            registry.register("mock", Arc::new(Provider::Mock(MockClient::new("summary"))));
            registry.set_default("mock");
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "mock", "mock-model");
        }
        // compact summarizes the source's included blocks; without content it
        // errors (see fork_compact_empty_source_errors).
        d.block_store()
            .insert_block_as(
                source,
                None,
                None,
                kaijutsu_crdt::Role::User,
                kaijutsu_crdt::BlockKind::Text,
                "hello world".to_string(),
                kaijutsu_crdt::Status::Done,
                kaijutsu_crdt::ContentType::Plain,
                Some(principal),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn fork_compact_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[s("fork"), s("--compact"), s("--prompt"), s("explore compact")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        let msg = sub
            .try_recv()
            .expect("fork --compact --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the child, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "explore compact");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_compact_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d.dispatch(&[s("fork"), s("--compact")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare compact fork must not request a turn"
        );
    }

    #[tokio::test]
    async fn fork_compact_switch_flag_moves_to_child() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        setup_compact_source(&d, source, principal).await;
        let c = caller_with_context(source);

        let result = d
            .dispatch(&[s("fork"), s("--compact"), s("--switch")], &c)
            .await;
        match &result {
            super::super::KjResult::Switch(id, _msg) => {
                assert_ne!(*id, source, "--switch should move to a new child context");
            }
            other => panic!("expected Switch with --switch, got: {}", other.message()),
        }
    }

    #[tokio::test]
    async fn fork_subtree_with_prompt_requests_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--as"),
                    s("parent"),
                    s("--name"),
                    s("tmpl"),
                    s("--prompt"),
                    s("explore subtree"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());

        // The turn targets the subtree root.
        let msg = sub
            .try_recv()
            .expect("fork --as --prompt should publish a turn request");
        match msg.payload {
            crate::flows::TurnFlow::Requested {
                context_id,
                principal_id,
                content,
                ..
            } => {
                assert_ne!(context_id, source, "the turn targets the new root, not parent");
                assert_eq!(principal_id, c.principal_id);
                assert_eq!(content, "explore subtree");
            }
            other => panic!("expected Requested, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fork_subtree_without_prompt_requests_no_turn() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("parent"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        let c = caller_with_context(source);
        let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

        let result = d
            .dispatch(
                &[s("fork"), s("--as"), s("parent"), s("--name"), s("tmpl")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(
            sub.try_recv().is_none(),
            "a bare subtree fork must not request a turn"
        );
    }

    // ── 2026-07-04 papercuts: compact-fork label conflict + distill provider ──

    /// Bug 2 (2026-07-04): `kj fork --compact --name <taken>` where the label is
    /// already in use must fail with an ACTIONABLE message — the existing
    /// context's full id plus how to reach it — not a bare
    /// `label conflict: label 'X' already in use`. Before the fix the conflict
    /// surfaced only at the final DB insert, deep after the (billed) distill and
    /// a created orphan document.
    #[tokio::test]
    async fn fork_compact_label_conflict_actionable_message() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        // A working distill provider + a source block, so that WITHOUT the fix
        // the distill succeeds and the fork gets all the way to the failing
        // insert (proving the conflict is caught late).
        setup_compact_source(&d, source, principal).await;
        // The pre-existing context that owns the label.
        let taken = register_context(&d, Some("taken"), None, principal);

        let c = caller_with_context(source);
        let before = d.kernel_db().lock().list_active_contexts().unwrap().len();
        let result = d
            .dispatch(&[s("fork"), s("--compact"), s("--name"), s("taken")], &c)
            .await;
        assert!(!result.is_ok(), "conflicting label must fail: {}", result.message());
        let msg = result.message();
        assert!(
            msg.contains(&taken.to_hex()),
            "error must name the existing context's full id: {msg}"
        );
        assert!(
            msg.contains("switch"),
            "error must hint how to reach the existing context: {msg}"
        );
        // No partially-created context row survives the failed fork.
        let after = d.kernel_db().lock().list_active_contexts().unwrap().len();
        assert_eq!(before, after, "a conflicting fork must not create a context");
    }

    /// The label check must run BEFORE the distillation LLM call — a conflict
    /// shouldn't burn a (slow, billed) summary first. With no LLM configured the
    /// unfixed path fails inside `summarize` ("no LLM configured"); the fixed
    /// path fails earlier with the label conflict + switch hint.
    #[tokio::test]
    async fn fork_compact_label_conflict_precedes_distill() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "some content"); // non-empty source
        register_context(&d, Some("taken"), None, principal);

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--compact"), s("--name"), s("taken")], &c)
            .await;
        assert!(!result.is_ok(), "conflicting label must fail: {}", result.message());
        let msg = result.message();
        assert!(
            msg.contains("switch"),
            "label check must win over the distill: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("llm") && !msg.contains("summariz"),
            "must not have reached the distill step: {msg}"
        );
    }

    /// Bug 3 (2026-07-04): a `--compact` distillation must run on the CALLING
    /// context's OWN provider+model — the pair known to work — not the source's
    /// model re-pinned on the registry's default provider. Here the caller runs
    /// on anthropic while the registry default is deepseek; before the fix the
    /// distill resolved the inherited model through the default provider and ran
    /// on deepseek. Two providers with distinct canned summaries reveal which
    /// one ran via the child's seed block.
    #[tokio::test]
    async fn fork_compact_distill_uses_calling_context_provider() {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "material to distill");

        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "anthropic",
                Arc::new(Provider::Mock(MockClient::new("ANTHROPIC-DISTILL"))),
            );
            reg.register(
                "deepseek",
                Arc::new(Provider::Mock(MockClient::new("DEEPSEEK-DISTILL"))),
            );
            // Default provider is the WRONG one for an anthropic caller — the
            // old resolver pinned the inherited model here.
            reg.set_default("deepseek");
        }
        // The calling context runs on anthropic.
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "anthropic", "claude-haiku-4-5");
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(&[s("fork"), s("--compact"), s("--name"), s("child")], &c)
            .await;
        assert!(result.is_ok(), "compact fork failed: {}", result.message());

        let seed = ordered_contents(&d, child_id(&d, "child"));
        assert!(
            seed.iter().any(|c| c.contains("ANTHROPIC-DISTILL")),
            "distill must run on the calling context's own provider (anthropic): {seed:?}"
        );
        assert!(
            !seed.iter().any(|c| c.contains("DEEPSEEK-DISTILL")),
            "distill must NOT fall to the registry default provider (deepseek): {seed:?}"
        );
    }

    /// An explicit `--distill-model` overrides the calling-context default and
    /// resolves through the registry (alias-aware) — locking that the Bug 3 fix
    /// only changed the *default*, not the override.
    #[tokio::test]
    async fn fork_compact_distill_model_override_wins() {
        use crate::llm::{MockClient, ModelAlias, Provider};
        use std::collections::HashMap;
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "material to distill");

        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "anthropic",
                Arc::new(Provider::Mock(MockClient::new("ANTHROPIC-DISTILL"))),
            );
            reg.register(
                "deepseek",
                Arc::new(Provider::Mock(MockClient::new("DEEPSEEK-DISTILL"))),
            );
            reg.set_default("anthropic");
            let mut aliases = HashMap::new();
            aliases.insert(
                s("cheap"),
                ModelAlias {
                    provider: s("deepseek"),
                    model: s("deepseek-flash"),
                },
            );
            reg.set_model_aliases(aliases);
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "anthropic", "claude-haiku-4-5");
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--compact"),
                    s("--name"),
                    s("child"),
                    s("--distill-model"),
                    s("cheap"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "compact fork failed: {}", result.message());

        let seed = ordered_contents(&d, child_id(&d, "child"));
        assert!(
            seed.iter().any(|c| c.contains("DEEPSEEK-DISTILL")),
            "--distill-model must override to deepseek despite the anthropic caller/default: {seed:?}"
        );
    }

    /// `--distill-model provider/model` (the slash form the failure hint
    /// recommends) must bind the NAMED provider — same grammar as `--model`
    /// via the shared `resolve_model_choice`. Before the fix the override went
    /// through `registry.resolve_model`, which doesn't parse the slash: the
    /// whole string rode as a literal model name on the DEFAULT provider, so
    /// the error message suggested a syntax that then misparsed — the same
    /// papercut class this sweep is killing.
    #[tokio::test]
    async fn fork_compact_distill_model_slash_form_binds_provider() {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "material to distill");

        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "anthropic",
                Arc::new(Provider::Mock(MockClient::new("ANTHROPIC-DISTILL"))),
            );
            reg.register(
                "deepseek",
                Arc::new(Provider::Mock(MockClient::new("DEEPSEEK-DISTILL"))),
            );
            // Default + caller both anthropic: only real slash parsing can
            // reach deepseek. The unfixed path pinned the whole spec on
            // anthropic (which, being a mock, would even "succeed" — with the
            // wrong provider's output).
            reg.set_default("anthropic");
        }
        {
            let mut drift = d.drift_router().write();
            let _ = drift.configure_llm(source, "anthropic", "claude-haiku-4-5");
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--compact"),
                    s("--name"),
                    s("child"),
                    s("--distill-model"),
                    s("deepseek/deepseek-flash"),
                ],
                &c,
            )
            .await;
        assert!(result.is_ok(), "compact fork failed: {}", result.message());

        let seed = ordered_contents(&d, child_id(&d, "child"));
        assert!(
            seed.iter().any(|c| c.contains("DEEPSEEK-DISTILL")),
            "slash-form --distill-model must bind the named provider: {seed:?}"
        );
        assert!(
            !seed.iter().any(|c| c.contains("ANTHROPIC-DISTILL")),
            "slash-form --distill-model must not ride the default provider: {seed:?}"
        );
    }

    /// A slash-form `--distill-model` naming an unknown provider fails LOUD
    /// before any mutation — same posture as `--model nonexistent/foo`. Before
    /// the fix the unknown provider was silently swallowed: the whole spec
    /// became a model name on the default provider and the typo only surfaced
    /// (if at all) as a call-time provider error.
    #[tokio::test]
    async fn fork_compact_distill_model_unknown_provider_errors() {
        use crate::llm::{MockClient, Provider};
        use std::sync::Arc;

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal);
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();
        insert_text(&d, source, principal, "material to distill");
        {
            let mut reg = d.kernel().llm().write().await;
            reg.register(
                "anthropic",
                Arc::new(Provider::Mock(MockClient::new("ANTHROPIC-DISTILL"))),
            );
            reg.set_default("anthropic");
        }

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[
                    s("fork"),
                    s("--compact"),
                    s("--name"),
                    s("child"),
                    s("--distill-model"),
                    s("nonexistent/foo"),
                ],
                &c,
            )
            .await;
        assert!(
            !result.is_ok(),
            "unknown provider in --distill-model must fail: {}",
            result.message()
        );
        assert!(
            result.message().contains("unknown provider"),
            "must fail through the shared resolver, naming the provider: {}",
            result.message()
        );
        // Fails before any mutation — no child context was created.
        let found = d.kernel_db().lock().find_context_by_label("child").unwrap();
        assert!(found.is_none(), "failed distill resolution must not create a context");
    }
}

//! `kj` kaish builtin — routes argv through `KjDispatcher`.
//!
//! Registered as a kaish Tool in EmbeddedKaish via the `configure_tools` callback.
//! Each connection gets its own `KjBuiltin` instance with the shared dispatcher
//! plus per-connection identity.
//!
//! ## Server-side command interception
//!
//! `kj synth` is intercepted here before forwarding to `KjDispatcher` because
//! synthesis requires `kaijutsu-index` (ONNX embedder, HNSW index), which is
//! only available in the server crate — not in `kaijutsu-kernel` where
//! `KjDispatcher` lives.

use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::ast::Value;
use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::{ExecContext, Tool};

use crate::kj::{KjCaller, KjDispatcher, KjResult};
#[allow(unused_imports)]
use kaijutsu_types::{ContentType, ContextId, PrincipalId, SessionId};

use super::context_engine::{SessionContextExt, SessionContextMap};

/// kaish builtin tool for the `kj` command.
///
/// Bridges kaish's `Tool` trait to `KjDispatcher`. Each connection gets its own
/// `KjBuiltin` with the shared dispatcher and per-connection identity fields.
pub struct KjBuiltin {
    dispatcher: Arc<KjDispatcher>,
    session_contexts: SessionContextMap,
    principal_id: PrincipalId,
    session_id: SessionId,
    /// Semantic index for synthesis commands. None if embedding model not configured.
    semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
    /// Block source adapter for fetching context blocks during synthesis.
    block_source: Arc<dyn kaijutsu_index::BlockSource>,
    /// True only when registered by the rc lifecycle's privileged kaish.
    /// Stamped onto every `KjCaller` this builtin constructs so the binding
    /// setter can tell rc-assigned loadouts from agent-issued ones. Trusted
    /// because the agent cannot influence how its `KjBuiltin` was built.
    privileged: bool,
}

impl KjBuiltin {
    pub fn new(
        dispatcher: Arc<KjDispatcher>,
        session_contexts: SessionContextMap,
        principal_id: PrincipalId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
        privileged: bool,
    ) -> Self {
        Self {
            dispatcher,
            session_contexts,
            principal_id,
            session_id,
            semantic_index,
            block_source,
            privileged,
        }
    }

    fn current_context_id(&self) -> Option<ContextId> {
        self.session_contexts.current(&self.session_id)
    }

    fn set_context_id(&self, id: ContextId) {
        self.session_contexts.insert(self.session_id, id);
    }

    /// Persist the current context's cwd to KernelDb so it survives session
    /// reconnects and context switches.
    ///
    /// The live cwd was validated when `cd` set it, so this normally persists
    /// known-good state — but if the directory was removed under us mid-session
    /// it would now point at a dead path. Validate against the backend and skip
    /// the write in that case, preserving the last good persisted value rather
    /// than overwriting it with a path every later restore would reject.
    async fn save_context_cwd(&self, context_id: ContextId, ctx: &ExecContext) {
        let path = ctx.cwd.clone();
        let is_dir = matches!(ctx.backend.stat(&path).await, Ok(entry) if entry.is_dir());
        if !is_dir {
            tracing::warn!(
                context = %context_id.to_hex(),
                cwd = %path.display(),
                "live cwd no longer resolves in backend; not persisting on switch",
            );
            kaijutsu_telemetry::record_cwd_restore_failed();
            return;
        }
        let db = self.dispatcher.kernel_db().lock();
        if let Err(e) = db.upsert_context_shell(
            &crate::kernel_db::ContextShellRow {
                context_id,
                cwd: Some(path.to_string_lossy().into_owned()),
                updated_at: kaijutsu_types::now_millis() as i64,
            },
        ) {
            tracing::warn!(
                context = %context_id.to_hex(),
                error = %e,
                "failed to persist context cwd"
            );
        }
    }

    /// Load context shell config (cwd + env vars) from KernelDb and apply to ExecContext.
    async fn apply_context_config(&self, context_id: ContextId, ctx: &mut ExecContext) {
        // Snapshot durable state and drop the DB lock before any await.
        let (persisted_cwd, env_vars) = {
            let db = self.dispatcher.kernel_db().lock();
            let cwd = db
                .get_context_shell(context_id)
                .ok()
                .flatten()
                .and_then(|shell| shell.cwd);
            let vars = db.get_context_env(context_id).unwrap_or_default();
            (cwd, vars)
        };

        // Apply cwd, validated against the shell's backend — the namespace `cd`
        // resolves against, not the host filesystem (a host-FS `is_dir()` check
        // would wrongly reject VFS-only cwds like /scratch or /v/docs).
        if let Some(cwd) = persisted_cwd {
            let path = std::path::PathBuf::from(&cwd);
            let is_dir = matches!(ctx.backend.stat(&path).await, Ok(entry) if entry.is_dir());
            if is_dir {
                ctx.set_cwd(path);
            } else {
                tracing::warn!(
                    context = %context_id.to_hex(),
                    cwd = %cwd,
                    "context cwd no longer resolves in backend on switch; keeping current",
                );
                kaijutsu_telemetry::record_cwd_restore_failed();
            }
        }

        // Apply env vars (exported so they propagate to child processes)
        for var in &env_vars {
            ctx.scope
                .set_exported(&var.key, Value::String(var.value.clone()));
        }
    }

    // ========================================================================
    // Synthesis commands (server-side, needs kaijutsu-index)
    // ========================================================================

    /// Dispatch `kj synth <subcommand>`.
    async fn dispatch_synth(&self, argv: &[String], caller: &KjCaller) -> ExecResult {
        let sub = argv.first().map(|s| s.as_str()).unwrap_or("help");

        match sub {
            "all" => self.synth_all().await,
            "status" => self.synth_status(),
            "help" | "--help" | "-h" => ExecResult::success(Self::synth_help()),
            // Anything else: treat as a context ref
            _ => self.synth_context(sub, caller).await,
        }
    }

    /// `kj synth all` — index + synthesize all active contexts.
    async fn synth_all(&self) -> ExecResult {
        let Some(ref idx) = self.semantic_index else {
            return ExecResult::failure(
                1,
                "semantic index not configured (check embedding model in models.toml)".to_string(),
            );
        };

        let _kernel_id = self.dispatcher.kernel_id();
        let contexts = {
            let db = self.dispatcher.kernel_db().lock();
            match db.list_active_contexts() {
                Ok(rows) => rows,
                Err(e) => return ExecResult::failure(1, format!("failed to list contexts: {e}")),
            }
        };

        if contexts.is_empty() {
            return ExecResult::success("no active contexts".to_string());
        }

        let total = contexts.len();
        let mut indexed = 0usize;
        let mut synthesized = 0usize;
        let mut skipped = 0usize;
        let mut errors = Vec::new();

        for row in &contexts {
            let ctx_id = row.context_id;
            let idx = idx.clone();
            let block_source = self.block_source.clone();

            // Index + synthesize on blocking thread.
            // BlockStoreSource auto-hydrates from DB if the document isn't in memory.
            // Contexts with no document at all (metadata-only) are skipped.
            let result = tokio::task::spawn_blocking(move || {
                let blocks = match block_source.block_snapshots(ctx_id) {
                    Ok(b) if b.is_empty() => return Ok(None), // empty doc, nothing to synthesize
                    Ok(b) => b,
                    Err(_) => return Ok(None), // no document in DB either, skip
                };

                let was_indexed = idx
                    .index_context(ctx_id, &blocks)
                    .map_err(|e| format!("index: {e}"))?;

                let synth =
                    super::synthesis::run_synthesis(ctx_id, idx.embedder_arc(), block_source);

                Ok::<Option<(bool, Option<kaijutsu_index::synthesis::SynthesisResult>)>, String>(
                    Some((was_indexed, synth)),
                )
            })
            .await;

            match result {
                Ok(Ok(Some((was_indexed, synth_result)))) => {
                    if was_indexed {
                        indexed += 1;
                    }
                    if let Some(synth) = synth_result {
                        if let Some(ref si) = self.semantic_index {
                            si.synthesis_cache().insert(ctx_id, synth);
                        }
                        synthesized += 1;
                    }
                }
                Ok(Ok(None)) => {
                    skipped += 1;
                } // no document or empty
                Ok(Err(e)) => errors.push(format!("{}: {e}", ctx_id.short())),
                Err(e) => errors.push(format!("{}: join error: {e}", ctx_id.short())),
            }
        }

        let mut out = format!(
            "{total} contexts: {indexed} indexed, {synthesized} synthesized, {skipped} skipped"
        );
        if !errors.is_empty() {
            out.push_str(&format!(
                "\nerrors ({}):\n  {}",
                errors.len(),
                errors.join("\n  ")
            ));
        }
        ExecResult::success(out)
    }

    /// `kj synth <ctx_ref>` — index + synthesize a single context.
    async fn synth_context(&self, ctx_ref: &str, caller: &KjCaller) -> ExecResult {
        let Some(ref idx) = self.semantic_index else {
            return ExecResult::failure(
                1,
                "semantic index not configured (check embedding model in models.toml)".to_string(),
            );
        };

        // Resolve context reference
        let parsed = crate::kj::refs::parse_context_ref(ctx_ref);
        let _kernel_id = self.dispatcher.kernel_id();
        let ctx_id = {
            let db = self.dispatcher.kernel_db().lock();
            match crate::kj::refs::resolve_context_ref(&parsed, caller, &db) {
                Ok(id) => id,
                Err(e) => return ExecResult::failure(1, e),
            }
        };

        let idx = idx.clone();
        let block_source = self.block_source.clone();

        let result = tokio::task::spawn_blocking(move || {
            let blocks = block_source
                .block_snapshots(ctx_id)
                .map_err(|e| format!("block fetch: {e}"))?;

            let was_indexed = idx
                .index_context(ctx_id, &blocks)
                .map_err(|e| format!("index: {e}"))?;

            let synth =
                super::synthesis::run_synthesis(ctx_id, idx.embedder_arc(), block_source);

            if let Some(ref s) = synth {
                idx.synthesis_cache().insert(ctx_id, s.clone());
            }

            Ok::<(bool, Option<kaijutsu_index::synthesis::SynthesisResult>), String>((
                was_indexed,
                synth,
            ))
        })
        .await;

        match result {
            Ok(Ok((was_indexed, synth_result))) => {
                let mut out = ctx_id.short().to_string();
                if was_indexed {
                    out.push_str(" (newly indexed)");
                }
                if let Some(synth) = synth_result {
                    let kw: Vec<&str> = synth.keywords.iter().map(|(k, _)| k.as_str()).collect();
                    let kw_str = if kw.is_empty() {
                        "(none)".to_string()
                    } else {
                        kw.join(", ")
                    };
                    out.push_str(&format!("\nkeywords: {kw_str}"));
                    if !synth.top_blocks.is_empty() {
                        let preview = &synth.top_blocks[0].2;
                        let end = preview.len().min(60);
                        out.push_str(&format!("\npreview: {}...", &preview[..end]));
                    }
                } else {
                    out.push_str("\nno synthesis result (empty context?)");
                }
                ExecResult::success(out)
            }
            Ok(Err(e)) => ExecResult::failure(1, format!("synthesis failed: {e}")),
            Err(e) => ExecResult::failure(1, format!("synthesis task failed: {e}")),
        }
    }

    /// `kj synth status` — show index statistics.
    fn synth_status(&self) -> ExecResult {
        let Some(ref idx) = self.semantic_index else {
            return ExecResult::success(
                "semantic index: not configured\n(set embedding model in models.toml)".to_string(),
            );
        };

        let count = idx.len();
        let model = idx.embedder().model_name().to_string();
        let dims = idx.embedder().dimensions();

        ExecResult::success(format!(
            "semantic index: {count} contexts indexed\nmodel: {model} ({dims}d)"
        ))
    }

    fn synth_help() -> String {
        "\
kj synth — semantic indexing + keyword synthesis

Commands:
  kj synth all          Index and synthesize all active contexts
  kj synth <ctx>        Index and synthesize a specific context
  kj synth status       Show index statistics
  kj synth help         Show this help

Context references: . (current), .parent, label, hex prefix

Examples:
  kj synth .            Synthesize current context
  kj synth all          Bulk index + synthesize everything
  kj synth explore      Synthesize context labeled \"explore\"
  kj synth status       Show model info and index count"
            .to_string()
    }
}

#[async_trait]
impl Tool for KjBuiltin {
    fn name(&self) -> &str {
        "kj"
    }

    fn schema(&self) -> ToolSchema {
        // Reflected from the composed clap `Command` tree — the single source of
        // truth for both routing (`dispatch`) and schema. `with_owned_output()`
        // marks the tree so the kernel skips its generic `--json` formatter (kj
        // renders its own envelopes) and re-advertises `json` per node. See
        // docs/monday-clap-upgrades.md §2.1/§2.4. This replaces the hand-written
        // flat `.param(...)` union that `11160e5` last reconciled; the `-t`
        // collision (cache `--target` vs context `--tree`) now resolves because
        // each lives on its own leaf.
        kaish_kernel::tools::schema_tree_from_clap(
            &crate::kj::kj_command(),
            "kj",
            "Kernel command interface. Run `kj help` or `kj <command> help` for detailed workflows.",
            [
                ("Discover commands", "kj help"),
                ("View context topology", "kj context list --tree"),
                ("Create isolated workspace", "kj fork --name debug-auth"),
                ("Navigate to context", "kj context switch debug-auth"),
                (
                    "Stage finding for another context",
                    "kj drift push main \"auth tokens are stored in Redis\"",
                ),
                ("Deliver all staged drifts", "kj drift flush"),
                (
                    "LLM-distill another context's work",
                    "kj drift pull main \"what changed in auth?\"",
                ),
                ("Merge fork back to parent", "kj drift merge"),
                (
                    "Set model on current context",
                    "kj context set . --model anthropic/claude-sonnet-4-5-20250929",
                ),
                ("Bulk synthesize keywords", "kj synth all"),
            ],
        )
        .with_owned_output()
        // Claim `--help` on the root schema so kaish's outer help router does
        // NOT intercept it. The kernel's dispatch_command short-circuits any
        // `--help` whose *root* schema doesn't claim the flag, rendering the
        // generic whole-tool help instead of routing — so `kj context create
        // --help` would never reach kj's own dispatch (where clap renders the
        // leaf's help). By advertising `help` here, `schema_claims("help")` is
        // true, kaish passes `--help` through to execute()→dispatch(), and the
        // leaf clap parser renders subcommand help. `kj --help` (first token)
        // still hits the dispatch top-level help arm, unchanged. This is the
        // local half of the fix; the general fix (make the kernel's wants_help
        // check schema-tree-aware for owned-output tools that re-parse their
        // own argv) is filed upstream at tobert/kaish#51.
        .param(
            kaish_kernel::tools::ParamSchema::new("help", "bool")
                .with_description("Show help for this command (routed to the leaf, not the outer router)"),
        )
    }

    async fn execute(&self, args: ToolArgs, ctx: &mut dyn ToolCtx) -> ExecResult {
        // kj is a trusted in-tree builtin: it needs the kernel's full
        // ExecContext (stdin pipe, nonce latch, cwd persistence), not the
        // trimmed portable surface. Downcast through the trait's escape hatch.
        let ctx = ctx
            .as_any_mut()
            .downcast_mut::<ExecContext>()
            .expect("kj builtin always runs against the kernel ExecContext");

        // Build argv from positional args + named args + flags.
        // kaish splits `kj fork --name exploration` into:
        //   positional: ["fork"], named: {"name": "exploration"}
        // We reconstruct the flat argv that KjDispatcher.dispatch() expects.
        let mut argv: Vec<String> = args
            .positional
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Int(n) => n.to_string(),
                Value::Float(f) => f.to_string(),
                Value::Bool(b) => b.to_string(),
                other => format!("{other:?}"),
            })
            .collect();

        // Reconstruct --key value pairs from named args
        for (key, val) in &args.named {
            let flag = if key.len() == 1 {
                format!("-{key}")
            } else {
                format!("--{key}")
            };
            argv.push(flag);
            match val {
                Value::String(s) => argv.push(s.clone()),
                Value::Int(n) => argv.push(n.to_string()),
                Value::Float(f) => argv.push(f.to_string()),
                Value::Bool(b) => argv.push(b.to_string()),
                other => argv.push(format!("{other:?}")),
            }
        }

        // Reconstruct boolean flags
        for flag in &args.flags {
            if flag.len() == 1 {
                argv.push(format!("-{flag}"));
            } else {
                argv.push(format!("--{flag}"));
            }
        }

        // Extract --confirm <nonce> before dispatch
        let confirm_nonce = crate::kj::parse::extract_named_arg(&argv, &["--confirm"]);
        crate::kj::parse::strip_named_arg(&mut argv, &["--confirm"]);

        // Extract the global --json flag before dispatch. The per-subcommand
        // clap parsers don't declare it (it's a kj-wide presentation flag), so
        // leaving it in argv would make `kj context list --json` fail with
        // "unexpected argument". When set, the KjResult is rendered as a JSON
        // envelope after dispatch instead of the human text.
        let json_requested = crate::kj::parse::has_flag(&argv, &["--json"]);
        argv.retain(|a| a != "--json");

        // Stdin → --content for `kj rc add` / `kj rc edit`. Lets shell
        // pipelines author multi-line .md / .kai scripts:
        //   cat prompt.md | kj rc add /etc/rc/coder/create/S00-stance.md
        // Only kicks in when --content was not given explicitly. Single
        // consumer for now; if more kj subcommands grow stdin appetite,
        // promote this to the dispatcher signature.
        if argv.first().map(|s| s.as_str()) == Some("rc")
            && matches!(argv.get(1).map(|s| s.as_str()), Some("add") | Some("edit"))
            && !crate::kj::parse::has_flag(&argv, &["--content"])
            && let Some(body) = ctx.read_stdin_to_string().await
            && !body.is_empty()
        {
            argv.push("--content".into());
            argv.push(body);
        }

        let mut caller = KjCaller {
            principal_id: self.principal_id,
            context_id: self.current_context_id(),
            session_id: self.session_id,
            confirmed: false,
            rc_depth: 0,
            privileged: self.privileged,
        };

        // If --confirm provided, verify nonce BEFORE dispatching
        if let Some(nonce) = &confirm_nonce {
            let cmd_scope = build_command_scope(&argv);
            let target_scope = build_target_scope(&argv);
            match ctx.verify_nonce(nonce, &cmd_scope, &[&target_scope]) {
                Ok(()) => caller.confirmed = true,
                Err(e) => return ExecResult::failure(1, format!("kj: {e}")),
            }
        }

        // Server-crate commands intercepted here because they require
        // dependencies that kaijutsu-kernel does not have.
        if argv.first().map(|s| s.as_str()) == Some("synth") {
            return self.dispatch_synth(&argv[1..], &caller).await;
        }

        // Distillation verbs make a blocking, in-line LLM completion
        // (`summarize` → `prompt_with_system`, which has no internal timeout of
        // its own). Without help, that model think-time races the script-level
        // `kaish_request_timeout` watchdog — forcing the operator to choose
        // between a watchdog tight enough to catch a wedged shell loop and one
        // loose enough for a legitimately-slow distill. `ToolCtx::patient`
        // dissolves that conflict: it freezes the script clock for the hold and
        // governs the distill under its own `llm_request_timeout` budget
        // (cancellation stays live, so a wedged provider or a user interrupt
        // still aborts it). The guard is scoped to the dispatch call so it drops
        // before the `match result` below re-borrows `ctx`. Only the distill
        // verbs are wrapped — wrapping every `kj` call would freeze the clock
        // through a tight `while true; do kj …; done` and the watchdog would
        // never catch the runaway. See docs/issues.md (kaish `patient` adoption).
        let result = if is_distill_verb(&argv) {
            let budget = self.dispatcher.kernel().timeouts().llm_request_timeout;
            let _patient = ctx.patient(budget);
            self.dispatcher.dispatch(&argv, &caller).await
        } else {
            self.dispatcher.dispatch(&argv, &caller).await
        };

        let exec = match result {
            KjResult::Ok {
                message,
                content_type,
                ephemeral,
                data,
            } => {
                let mut result = ExecResult::success(message);
                result.content_type = if content_type != ContentType::Plain {
                    Some(content_type.as_mime().to_string())
                } else {
                    None
                };
                if ephemeral {
                    result
                        .baggage
                        .insert("kaijutsu.ephemeral".into(), "true".into());
                }
                // Carry structured payload into the shell's `.data` slot so
                // `for x in $(kj …)` iterates JSON arrays and `kaish-last`
                // can read JSON objects. See `KjResult::Ok::data` for the
                // shape conventions.
                if let Some(json) = data {
                    result.data = Some(kaish_kernel::interpreter::json_to_value(json));
                }
                result
            }
            KjResult::Err(msg) => ExecResult::failure(1, msg),
            KjResult::Switch(new_id, msg) => {
                // Persist outgoing context's cwd so it survives the switch
                if let Some(old_id) = self.current_context_id() {
                    self.save_context_cwd(old_id, ctx).await;
                }

                // Side-effect: update the shared context ID
                self.set_context_id(new_id);

                // Apply context shell config (cwd + env vars)
                self.apply_context_config(new_id, ctx).await;

                ExecResult::success(msg)
            }
            KjResult::Latch {
                command,
                target,
                message,
            } => {
                let original_argv = argv.join(" ");
                ctx.latch_result(&command, &[&target], &message, |nonce| {
                    format!("kj {} --confirm {}", original_argv, nonce)
                })
            }
        };

        // --json post-processing: re-render the ExecResult as a JSON envelope on
        // stdout while preserving the exit code and the side effects already
        // applied above (a `Switch` has switched, a `Latch` has latched). The
        // structured `.data` rides along so `kaish-last` / for-loops still see
        // it; the human-readable text moves into the envelope's `message`.
        if json_requested {
            return render_json_envelope(exec);
        }
        exec
    }
}

/// Wrap an already-produced [`ExecResult`] in the `kj --json` envelope:
/// `{ "ok", "exit_code", "message", "data" }`. The original exit code is
/// preserved (nonzero on failure, 2 on a latch prompt) so `$?`/scripting still
/// works; the body becomes the JSON object with `content_type=application/json`.
fn render_json_envelope(exec: ExecResult) -> ExecResult {
    let ok = exec.code == 0;
    // On failure the human message lives in `.err`; on success it's stdout.
    let stdout = exec.text_out().into_owned();
    let message = if exec.err.is_empty() {
        stdout
    } else {
        exec.err.clone()
    };
    let data_json = exec
        .data
        .as_ref()
        .map(kaish_kernel::interpreter::value_to_json)
        .unwrap_or(serde_json::Value::Null);
    let envelope = serde_json::json!({
        "ok": ok,
        "exit_code": exec.code,
        "message": message,
        "data": data_json,
    });

    let mut out = ExecResult::success(envelope.to_string());
    out.code = exec.code;
    out.content_type = Some("application/json".to_string());
    // Preserve the iteration-friendly structured payload and any baggage
    // (e.g. the ephemeral marker) the original result carried.
    out.data = exec.data;
    out.baggage = exec.baggage;
    out
}

/// Does this `kj` invocation make a blocking, in-line LLM distillation call?
///
/// These are the only `kj` verbs that synchronously hold the kaish builtin on a
/// `summarize`/`prompt_with_system` completion (kj/drift.rs + kj/fork.rs); every
/// other verb either returns promptly or hands LLM work off to the server's
/// stream loop (`kj drive` publishes a turn request and returns). `argv` here is
/// post-normalization — `--confirm`/`--json` already stripped — so a flag like
/// `--summarize`/`--compact` survives if the caller passed it. The match mirrors
/// the dispatcher's routing; a miss only forfeits the patient hold (status quo),
/// never miscarries the command.
fn is_distill_verb(argv: &[String]) -> bool {
    let has = |flag: &str| argv.iter().any(|a| a == flag);
    match (argv.first().map(String::as_str), argv.get(1).map(String::as_str)) {
        (Some("fork"), _) => has("--compact"),
        (Some("drift"), Some("pull")) | (Some("drift"), Some("merge")) => true,
        (Some("drift"), Some("push")) => has("--summarize"),
        _ => false,
    }
}

/// Build a command scope string from argv for nonce validation.
///
/// e.g., `["context", "archive", "old-ctx"]` → `"kj context archive"`
fn build_command_scope(argv: &[String]) -> String {
    let mut parts: Vec<&str> = argv
        .iter()
        .map(|s| s.as_str())
        .take_while(|s| !s.starts_with('-'))
        .take(2) // At most: subcommand + verb
        .collect();
    // Canonicalize the verb alias so a nonce issued for the canonical scope
    // (e.g. "kj context remove" — the form printed in the "To confirm, run:"
    // message) verifies whether the user confirmed with the canonical verb or
    // its alias ("kj context rm … --confirm"). Latched destructive commands
    // are the only ones that reach here; today only `rm` aliases a latched
    // verb (context/preset/workspace remove), but mapping is centralized so a
    // future alias (`del`, etc.) is a one-line add.
    if let Some(verb) = parts.get_mut(1) {
        *verb = canonical_latched_verb(verb);
    }
    format!("kj {}", parts.join(" "))
}

/// Map a verb alias to the canonical form used by latched-command nonce
/// scopes. Identity for anything that isn't a known destructive alias.
fn canonical_latched_verb(verb: &str) -> &str {
    match verb {
        "rm" => "remove",
        other => other,
    }
}

/// Extract the target (context label/ref) from argv for nonce validation.
///
/// Heuristic: first positional arg after the subcommand verb.
/// e.g., `["context", "archive", "old-ctx"]` → `"old-ctx"`
fn build_target_scope(argv: &[String]) -> String {
    // Skip subcommand and verb, take the first non-flag arg
    argv.iter()
        .skip(2) // Skip e.g. "context" "archive"
        .find(|s| !s.starts_with('-'))
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! End-to-end coverage that `KjResult::Ok::data` survives the
    //! kj → KjBuiltin → ExecResult → kaish for-loop pipeline. The
    //! dispatcher-level tests in `kj/{context,block}.rs` cover the
    //! `KjResult` half; these tests cover the wiring half — concretely,
    //! that `for c in $(kj context list); do echo $c; done` walks the
    //! handles instead of the rendered table.
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::kj::test_helpers::{register_context, test_dispatcher};
    use crate::runtime::context_engine::session_context_map;
    use crate::runtime::embedded_kaish::EmbeddedKaish;
    use kaish_kernel::ExecuteOptions;
    use kaijutsu_types::SessionId;

    /// Build an `EmbeddedKaish` wired to a `KjBuiltin` rooted at the given
    /// dispatcher. Mirrors the rc-lifecycle wiring in `kj/lifecycle.rs`
    /// but without the script-execution scaffolding.
    async fn embedded_with_kj(
        dispatcher: Arc<KjDispatcher>,
        ctx: ContextId,
    ) -> EmbeddedKaish {
        let blocks = shared_block_store(PrincipalId::system());
        let kernel = dispatcher.kernel().clone();
        let _kernel_id = dispatcher.kernel_id();
        let session_id = SessionId::new();
        let session_contexts = session_context_map();
        session_contexts.insert(session_id, ctx);

        let configure_tools = move |scm: SessionContextMap,
                                    sid: SessionId,
                                    tools: &mut kaish_kernel::ToolRegistry| {
            tools.register(KjBuiltin::new(
                dispatcher,
                scm,
                PrincipalId::system(),
                sid,
                None,
                Arc::new(crate::kj::lifecycle::NoopBlockSource),
                false,
            ));
        };

        EmbeddedKaish::with_identity(
            "test-kj-data",
            blocks,
            kernel,
            None,
            PrincipalId::system(),
            ctx,
            session_id,
                        session_contexts,
            crate::runtime::embedded_kaish::ExternalExec::Deny,
            configure_tools,
        )
        .expect("EmbeddedKaish init")
    }

    /// The headline guarantee: structured `.data` makes
    /// `for c in $(kj context list)` iterate per handle. Without the
    /// wiring this loop would split the rendered table line-by-line and
    /// the asserted echoes would look very different.
    #[tokio::test]
    async fn for_loop_iterates_kj_context_list_handles() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let alpha = register_context(&dispatcher, Some("alpha"), None, principal);
        let beta = register_context(&dispatcher, Some("beta"), None, principal);

        let kaish = embedded_with_kj(dispatcher.clone(), alpha).await;

        // The loop body uses `echo` so we can assert on a stable stdout shape.
        let script = "for c in $(kj context list); do echo \"got=$c\"; done";
        let res = kaish
            .execute_with_options(script, ExecuteOptions::default())
            .await
            .expect("kaish exec");

        assert!(res.ok(), "for-loop exit code != 0: {:?}", res);
        let stdout = res.text_out();
        assert!(
            stdout.contains("got=alpha"),
            "expected per-handle echo for 'alpha': {stdout}"
        );
        assert!(
            stdout.contains("got=beta"),
            "expected per-handle echo for 'beta': {stdout}"
        );
        // Negative: the rendered table looks like `* <short>  alpha …`. If
        // `.data` were missing kaish would split the table per line, and we'd
        // see lines that begin with the column header rather than just the
        // handle. Iterations must be exactly two and exactly the labels —
        // anything else means the fallback path is leaking through.
        let got_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("got="))
            .collect();
        assert_eq!(
            got_lines.len(),
            2,
            "expected 2 iterations, got {}: {got_lines:?}",
            got_lines.len()
        );
        for line in &got_lines {
            let payload = line.strip_prefix("got=").unwrap();
            assert!(
                payload == "alpha" || payload == "beta",
                "iteration leaked table text in line {line:?}; \
                 expected exactly a label, got {payload:?}"
            );
        }
        let _ = beta; // touched above; keep the binding live
    }

    /// `kj context list --json` (flag AFTER the subcommand) must emit the JSON
    /// envelope, not error with "unexpected argument". The whole point of item 3:
    /// `--json` binds at the leaf even though no leaf declares it.
    #[tokio::test]
    async fn json_flag_after_subcommand_emits_envelope() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let alpha = register_context(&dispatcher, Some("alpha"), None, principal);
        register_context(&dispatcher, Some("beta"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), alpha).await;

        let res = kaish
            .execute_with_options("kj context list --json", ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "context list --json exit != 0: {res:?}");

        let stdout = res.text_out();
        let parsed: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout not JSON ({e}): {stdout}"));
        assert_eq!(parsed["ok"], serde_json::json!(true));
        assert_eq!(parsed["exit_code"], serde_json::json!(0));
        // `kj context list` emits a `data` array of context handles — it must
        // survive into the envelope.
        let data = parsed["data"].as_array().expect("data is an array");
        let labels: Vec<&str> = data.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            labels.contains(&"alpha") && labels.contains(&"beta"),
            "envelope data must carry context handles: {labels:?}"
        );
    }

    /// `kj --json context list` (flag BEFORE the subcommand) is the other form
    /// the issue called out — it must work identically.
    #[tokio::test]
    async fn json_flag_before_subcommand_emits_envelope() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let alpha = register_context(&dispatcher, Some("alpha"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), alpha).await;

        let res = kaish
            .execute_with_options("kj --json context list", ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "--json context list exit != 0: {res:?}");
        let parsed: serde_json::Value = serde_json::from_str(&res.text_out())
            .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {}", res.text_out()));
        assert_eq!(parsed["ok"], serde_json::json!(true));
    }

    /// An error under `--json` still produces a parseable envelope with
    /// `ok=false` and a nonzero exit code (the message moves into the envelope).
    #[tokio::test]
    async fn json_flag_renders_errors_as_envelope() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("alpha"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), ctx).await;

        // A context ref that resolves to nothing → KjResult::Err.
        let res = kaish
            .execute_with_options(
                "kj context info no-such-context --json",
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec");
        assert!(!res.ok(), "errored command should keep nonzero exit: {res:?}");
        let parsed: serde_json::Value = serde_json::from_str(&res.text_out())
            .unwrap_or_else(|e| panic!("error envelope not JSON ({e}): {}", res.text_out()));
        assert_eq!(parsed["ok"], serde_json::json!(false));
        assert!(
            parsed["message"].as_str().is_some_and(|m| !m.is_empty()),
            "error envelope must carry a message: {parsed}"
        );
    }

    /// A latch nonce issued for the canonical scope (`kj context remove`,
    /// the form printed in the confirm prompt) must verify whether the user
    /// confirms via the canonical verb or the `rm` alias. Regression: the
    /// confirm path echoed the raw alias (`kj context rm`) and rejected the
    /// nonce with "scope mismatch".
    #[test]
    fn latch_scope_canonicalizes_rm_alias() {
        let s = |v: &str| v.to_string();
        let canonical = build_command_scope(&[s("context"), s("remove"), s("victim")]);
        let aliased = build_command_scope(&[s("context"), s("rm"), s("victim")]);
        assert_eq!(canonical, "kj context remove");
        assert_eq!(aliased, canonical, "rm alias must map to the canonical scope");
        // Non-aliased verbs are untouched.
        assert_eq!(
            build_command_scope(&[s("context"), s("archive"), s("x")]),
            "kj context archive"
        );
    }

    /// A space-separated valued flag (`--type default`) must reach kj
    /// through kaish, not just the `--type=default` form. Regression: kaish
    /// only treats `--flag value` as a valued arg when the flag is declared
    /// in the kj tool schema; an undeclared `--type` was parsed as a bool
    /// flag and the value dropped, so `kj rc list --type default` silently
    /// listed every type. Asserts the filter actually applies.
    #[tokio::test]
    async fn space_separated_type_flag_reaches_kj() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("alpha"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), ctx).await;

        let res = kaish
            .execute_with_options(
                "kj rc list --type default",
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec");
        assert!(res.ok(), "kj rc list exit != 0: {res:?}");
        let stdout = res.text_out();

        assert!(
            stdout.contains("/etc/rc/default/"),
            "filtered list should include default paths: {stdout}"
        );
        // The filter must EXCLUDE other types — the whole point of --type.
        for other in [
            "/etc/rc/coder/",
            "/etc/rc/mcp/",
            "/etc/rc/director/",
            "/etc/rc/toolie/",
        ] {
            assert!(
                !stdout.contains(other),
                "--type default must exclude {other}, got: {stdout}"
            );
        }
    }

    /// The headline win of per-leaf schemas (the flat-schema retirement): `-t`
    /// means different things on different leaves and BOTH bind correctly.
    /// `kj cache add -t <target>` takes a value (the cache target); `kj context
    /// list -t` is a bool (tree view). The old flat schema could bind `-t` to
    /// exactly one meaning (`target`), so `kj context list -t` was the casualty.
    /// This is the acceptance gate for the reflected schema.
    #[tokio::test]
    async fn dash_t_disambiguates_per_leaf() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::system();
        let ctx = register_context(&dispatcher, Some("alpha"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), ctx).await;

        // `-t tools` binds as a VALUE on `cache add`. If `-t` bound as a bool,
        // clap would reject the missing required `--target` (or strand "tools"),
        // so a clean exit already proves it took the value.
        let add = kaish
            .execute_with_options("kj cache add -t tools --ttl extended", ExecuteOptions::default())
            .await
            .expect("kaish exec (cache add)");
        assert!(add.ok(), "cache add -t tools (value) failed: {add:?}");

        // Confirm the value reached the target — a 'tools' breakpoint exists.
        let bps = dispatcher
            .kernel_db()
            .lock()
            .list_cache_breakpoints(ctx)
            .expect("list breakpoints");
        assert!(
            bps.iter().any(|bp| matches!(bp, crate::llm::stream::CacheTarget::Tools(_))),
            "`-t tools` must bind target=tools, got: {bps:?}"
        );

        // `-t` binds as a BOOL on `context list` (tree view). Under the old flat
        // schema `-t` was the value flag `target`, so this form mis-bound; now it
        // resolves to `--tree` on its own leaf.
        let list = kaish
            .execute_with_options("kj context list -t", ExecuteOptions::default())
            .await
            .expect("kaish exec (context list -t)");
        assert!(list.ok(), "context list -t (bool tree) failed: {list:?}");
    }

    /// `kj context create <label> --type <t>` must land the context_type on
    /// the row for BOTH the space form (`--type toolie`) and the equals
    /// form (`--type=toolie`). Regression: when `--type` is absent from the
    /// kj tool schema, kaish parses the space form as a bool flag + a stray
    /// positional, the value is divorced, and create silently falls back to
    /// the permissive "default" context_type — a privilege-escalation-by-typo
    /// (read-only `toolie` becomes the default loadout). The equals form is
    /// the control: it binds regardless of the schema, so if both asserts
    /// pass we know the space form genuinely round-tripped.
    #[tokio::test]
    async fn context_create_type_flag_binds_both_forms() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::system();
        let root = register_context(&dispatcher, Some("root"), None, principal);
        let kaish = embedded_with_kj(dispatcher.clone(), root).await;

        // Space form.
        let res = kaish
            .execute_with_options(
                "kj context create exp --type toolie",
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec");
        assert!(res.ok(), "create (space form) exit != 0: {res:?}");

        // Equals form (control).
        let res2 = kaish
            .execute_with_options(
                "kj context create exp2 --type=toolie",
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec");
        assert!(res2.ok(), "create (eq form) exit != 0: {res2:?}");

        let db = dispatcher.kernel_db().lock();
        let exp = db
            .find_context_by_label("exp")
            .unwrap()
            .expect("exp context exists");
        assert_eq!(
            exp.context_type, "toolie",
            "space-form `--type toolie` must set context_type, not silently default"
        );
        let exp2 = db
            .find_context_by_label("exp2")
            .unwrap()
            .expect("exp2 context exists");
        assert_eq!(
            exp2.context_type, "toolie",
            "eq-form `--type=toolie` must set context_type"
        );
    }

    /// A newly-declared clap-subcommand value flag binds in the space form.
    /// `kj block append <id> --text <body>` lives in the clap-parsed `block`
    /// surface; before `text` was added to the kj tool schema, kaish would
    /// bind `--text` as a bool and divorce the body, and clap would then
    /// reject the leftover positional. Asserts the appended text landed.
    #[tokio::test]
    async fn block_append_text_flag_binds_space_form() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();
        let principal = PrincipalId::system();
        let ctx = register_context(&dispatcher, Some("append-ctx"), None, principal);
        dispatcher
            .block_store()
            .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
            .expect("create_document");
        let bid = dispatcher
            .block_store()
            .insert_block(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "seed ",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .expect("insert block");

        let kaish = embedded_with_kj(dispatcher.clone(), ctx).await;
        let script = format!("kj block append {} --text appended", bid.to_key());
        let res = kaish
            .execute_with_options(&script, ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "block append exit != 0: {res:?}");

        let snap = dispatcher
            .block_store()
            .get_block_snapshot(ctx, &bid)
            .expect("block store ok")
            .expect("block exists");
        assert!(
            snap.content.contains("appended"),
            "space-form `--text appended` must reach block append, got: {:?}",
            snap.content
        );
    }

    /// The other half of the headline guarantee: the keys `kj block list`
    /// emits round-trip through `kj block inspect` without manual munging.
    /// Before fix: `block list` returned `to_key()` (delimiter `_`) but
    /// `block inspect` parsed `split(':')` and rejected every iteration as
    /// "malformed id". This test runs the full loop and asserts an inspect
    /// record came back per block.
    #[tokio::test]
    async fn for_loop_block_list_to_inspect_round_trips() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("inspect-ctx"), None, principal);
        dispatcher
            .block_store()
            .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
            .expect("create_document");

        // Two blocks so iteration is non-trivial.
        let b1 = dispatcher
            .block_store()
            .insert_block(
                ctx,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "alpha body",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .expect("insert block 1");
        let b2 = dispatcher
            .block_store()
            .insert_block(
                ctx,
                None,
                None,
                kaijutsu_types::Role::Model,
                kaijutsu_types::BlockKind::Text,
                "beta body",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .expect("insert block 2");

        let kaish = embedded_with_kj(dispatcher, ctx).await;

        let script = "for b in $(kj block list); do echo \"---\"; kj block inspect $b; done";
        let res = kaish
            .execute_with_options(script, ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "round-trip exit != 0: {res:?}");

        let stdout = res.text_out();
        assert!(
            !stdout.contains("malformed"),
            "block inspect rejected an emitted key: {stdout}"
        );
        // Each block must show up once in the inspect output (we used
        // `id:` as the leading line in plain-text inspect render).
        assert!(
            stdout.contains(&b1.to_key()),
            "missing block 1 ({}) in stdout: {stdout}",
            b1.to_key()
        );
        assert!(
            stdout.contains(&b2.to_key()),
            "missing block 2 ({}) in stdout: {stdout}",
            b2.to_key()
        );
        // And we should have crossed the `---` separator twice.
        assert_eq!(
            stdout.matches("---").count(),
            2,
            "expected two iterations, got: {stdout}"
        );
    }

    /// End-to-end round-trip: emit handles from `kj context list`, feed
    /// each into `kj context info`. The fix is that unlabeled contexts
    /// now emit *full hex* (not short prefix), so `kj context info` —
    /// which resolves via prefix-match in the DB — accepts handles
    /// emitted by `list` even when no label is set.
    #[tokio::test]
    async fn for_loop_context_list_to_info_round_trips_with_full_hex() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        // One labeled, one unlabeled — both must round-trip.
        let labeled = register_context(&dispatcher, Some("gamma"), None, principal);
        let unlabeled = register_context(&dispatcher, None, None, principal);

        let kaish = embedded_with_kj(dispatcher, labeled).await;

        let script = "for c in $(kj context list); do echo \"---\"; kj context info $c; done";
        let res = kaish
            .execute_with_options(script, ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "round-trip exit != 0: {res:?}");

        let stdout = res.text_out();
        // Two contexts → two `---` separators, no `not found` errors.
        assert!(
            !stdout.contains("not found"),
            "context info rejected a handle: {stdout}"
        );
        assert_eq!(
            stdout.matches("---").count(),
            2,
            "expected 2 iterations: {stdout}"
        );
        // `format_context_info` renders "ID: <short>"; both contexts'
        // short ids should appear (independent of whether the handle
        // was the label or the full hex).
        assert!(
            stdout.contains(&labeled.short()),
            "labeled context short id missing: {stdout}"
        );
        assert!(
            stdout.contains(&unlabeled.short()),
            "unlabeled context short id missing: {stdout}"
        );
    }

    /// `kj block count` returns a scalar (number, not array). It must
    /// still set `.data` so `kaish-last`-style consumers see the value,
    /// but the for-loop case won't iterate. This guards the scalar path
    /// in `KjBuiltin::execute`'s data conversion.
    #[tokio::test]
    async fn scalar_data_flows_into_exec_result() {
        use kaish_kernel::ast::Value;

        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let ctx = {
            let c = register_context(&dispatcher, Some("solo"), None, principal);
            dispatcher
                .block_store()
                .create_document(c, kaijutsu_types::DocKind::Conversation, None)
                .expect("create_document");
            c
        };

        let kaish = embedded_with_kj(dispatcher, ctx).await;

        let res = kaish
            .execute_with_options("kj block count", ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "kj block count exit != 0: {res:?}");
        assert_eq!(
            res.data,
            Some(Value::Int(0)),
            "expected scalar Int(0) in .data, got {:?}",
            res.data,
        );
    }

    /// Piped stdin populates `--content` for `kj rc add` when the flag is
    /// omitted. Without the injection in `KjBuiltin::execute`, `rc add`
    /// would error with "missing content" and the documented
    /// `echo … | kj rc add …` example would never have worked.
    #[tokio::test]
    async fn pipe_stdin_provides_rc_add_content() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("stdinhost"), None, principal);
        let kaish = embedded_with_kj(dispatcher, ctx).await;

        let script = r#"
            echo 'hello from pipe' | kj rc add /etc/rc/stditest/create/S00-from-pipe.kai
            kj rc show /etc/rc/stditest/create/S00-from-pipe.kai
        "#;
        let res = kaish
            .execute_with_options(script, ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "pipe-into-rc-add exit != 0: {res:?}");

        let stdout = res.text_out();
        assert!(
            stdout.contains("hello from pipe"),
            "stdin content didn't reach the script: {stdout}"
        );
        // Make sure the old "missing --content" error didn't leak through.
        assert!(
            !stdout.contains("missing content")
                && !stdout.contains("missing --content"),
            "rc add still reported missing content despite piped stdin: {stdout}"
        );
    }

    /// Explicit `--content` wins over stdin: pipe + flag → flag's value
    /// ends up persisted. Guards the precedence promised by the help text.
    #[tokio::test]
    async fn explicit_content_wins_over_stdin() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("stdinhost2"), None, principal);
        let kaish = embedded_with_kj(dispatcher, ctx).await;

        let script = r#"
            echo 'from stdin' | kj rc add /etc/rc/stditest2/create/S00-flag-wins.kai --content 'from flag'
            kj rc show /etc/rc/stditest2/create/S00-flag-wins.kai
        "#;
        let res = kaish
            .execute_with_options(script, ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(res.ok(), "flag-wins exit != 0: {res:?}");

        let stdout = res.text_out();
        assert!(
            stdout.contains("from flag"),
            "explicit --content didn't win: {stdout}"
        );
        assert!(
            !stdout.contains("from stdin"),
            "stdin leaked when --content was provided: {stdout}"
        );
    }

    /// Latch regression: a confirmation nonce issued by one `EmbeddedKaish`
    /// must still validate when the *next* shell confirms it. kaish is
    /// materialized fresh per MCP `execute`, so the nonce store can't live on
    /// the shell — it lives per-context on the kernel. Before the fix, the
    /// `--confirm` landed in a brand-new empty store and the kernel reported
    /// "invalid nonce", exactly the `kj context retag … --confirm <nonce>`
    /// failure observed in the app.
    #[tokio::test]
    async fn latch_nonce_survives_fresh_shell_for_same_context() {
        let dispatcher = Arc::new(test_dispatcher().await);
        dispatcher.set_self_arc();

        let principal = PrincipalId::new();
        let ctx = register_context(&dispatcher, Some("alpha"), None, principal);

        // Shell #1: issue the latch nonce (no --confirm yet → exit 2).
        let kaish_a = embedded_with_kj(dispatcher.clone(), ctx).await;
        let issue = kaish_a
            .execute_with_options(
                "kj context retag beta alpha",
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec (issue)");
        assert_eq!(
            issue.code, 2,
            "retag without --confirm should latch (exit 2): {issue:?}"
        );

        // Pull the nonce out of the confirmation hint kaish emitted, e.g.
        // "...To confirm, run: kj context retag beta alpha --confirm 1a2b3c4d".
        let hint = &issue.err;
        let nonce = hint
            .split("--confirm ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("no --confirm nonce in latch message: {hint}"));

        // Shell #2: a *fresh* materialization for the same context — the real
        // path a follow-up MCP `execute` takes. The nonce must still validate.
        let kaish_b = embedded_with_kj(dispatcher.clone(), ctx).await;
        let confirm = kaish_b
            .execute_with_options(
                &format!("kj context retag beta alpha --confirm {nonce}"),
                ExecuteOptions::default(),
            )
            .await
            .expect("kaish exec (confirm)");

        assert!(
            !confirm.err.contains("invalid nonce"),
            "nonce was lost between fresh shells: {}",
            confirm.err
        );
        assert!(
            confirm.ok(),
            "confirm in a fresh shell should succeed, got code {} / err {:?}",
            confirm.code,
            confirm.err
        );
    }

    /// `is_distill_verb` must classify exactly the four in-line-LLM verbs and
    /// nothing else. A false positive would freeze the script watchdog for a
    /// fast command (e.g. a tight loop never trips its timeout); a false
    /// negative silently forfeits the patient hold. Mirrors the dispatcher's
    /// routing, so it fails loudly if the two drift apart.
    #[test]
    fn is_distill_verb_classifies_inline_llm_verbs() {
        let argv = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();

        // Positives: the only verbs that block on `summarize`/`prompt_with_system`.
        assert!(is_distill_verb(&argv("fork --compact")));
        assert!(is_distill_verb(&argv("fork --name x --compact")));
        assert!(is_distill_verb(&argv("drift pull main what-changed")));
        assert!(is_distill_verb(&argv("drift merge")));
        assert!(is_distill_verb(&argv("drift merge some-ctx")));
        assert!(is_distill_verb(&argv("drift push main --summarize")));

        // Negatives: a plain fork copies (no distill); `drift push` without
        // `--summarize` stages literal content; `drift flush`/`push` deliver;
        // `drive` only publishes a turn request; everything else is local.
        assert!(!is_distill_verb(&argv("fork --name x")));
        assert!(!is_distill_verb(&argv("drift push main some literal content")));
        assert!(!is_distill_verb(&argv("drift flush")));
        assert!(!is_distill_verb(&argv("drift queue")));
        assert!(!is_distill_verb(&argv("drive")));
        assert!(!is_distill_verb(&argv("drive --prompt go")));
        assert!(!is_distill_verb(&argv("block list")));
        assert!(!is_distill_verb(&argv("context list")));
        assert!(!is_distill_verb(&[]));
    }

    /// The headline guarantee of the `patient` adoption: a distill verb whose
    /// LLM completion runs *longer than* the script-level `kaish_request_timeout`
    /// still completes, because `ctx.patient(llm_request_timeout)` freezes the
    /// script clock for the hold. Delete the `ctx.patient(...)` line in
    /// `execute` and this test fails (the slow distill trips the watchdog → the
    /// command errors). The control half proves the watchdog is genuinely armed
    /// and tight in this harness, so the positive half isn't passing vacuously.
    #[tokio::test]
    async fn slow_distill_survives_tight_request_timeout() {
        use crate::llm::{MockClient, Provider};
        use std::time::Duration;

        // A tight 300ms script watchdog, but a generous 10s per-LLM budget —
        // exactly the split `patient` exists to honor.
        let policy = kaijutsu_types::TimeoutPolicy {
            kaish_request_timeout: Duration::from_millis(300),
            llm_request_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let dispatcher = Arc::new(
            crate::kj::test_helpers::test_dispatcher_with_timeouts(policy).await,
        );
        dispatcher.set_self_arc();

        // A provider that takes 800ms to answer — past the 300ms watchdog, well
        // under the 10s LLM budget. summarize() falls to the registry default
        // when the context has no model of its own.
        {
            let mut reg = dispatcher.kernel().llm().write().await;
            reg.register(
                "mock",
                Arc::new(Provider::Mock(
                    MockClient::new("distilled summary").with_delay(Duration::from_millis(800)),
                )),
            );
            assert!(reg.set_default("mock"), "set default provider");
            reg.set_default_model("mock-model");
        }

        let principal = PrincipalId::system();
        let here = register_context(&dispatcher, Some("here"), None, principal);
        register_context(&dispatcher, Some("sink"), None, principal);
        // summarize needs blocks to distill — seed one.
        dispatcher
            .block_store()
            .create_document(here, kaijutsu_types::DocKind::Conversation, None)
            .expect("create_document");
        dispatcher
            .block_store()
            .insert_block(
                here,
                None,
                None,
                kaijutsu_types::Role::User,
                kaijutsu_types::BlockKind::Text,
                "something worth distilling",
                kaijutsu_types::Status::Done,
                kaijutsu_types::ContentType::Plain,
            )
            .expect("insert block");

        let kaish = embedded_with_kj(dispatcher.clone(), here).await;

        // Control: a plain shell sleep well past the 300ms watchdog is NOT
        // wrapped in patient, so it must be interrupted (cancelled fast, nonzero
        // exit) — proving the watchdog is live and tight in this harness.
        let control = kaish
            .execute_with_options("sleep 5", ExecuteOptions::default())
            .await
            .expect("kaish exec (control sleep)");
        assert!(
            !control.ok(),
            "control sleep 5 under a 300ms watchdog must be interrupted, got {control:?}"
        );

        // Headline: the 800ms distill outlasts the same 300ms watchdog but still
        // completes, because patient froze the script clock for the LLM hold.
        let res = kaish
            .execute_with_options("kj drift push sink --summarize", ExecuteOptions::default())
            .await
            .expect("kaish exec (distill)");
        assert!(
            res.ok(),
            "slow distill should survive the tight watchdog under patient, got code {} / {:?}",
            res.code,
            res.text_out()
        );
        assert_ne!(res.code, 124, "distill must not trip the script timeout (124)");
        assert!(
            res.text_out().contains("staged drift"),
            "distill should have staged the summary: {}",
            res.text_out()
        );
    }

    /// Regression guard for the outer-help-router papercut: kaish's
    /// `dispatch_command` intercepts `--help` (rendering the generic whole-tool
    /// help and skipping the tool entirely) unless the tool's **root** schema
    /// claims the `help` flag — `schema_claims("help")` in kaish-kernel's
    /// kernel.rs. kj owns its own output and re-parses its argv with clap, so
    /// it wants `--help` routed through to its dispatch (where the leaf clap
    /// parser renders subcommand help). That only happens if the reflected root
    /// schema advertises `help`. This asserts the contract kaish relies on,
    /// using the same `matches_flag` predicate kaish does.
    #[tokio::test]
    async fn root_schema_claims_help_flag_so_kaish_routes_it() {
        let dispatcher = Arc::new(test_dispatcher().await);
        let kj = KjBuiltin::new(
            dispatcher,
            session_context_map(),
            PrincipalId::system(),
            SessionId::new(),
            None,
            Arc::new(crate::kj::lifecycle::NoopBlockSource),
            false,
        );
        let schema = kj.schema();
        // Mirror kaish-kernel's `schema_claims("help")`: a root param whose
        // name or alias matches the bare flag. If this fails, kaish's outer
        // help router will swallow `kj <verb> --help` and show top-level help.
        assert!(
            schema.params.iter().any(|p| p.matches_flag("help")),
            "kj root schema must claim `help` so kaish routes --help to the leaf \
             instead of intercepting it; root params = {:?}",
            schema.params.iter().map(|p| &p.name).collect::<Vec<_>>(),
        );
    }
}

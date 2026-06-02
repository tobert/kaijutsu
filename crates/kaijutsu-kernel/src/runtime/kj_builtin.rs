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
use kaish_kernel::tools::{ParamSchema, ToolArgs, ToolSchema};
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
}

impl KjBuiltin {
    pub fn new(
        dispatcher: Arc<KjDispatcher>,
        session_contexts: SessionContextMap,
        principal_id: PrincipalId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Self {
        Self {
            dispatcher,
            session_contexts,
            principal_id,
            session_id,
            semantic_index,
            block_source,
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
        ToolSchema::new("kj", "Kernel command interface. Run `kj help` or `kj <command> help` for detailed workflows.")
            .param(ParamSchema::required("subcommand", "string", "Command and arguments (e.g. 'drift push main \"finding\"')"))
            // Schema-declared named params so kaish puts them in `named` (not `flags`)
            .param(ParamSchema::optional("name", "string", Value::String(String::new()), "Label for context or fork")
                .with_aliases(["-n"]))
            .param(ParamSchema::optional("depth", "int", Value::Int(50), "Depth limit for shallow fork"))
            .param(ParamSchema::optional("prompt", "string", Value::String(String::new()), "Prompt note to inject on fork"))
            .param(ParamSchema::optional("preset", "string", Value::String(String::new()), "Preset to apply on fork"))
            .param(ParamSchema::optional("model", "string", Value::String(String::new()), "Model spec (provider/model)"))
            .param(ParamSchema::optional("tools", "string", Value::String(String::new()), "Tool filter spec"))
            .param(ParamSchema::optional("confirm", "string", Value::String(String::new()), "Latch confirmation nonce"))
            .param(ParamSchema::optional("as", "string", Value::String(String::new()), "Template context for subtree fork"))
            // Declared so kaish parses `--context main` / `--kind text` etc. as
            // named args (consuming the next positional) instead of defaulting
            // to bool flags. Used by `kj block` subcommands.
            .param(ParamSchema::optional("context", "string", Value::String(String::new()), "Target context ref (label, hex, .parent)")
                .with_aliases(["-c"]))
            .param(ParamSchema::optional("kind", "string", Value::String(String::new()), "Block kind filter"))
            .param(ParamSchema::optional("role", "string", Value::String(String::new()), "Block role filter"))
            .param(ParamSchema::optional("status", "string", Value::String(String::new()), "Block status filter"))
            // `kj rc add --timeout <secs>` — per-script wall-clock budget
            // for .kai execution; omit to inherit the kernel default.
            .param(ParamSchema::optional("timeout", "string", Value::String(String::new()), "Per-script wall-clock budget in seconds (kj rc add)"))
            // `kj rc add|edit --content <body>` — script body. Declared so
            // kaish treats `--content "multi\nline"` as a named arg
            // (consuming the next positional) rather than a bool flag.
            .param(ParamSchema::optional("content", "string", Value::String(String::new()), "Script body (kj rc add / edit). Pipe stdin to omit."))
            .example("Discover commands", "kj help")
            .example("View context topology", "kj context list --tree")
            .example("Create isolated workspace", "kj fork --name debug-auth")
            .example("Navigate to context", "kj context switch debug-auth")
            .example("Stage finding for another context", "kj drift push main \"auth tokens are stored in Redis\"")
            .example("Deliver all staged drifts", "kj drift flush")
            .example("LLM-distill another context's work", "kj drift pull main \"what changed in auth?\"")
            .example("Distill from parent context", "kj drift pull .parent \"summarize findings\"")
            .example("Merge fork back to parent", "kj drift merge")
            .example("Set model on current context", "kj context set . --model anthropic:claude-sonnet-4-5-20250929")
            .example("Learn drift workflows", "kj drift help")
            .example("Bulk synthesize keywords", "kj synth all")
    }

    async fn execute(&self, args: ToolArgs, ctx: &mut ExecContext) -> ExecResult {
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

        let result = self.dispatcher.dispatch(&argv, &caller).await;

        match result {
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
        }
    }
}

/// Build a command scope string from argv for nonce validation.
///
/// e.g., `["context", "archive", "old-ctx"]` → `"kj context archive"`
fn build_command_scope(argv: &[String]) -> String {
    let parts: Vec<&str> = argv
        .iter()
        .map(|s| s.as_str())
        .take_while(|s| !s.starts_with('-'))
        .take(2) // At most: subcommand + verb
        .collect();
    format!("kj {}", parts.join(" "))
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
}

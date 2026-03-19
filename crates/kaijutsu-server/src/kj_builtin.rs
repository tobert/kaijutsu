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

use kaijutsu_kernel::kj::{KjCaller, KjDispatcher, KjResult};
#[allow(unused_imports)]
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use crate::kaish_backend::SharedContextId;

/// kaish builtin tool for the `kj` command.
///
/// Bridges kaish's `Tool` trait to `KjDispatcher`. Each connection gets its own
/// `KjBuiltin` with the shared dispatcher and per-connection identity fields.
pub struct KjBuiltin {
    dispatcher: Arc<KjDispatcher>,
    shared_context_id: SharedContextId,
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
        shared_context_id: SharedContextId,
        principal_id: PrincipalId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Self {
        Self {
            dispatcher,
            shared_context_id,
            principal_id,
            session_id,
            semantic_index,
            block_source,
        }
    }

    fn current_context_id(&self) -> ContextId {
        *self
            .shared_context_id
            .read()
            .expect("context_id lock poisoned")
    }

    fn set_context_id(&self, id: ContextId) {
        *self
            .shared_context_id
            .write()
            .expect("context_id lock poisoned") = id;
    }

    /// Load context shell config (cwd + env vars) from KernelDb and apply to ExecContext.
    fn apply_context_config(&self, context_id: ContextId, ctx: &mut ExecContext) {
        let db = self.dispatcher.kernel_db().lock();

        // Apply cwd
        if let Ok(Some(shell)) = db.get_context_shell(context_id)
            && let Some(cwd) = shell.cwd
        {
            let path = std::path::PathBuf::from(&cwd);
            if path.is_dir() {
                ctx.set_cwd(path);
            } else {
                tracing::warn!("context cwd '{}' is not a directory, skipping", cwd);
            }
        }

        // Apply env vars (exported so they propagate to child processes)
        if let Ok(vars) = db.get_context_env(context_id) {
            for var in &vars {
                ctx.scope
                    .set_exported(&var.key, Value::String(var.value.clone()));
            }
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
                "semantic index not configured (check embedding model in models.rhai)".to_string(),
            );
        };

        let kernel_id = self.dispatcher.kernel_id();
        let contexts = {
            let db = self.dispatcher.kernel_db().lock();
            match db.list_active_contexts(kernel_id) {
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
                    crate::synthesis_rhai::run_synthesis(ctx_id, idx.embedder_arc(), block_source);

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
                "semantic index not configured (check embedding model in models.rhai)".to_string(),
            );
        };

        // Resolve context reference
        let parsed = kaijutsu_kernel::kj::refs::parse_context_ref(ctx_ref);
        let kernel_id = self.dispatcher.kernel_id();
        let ctx_id = {
            let db = self.dispatcher.kernel_db().lock();
            match kaijutsu_kernel::kj::refs::resolve_context_ref(&parsed, caller, &db, kernel_id) {
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
                crate::synthesis_rhai::run_synthesis(ctx_id, idx.embedder_arc(), block_source);

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
                "semantic index: not configured\n(set embedding model in models.rhai)".to_string(),
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
        let confirm_nonce = kaijutsu_kernel::kj::parse::extract_named_arg(&argv, &["--confirm"]);
        kaijutsu_kernel::kj::parse::strip_named_arg(&mut argv, &["--confirm"]);

        let mut caller = KjCaller {
            principal_id: self.principal_id,
            context_id: self.current_context_id(),
            session_id: self.session_id,
            confirmed: false,
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
            } => {
                let mut result = ExecResult::success(message);
                result.content_type = content_type;
                if ephemeral {
                    result
                        .baggage
                        .insert("kaijutsu.ephemeral".into(), "true".into());
                }
                result
            }
            KjResult::Err(msg) => ExecResult::failure(1, msg),
            KjResult::Switch(new_id, msg) => {
                // Side-effect: update the shared context ID
                self.set_context_id(new_id);

                // Apply context shell config (cwd + env vars)
                self.apply_context_config(new_id, ctx);

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

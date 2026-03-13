//! `kj` kaish builtin — routes argv through `KjDispatcher`.
//!
//! Registered as a kaish Tool in EmbeddedKaish via the `configure_tools` callback.
//! Each connection gets its own `KjBuiltin` instance with the shared dispatcher
//! plus per-connection identity.

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
}

impl KjBuiltin {
    pub fn new(
        dispatcher: Arc<KjDispatcher>,
        shared_context_id: SharedContextId,
        principal_id: PrincipalId,
        session_id: SessionId,
    ) -> Self {
        Self {
            dispatcher,
            shared_context_id,
            principal_id,
            session_id,
        }
    }

    fn current_context_id(&self) -> ContextId {
        *self.shared_context_id.read().expect("context_id lock poisoned")
    }

    fn set_context_id(&self, id: ContextId) {
        *self.shared_context_id.write().expect("context_id lock poisoned") = id;
    }

    /// Load context shell config (cwd + env vars) from KernelDb and apply to ExecContext.
    fn apply_context_config(&self, context_id: ContextId, ctx: &mut ExecContext) {
        let db = self.dispatcher.kernel_db().lock().unwrap();

        // Apply cwd
        if let Ok(Some(shell)) = db.get_context_shell(context_id) {
            if let Some(cwd) = shell.cwd {
                let path = std::path::PathBuf::from(&cwd);
                if path.is_dir() {
                    ctx.set_cwd(path);
                } else {
                    tracing::warn!("context cwd '{}' is not a directory, skipping", cwd);
                }
            }
        }

        // Apply env vars (exported so they propagate to child processes)
        if let Ok(vars) = db.get_context_env(context_id) {
            for var in &vars {
                ctx.scope.set_exported(&var.key, Value::String(var.value.clone()));
            }
        }
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

        let result = self.dispatcher.dispatch(&argv, &caller).await;

        match result {
            KjResult::Ok(msg) => ExecResult::success(msg),
            KjResult::Err(msg) => ExecResult::failure(1, msg),
            KjResult::Switch(new_id, msg) => {
                // Side-effect: update the shared context ID
                self.set_context_id(new_id);

                // Apply context shell config (cwd + env vars)
                self.apply_context_config(new_id, ctx);

                ExecResult::success(msg)
            }
            KjResult::Latch { command, target, message } => {
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

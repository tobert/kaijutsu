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
}

#[async_trait]
impl Tool for KjBuiltin {
    fn name(&self) -> &str {
        "kj"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new("kj", "Kernel command interface. Run `kj help` or `kj <command> help` for detailed workflows.")
            .param(ParamSchema::required("subcommand", "string", "Command and arguments (e.g. 'drift push main \"finding\"')"))
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
        // Build argv from positional args
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

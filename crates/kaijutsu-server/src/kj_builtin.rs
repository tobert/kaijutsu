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
        ToolSchema::new("kj", "Kernel command interface — context, fork, drift, preset, workspace management")
            .param(ParamSchema::required("subcommand", "string", "Command: context, fork, drift, preset, workspace"))
    }

    async fn execute(&self, args: ToolArgs, _ctx: &mut ExecContext) -> ExecResult {
        // Build argv from positional args
        let argv: Vec<String> = args
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

        let caller = KjCaller {
            principal_id: self.principal_id,
            context_id: self.current_context_id(),
            session_id: self.session_id,
        };

        let result = self.dispatcher.dispatch(&argv, &caller).await;

        match result {
            KjResult::Ok(msg) => ExecResult::success(msg),
            KjResult::Err(msg) => ExecResult::failure(1, msg),
            KjResult::Switch(new_id, msg) => {
                // Side-effect: update the shared context ID
                self.set_context_id(new_id);
                ExecResult::success(msg)
            }
        }
    }
}

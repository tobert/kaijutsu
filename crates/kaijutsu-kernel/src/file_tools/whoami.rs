//! WhoamiEngine — returns current context identity from the drift router.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::drift::DriftRouter;
use crate::tools::{ExecResult, ExecutionEngine, ToolContext};

/// Engine that returns the current context's identity.
pub struct WhoamiEngine {
    drift_router: Arc<RwLock<DriftRouter>>,
}

impl WhoamiEngine {
    pub fn new(drift_router: Arc<RwLock<DriftRouter>>) -> Self {
        Self { drift_router }
    }
}

#[async_trait]
impl ExecutionEngine for WhoamiEngine {
    fn name(&self) -> &str {
        "whoami"
    }

    fn description(&self) -> &str {
        "Show current context identity: ID, label, model, parent"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "Returns the current context identity (no parameters needed)"
        }))
    }

    #[tracing::instrument(skip(self, _params, ctx), name = "engine.whoami")]
    async fn execute(&self, _params: &str, ctx: &ToolContext) -> anyhow::Result<ExecResult> {
        let router = self.drift_router.read().await;

        let handle = router.get(ctx.context_id);

        let info = serde_json::json!({
            "context_id": ctx.context_id.to_hex(),
            "context_id_short": ctx.context_id.short(),
            "label": handle.and_then(|h| h.label.as_deref()),
            "model": handle.and_then(|h| h.model.as_deref()),
            "provider": handle.and_then(|h| h.provider.as_deref()),
            "forked_from": handle.and_then(|h| h.forked_from.map(|p| p.short())),
        });

        Ok(ExecResult::success(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        ))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

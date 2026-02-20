//! WhoamiEngine — returns current context identity from the drift router.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use kaijutsu_crdt::ContextId;

use crate::drift::DriftRouter;
use crate::tools::{ExecResult, ExecutionEngine};

/// Engine that returns the current context's identity.
pub struct WhoamiEngine {
    drift_router: Arc<RwLock<DriftRouter>>,
    context_id: ContextId,
}

impl WhoamiEngine {
    pub fn new(drift_router: Arc<RwLock<DriftRouter>>, context_id: ContextId) -> Self {
        Self {
            drift_router,
            context_id,
        }
    }
}

#[async_trait]
impl ExecutionEngine for WhoamiEngine {
    fn name(&self) -> &str {
        "whoami"
    }

    fn description(&self) -> &str {
        "Show current context identity: ID, label, model, parent, document"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "Returns the current context identity (no parameters needed)"
        }))
    }

    #[tracing::instrument(skip(self, _params), name = "engine.whoami")]
    async fn execute(&self, _params: &str) -> anyhow::Result<ExecResult> {
        let router = self.drift_router.read().await;

        let handle = router.get(self.context_id);

        let info = serde_json::json!({
            "context_id": self.context_id.to_hex(),
            "context_id_short": self.context_id.short(),
            "label": handle.and_then(|h| h.label.as_deref()),
            "document_id": handle.map(|h| h.document_id.as_str()),
            "model": handle.and_then(|h| h.model.as_deref()),
            "provider": handle.and_then(|h| h.provider.as_deref()),
            "parent_id": handle.and_then(|h| h.parent_id.map(|p| p.short())),
        });

        Ok(ExecResult::success(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        ))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

//! WhoamiEngine — returns current context identity from the drift router.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::drift::DriftRouter;
use crate::tools::{ExecResult, ExecutionEngine};

/// Engine that returns the current context's identity.
pub struct WhoamiEngine {
    drift_router: Arc<RwLock<DriftRouter>>,
    context_name: String,
}

impl WhoamiEngine {
    pub fn new(drift_router: Arc<RwLock<DriftRouter>>, context_name: impl Into<String>) -> Self {
        Self {
            drift_router,
            context_name: context_name.into(),
        }
    }
}

#[async_trait]
impl ExecutionEngine for WhoamiEngine {
    fn name(&self) -> &str {
        "whoami"
    }

    fn description(&self) -> &str {
        "Show current context identity: short ID, name, model, parent, document"
    }

    fn schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {},
            "description": "Returns the current context identity (no parameters needed)"
        }))
    }

    async fn execute(&self, _params: &str) -> anyhow::Result<ExecResult> {
        let router = self.drift_router.read().await;

        let short_id = router
            .short_id_for_context(&self.context_name)
            .unwrap_or("(unregistered)");

        let handle = router
            .short_id_for_context(&self.context_name)
            .and_then(|sid| router.get(sid));

        let info = serde_json::json!({
            "context_name": self.context_name,
            "short_id": short_id,
            "document_id": handle.map(|h| h.document_id.as_str()),
            "model": handle.and_then(|h| h.model.as_deref()),
            "provider": handle.and_then(|h| h.provider.as_deref()),
            "parent": handle.and_then(|h| h.parent_short_id.as_deref()),
        });

        Ok(ExecResult::success(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        ))
    }

    async fn is_available(&self) -> bool {
        true
    }
}

//! WhoamiEngine — returns current context identity from the drift router.

use anyhow::Context as _;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;

use crate::drift::DriftRouter;
use crate::execution::{ExecContext, ExecResult};
use crate::kernel_db::KernelDb;
use crate::kj::format::hex32;

/// Engine that returns the current context's identity.
pub struct WhoamiEngine {
    drift_router: Arc<RwLock<DriftRouter>>,
    /// Kernel DB, used to surface persisted fields (e.g. `context_type`) that
    /// don't live on the in-memory drift handle. Required — a kernel can't run
    /// without one, so making it optional would only mask a wiring bug.
    kernel_db: Arc<Mutex<KernelDb>>,
}

impl WhoamiEngine {
    pub fn new(drift_router: Arc<RwLock<DriftRouter>>, kernel_db: Arc<Mutex<KernelDb>>) -> Self {
        Self {
            drift_router,
            kernel_db,
        }
    }

    pub fn description(&self) -> &str {
        "Show current context identity: ID, label, model, type, trace, parent"
    }

    #[tracing::instrument(skip(self, _params, ctx), name = "engine.whoami")]
    pub async fn execute(
        &self,
        _params: &str,
        ctx: &ExecContext,
    ) -> anyhow::Result<ExecResult> {
        let router = self.drift_router.read();

        let handle = router.get(ctx.context_id);

        // context_type is persisted on the row, not the handle. Propagate DB
        // errors rather than swallowing them into a misleading "no row".
        let row = self
            .kernel_db
            .lock()
            .get_context(ctx.context_id)
            .context("whoami: failed to read context row")?;

        // Invariant: every registered context (handle present) is written to the
        // DB before it is registered, so a handle with no row means state
        // corruption — fail loudly rather than report a half-identity. (The
        // lost+found sink is persisted like any other context, so it's no
        // exception.)
        if handle.is_some() && row.is_none() {
            let id = ctx.context_id.to_hex();
            tracing::error!(
                context_id = %id,
                "whoami: drift handle exists with no context row — invariant violated"
            );
            anyhow::bail!(
                "whoami: context {id} has a drift handle but no persisted row (state corruption)"
            );
        }

        let context_type = row.map(|row| row.context_type);

        let info = serde_json::json!({
            "context_id": ctx.context_id.to_hex(),
            "context_id_short": ctx.context_id.short(),
            "label": handle.and_then(|h| h.label.as_deref()),
            "model": handle.and_then(|h| h.model.as_deref()),
            "provider": handle.and_then(|h| h.provider.as_deref()),
            "context_type": context_type,
            "trace_id": handle.map(|h| hex32(h.trace_id)),
            "forked_from": handle.and_then(|h| h.forked_from.map(|p| p.short())),
        });

        Ok(ExecResult::success(
            serde_json::to_string_pretty(&info).unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::shared_drift_router;
    use crate::kernel_db::{ContextRow, DocumentRow, KernelDb};
    use kaijutsu_types::{ConsentMode, ContextId, ContextState, DocKind, PrincipalId};

    fn ctx_row(id: ContextId, context_type: &str) -> ContextRow {
        ContextRow {
            context_id: id,
            label: Some("ctx".to_string()),
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            context_type: context_type.to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
        }
    }

    fn exec_ctx(id: ContextId) -> ExecContext {
        ExecContext {
            context_id: id,
            ..ExecContext::test()
        }
    }

    /// Seed a persisted context row of the given type. A context row extends a
    /// document row (PK FK), so the document + workspace are seeded first.
    fn seed_context(db: &Mutex<KernelDb>, id: ContextId, context_type: &str) {
        let g = db.lock();
        let creator = PrincipalId::new();
        let ws_id = g.get_or_create_default_workspace(creator).unwrap();
        g.insert_document(&DocumentRow {
            document_id: id,
            workspace_id: ws_id,
            doc_kind: DocKind::Conversation,
            language: None,
            path: None,
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: creator,
        })
        .unwrap();
        g.insert_context(&ctx_row(id, context_type)).unwrap();
    }

    #[tokio::test]
    async fn whoami_surfaces_context_type_and_trace_id() {
        let router = shared_drift_router();
        let id = ContextId::new();
        router
            .write()
            .register(id, Some("ctx"), None, PrincipalId::new())
            .unwrap();

        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        seed_context(&db, id, "coder");

        let engine = WhoamiEngine::new(router, db);
        let result = engine.execute("", &exec_ctx(id)).await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(json["context_type"], "coder");
        let trace = json["trace_id"].as_str().expect("trace_id present");
        assert_eq!(trace.len(), 32);
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// context_type comes from the DB row; label comes from the drift handle.
    /// A handle with no label must still surface the persisted context_type.
    #[tokio::test]
    async fn whoami_context_type_is_independent_of_handle_label() {
        let router = shared_drift_router();
        let id = ContextId::new();
        router
            .write()
            .register(id, None, None, PrincipalId::new())
            .unwrap();

        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        seed_context(&db, id, "planner");

        let engine = WhoamiEngine::new(router, db);
        let result = engine.execute("", &exec_ctx(id)).await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();

        assert_eq!(json["context_type"], "planner");
        assert!(json["label"].is_null());
        assert!(json["trace_id"].is_string());
    }

    /// A registered handle with no persisted row is state corruption: whoami
    /// must fail loudly, not report a half-identity with a null context_type.
    #[tokio::test]
    async fn whoami_errors_when_handle_has_no_row() {
        let router = shared_drift_router();
        let id = ContextId::new();
        router
            .write()
            .register(id, Some("ghost"), None, PrincipalId::new())
            .unwrap();

        // DB has no row for `id`.
        let db = Arc::new(Mutex::new(KernelDb::in_memory().unwrap()));
        let engine = WhoamiEngine::new(router, db);

        let err = engine
            .execute("", &exec_ctx(id))
            .await
            .expect_err("handle without row must be fatal");
        assert!(err.to_string().contains("no persisted row"));
    }
}

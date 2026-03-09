//! Fork subcommand: deep-copy the current context's document.

use kaijutsu_types::{ConsentMode, ContextId, EdgeKind, ForkKind};

use crate::kernel_db::{ContextEdgeRow, ContextRow};

use super::{KjCaller, KjDispatcher, KjResult};

impl KjDispatcher {
    pub(crate) async fn dispatch_fork(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.first().map(|a| a.as_str()) == Some("help")
            || argv.first().map(|a| a.as_str()) == Some("--help")
        {
            return KjResult::Ok(self.fork_help());
        }

        self.fork_full(argv, caller).await
    }

    fn fork_help(&self) -> String {
        "\
kj fork — fork the current context

USAGE:
    kj fork [--name <label>] [--prompt \"...\"]

OPTIONS:
    --name, -n <label>    Label for the new context
    --prompt \"...\"        Inject a drift block with a fork note"
            .to_string()
    }

    async fn fork_full(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        // Parse --name / -n
        let label = extract_named_arg(argv, &["--name", "-n"]);

        // Parse --prompt
        let prompt = extract_named_arg(argv, &["--prompt"]);

        let source_id = caller.context_id;
        let new_id = ContextId::new();
        let kernel_id = self.kernel_id();

        // Deep-copy the BlockStore document
        if let Err(e) = self.block_store().fork_document(source_id, new_id) {
            return KjResult::Err(format!("kj fork: failed to copy document: {e}"));
        }

        // Write-through: KernelDb then DriftRouter
        {
            let db = self.kernel_db().lock().unwrap();
            let row = ContextRow {
                context_id: new_id,
                kernel_id,
                label: label.clone(),
                provider: None,
                model: None,
                system_prompt: None,
                tool_filter: None,
                consent_mode: ConsentMode::Collaborative,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by: caller.principal_id,
                forked_from: Some(source_id),
                fork_kind: Some(ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
            };
            if let Err(e) = db.insert_context(&row) {
                return KjResult::Err(format!("kj fork: {e}"));
            }

            // Structural edge: source → new
            let edge = ContextEdgeRow {
                edge_id: uuid::Uuid::now_v7(),
                source_id,
                target_id: new_id,
                kind: EdgeKind::Structural,
                metadata: None,
                created_at: kaijutsu_types::now_millis() as i64,
            };
            if let Err(e) = db.insert_edge(&edge) {
                tracing::warn!("failed to insert fork edge: {e}");
            }
        }

        // Register in DriftRouter (inherits parent's model)
        {
            let mut drift = self.drift_router().write().await;
            drift.register_fork(new_id, label.as_deref(), source_id, caller.principal_id);
        }

        // If --prompt given, inject a Drift block
        if let Some(note) = prompt {
            if let Err(e) = self.inject_fork_note(new_id, caller, &note) {
                tracing::warn!("failed to inject fork note: {e}");
            }
        }

        let short = new_id.short();
        let display = label
            .as_deref()
            .unwrap_or(&short);
        KjResult::Ok(format!("forked to '{}' ({})", display, new_id.short()))
    }

    fn inject_fork_note(
        &self,
        target_id: ContextId,
        caller: &KjCaller,
        note: &str,
    ) -> Result<(), String> {
        use kaijutsu_crdt::DriftKind;

        let after = self.block_store().last_block_id(target_id);
        self.block_store()
            .insert_drift_block(
                target_id,
                None,
                after.as_ref(),
                note,
                caller.context_id,
                None,
                DriftKind::Push,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Extract a named argument value from argv (e.g., `--name foo`).
fn extract_named_arg(argv: &[String], names: &[&str]) -> Option<String> {
    for (i, arg) in argv.iter().enumerate() {
        if names.contains(&arg.as_str()) {
            return argv.get(i + 1).cloned();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use kaijutsu_types::PrincipalId;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn fork_basic() {
        let d = test_dispatcher();
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("source"), None, principal).await;

        // Create a document for the source context
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork"), s("--name"), s("branch")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(result.message().contains("branch"), "msg: {}", result.message());

        // Verify new context exists in DB
        let db = d.kernel_db().lock().unwrap();
        let contexts = db.list_active_contexts(d.kernel_id()).unwrap();
        assert!(contexts.iter().any(|r| r.label.as_deref() == Some("branch")));
    }

    #[tokio::test]
    async fn fork_no_name() {
        let d = test_dispatcher();
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d.dispatch(&[s("fork")], &c).await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
        assert!(result.message().contains("forked to"));
    }

    #[tokio::test]
    async fn fork_with_prompt() {
        let d = test_dispatcher();
        let principal = PrincipalId::new();
        let source = register_context(&d, Some("src"), None, principal).await;
        d.block_store()
            .create_document(source, crate::DocumentKind::Conversation, None)
            .unwrap();

        let c = caller_with_context(source);
        let result = d
            .dispatch(
                &[s("fork"), s("--name"), s("noted"), s("--prompt"), s("explore auth bug")],
                &c,
            )
            .await;
        assert!(result.is_ok(), "fork failed: {}", result.message());
    }

    #[tokio::test]
    async fn fork_help() {
        let d = test_dispatcher();
        let c = test_caller();
        let result = d.dispatch(&[s("fork"), s("help")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("USAGE"));
    }
}

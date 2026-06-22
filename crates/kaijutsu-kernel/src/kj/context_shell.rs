//! Per-use context-shell materialization.
//!
//! The "context shell" is **not** a long-lived process — it is shared state
//! that evolves over a context's lifetime. Its durable identity is `env + cwd`,
//! persisted in the DB (`context_env` + `context_shell.cwd`). Every invocation
//! materializes a throwaway [`EmbeddedKaish`] for exactly one
//! `(principal, session, context)`, seeded from that durable state, runs one
//! command, and is dropped.
//!
//! Two consequences fall out of "one instance per invocation":
//!
//! * **Identity is first-class and correct without trickery.** The old
//!   per-connection kaish baked in a principal and was shared, so a stop/kill
//!   keyed on "the calling connection" rather than the context (see
//!   `docs/issues.md`). Because a materialized instance serves a single
//!   principal for a single command, baking that principal in is accurate, not
//!   a hack. Telemetry baggage is a separate, parallel concern — never the
//!   source of truth for authorship.
//! * **No junk builds up.** Durable state changes only through the explicit
//!   `kj context set --env/--cwd` channel; transient scope evaporates with the
//!   instance. rc scripts, hooks, the model's `shell` tool, the interactive
//!   shell, and headless turns all share this one path, so they all see the
//!   same durable state and none of each other's transients.

use std::sync::Arc;

use anyhow::Result;
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use crate::runtime::context_engine::SessionContextMap;
use crate::runtime::embedded_kaish::EmbeddedKaish;

use super::KjDispatcher;

impl KjDispatcher {
    /// Materialize a single-use context shell for `(principal, context,
    /// session)`, seeded from the context's durable state (`context_env` +
    /// `context_shell.cwd`).
    ///
    /// `semantic_index` / `block_source` wire `kj`'s synthesis tools: kernel-side
    /// callers (rc, hooks) pass `None` + a no-op source; the server passes the
    /// real index and a block-backed source. The returned instance is
    /// throwaway — run one command against it and drop it. Durable changes go
    /// through `kj context set`, not through this instance's scope.
    pub async fn materialize_context_kaish(
        &self,
        name: &str,
        principal: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Result<EmbeddedKaish> {
        self.materialize_context_kaish_inner(
            name,
            principal,
            context_id,
            session_id,
            semantic_index,
            block_source,
            false,
            false,
        )
        .await
    }

    /// Like [`Self::materialize_context_kaish`] but the materialized `kj` runs
    /// **privileged** — the rc lifecycle's trusted control plane, allowed to
    /// assign (widen) a context's loadout. ONLY the rc runner may call this;
    /// agent/hook/human paths use the unprivileged variant.
    pub async fn materialize_context_kaish_rc(
        &self,
        name: &str,
        principal: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Result<EmbeddedKaish> {
        self.materialize_context_kaish_inner(
            name,
            principal,
            context_id,
            session_id,
            semantic_index,
            block_source,
            true,
            false,
        )
        .await
    }

    /// Like [`Self::materialize_context_kaish`] but the materialized shell is
    /// **read-only**: filesystem mutations and external commands are refused by
    /// construction, while reads — real files and the CRDT `/v/docs` /
    /// `/v/input` views — still work. Backs the toolie's `read_only_shell`.
    /// Unprivileged (the read-only role is never the rc control plane).
    pub async fn materialize_context_kaish_read_only(
        &self,
        name: &str,
        principal: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Result<EmbeddedKaish> {
        self.materialize_context_kaish_inner(
            name,
            principal,
            context_id,
            session_id,
            semantic_index,
            block_source,
            false,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn materialize_context_kaish_inner(
        &self,
        name: &str,
        principal: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
        privileged: bool,
        read_only: bool,
    ) -> Result<EmbeddedKaish> {
        // Fresh, isolated session map: this kaish lives for one invocation and
        // tracks exactly one session→context mapping. No cross-invocation
        // leakage, nothing to evict.
        let session_contexts: SessionContextMap =
            crate::runtime::context_engine::session_context_map();
        session_contexts.insert(session_id, context_id);

        // Register `kj` so the shell can introspect/mutate the context. Falls
        // back to the bare kaish surface if `set_self_arc` was never called
        // (some test dispatchers don't bother).
        let dispatcher = self.self_arc();
        let configure_tools = move |scm: SessionContextMap,
                                    sid: SessionId,
                                    tools: &mut kaish_kernel::ToolRegistry| {
            if let Some(d) = dispatcher {
                tools.register(crate::runtime::kj_builtin::KjBuiltin::new(
                    d,
                    scm,
                    principal,
                    sid,
                    semantic_index,
                    block_source,
                    privileged,
                ));
            }
        };

        let kaish = if read_only {
            EmbeddedKaish::with_identity_read_only(
                name,
                self.block_store().clone(),
                self.kernel().clone(),
                None,
                principal,
                context_id,
                session_id,
                session_contexts,
                configure_tools,
            )?
        } else {
            EmbeddedKaish::with_identity(
                name,
                self.block_store().clone(),
                self.kernel().clone(),
                None,
                principal,
                context_id,
                session_id,
                session_contexts,
                configure_tools,
            )?
        };

        // Seed the env half of the context's durable state.
        kaish.apply_context_config(self.kernel_db(), context_id).await;

        // Restore the persisted cwd, validated against the shell's backend (the
        // VFS namespace `cd` uses — a host-FS check would wrongly reject
        // VFS-only cwds like /scratch or /v/docs). A persisted cwd that no
        // longer resolves is surfaced, not silently dropped.
        if let Err(dead) = kaish.restore_cwd_from_db(self.kernel_db(), context_id).await {
            tracing::warn!(
                context = %context_id.to_hex(),
                cwd = %dead.display(),
                "persisted context cwd no longer resolves in backend; using default landing dir",
            );
            kaijutsu_telemetry::record_cwd_restore_failed();
        }

        Ok(kaish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::lifecycle::NoopBlockSource;
    use crate::kj::test_helpers::*;
    use kaish_kernel::ExecuteOptions;

    /// A materialized context shell must be seeded from the context's durable
    /// env (`context_env`) — that is the whole point of "shared state that
    /// evolves over the context lifetime." This test fails if materialization
    /// stops reading L1.
    #[tokio::test]
    async fn materialized_shell_seeds_durable_env() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);

        // L1: set a durable env var on the context.
        d.kernel_db()
            .lock()
            .set_context_env(ctx, "FOO", "bar")
            .unwrap();

        let kaish = d
            .materialize_context_kaish(
                "test-ctx-shell",
                principal,
                ctx,
                SessionId::new(),
                None,
                Arc::new(NoopBlockSource),
            )
            .await
            .expect("materialize context shell");

        let result = kaish
            .execute_with_options("echo $FOO", ExecuteOptions::default())
            .await
            .expect("run echo");
        assert_eq!(
            result.text_out().trim(),
            "bar",
            "durable context_env FOO should seed the materialized shell",
        );
    }

    /// Two materializations of the same context, by different principals, are
    /// independent instances — each authors as its own principal. This guards
    /// the core fix: identity is per-invocation, not shared/baked-once.
    #[tokio::test]
    async fn materializations_are_independent_per_principal() {
        let d = test_dispatcher().await;
        let alice = PrincipalId::new();
        let bob = PrincipalId::new();
        let ctx = register_context(&d, Some("shared"), None, alice);

        let ka = d
            .materialize_context_kaish(
                "alice",
                alice,
                ctx,
                SessionId::new(),
                None,
                Arc::new(NoopBlockSource),
            )
            .await
            .expect("materialize for alice");
        let kb = d
            .materialize_context_kaish(
                "bob",
                bob,
                ctx,
                SessionId::new(),
                None,
                Arc::new(NoopBlockSource),
            )
            .await
            .expect("materialize for bob");

        // A var set in one instance's scope must not bleed into the other —
        // transients are per-invocation, the durable channel is the DB.
        ka.execute_with_options("export ONLY_ALICE=1", ExecuteOptions::default())
            .await
            .expect("set in alice");
        let leaked = kb
            .execute_with_options("echo \"[$ONLY_ALICE]\"", ExecuteOptions::default())
            .await
            .expect("read in bob");
        assert_eq!(
            leaked.text_out().trim(),
            "[]",
            "transient scope must not leak between materialized instances",
        );
    }

    /// The model shell's synthesis fix: `KjDispatcher::block_source` surfaces a
    /// context's real block snapshots (what `kj search`/synthesis consume),
    /// where the rc/hook `NoopBlockSource` is deliberately blind. Also pins the
    /// `semantic_index` install round-trip. Without this wiring the model's
    /// `shell` / `read_only_shell` ran with degraded (empty) block search.
    #[tokio::test]
    async fn block_source_surfaces_real_blocks_where_noop_is_blind() {
        use crate::kj::lifecycle::NoopBlockSource;
        use kaijutsu_index::BlockSource as _;
        use kaijutsu_types::{BlockKind, ContentType, DocKind, Role, Status};

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("synth"), None, principal);

        // Seed one block into the context's document.
        d.block_store()
            .create_document(ctx, DocKind::Conversation, None)
            .expect("create document");
        d.block_store()
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "hello synthesis",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert block");

        // The real source sees the block; NoopBlockSource (rc/hook path) does not.
        let real = d.block_source().block_snapshots(ctx).expect("real snapshots");
        assert!(
            !real.is_empty(),
            "block_source must surface the context's blocks (the synthesis fix)",
        );
        let noop = NoopBlockSource.block_snapshots(ctx).expect("noop snapshots");
        assert!(
            noop.is_empty(),
            "NoopBlockSource is the degraded path — it surfaces nothing",
        );

        // The index install round-trips (server wires it at bootstrap; None
        // when embeddings are off — the model shell then degrades gracefully).
        assert!(d.semantic_index().is_none(), "no index installed by default");
        d.set_semantic_index(None);
        assert!(d.semantic_index().is_none());
    }
}

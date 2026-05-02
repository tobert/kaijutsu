//! Run-control (rc) lifecycle dispatch.
//!
//! Fires at context lifecycle moments (`create`, `fork`; `attach` and
//! `drift` reserved). Looks up scripts at `/etc/rc/<context_type>/<verb>/`,
//! runs them in lexical sort order. `.md` scripts become blocks; `.kai`
//! scripts execute via `kaish_kernel::Kernel::execute_with_vars`.
//!
//! ## Failure semantics
//!
//! Scripts run after the context is committed. A script failure inserts a
//! `BlockKind::Error` block into the new context with rc path, sort key,
//! exit code, and last 4 KB of stderr/stdout. Subsequent scripts continue
//! to run — the new context is "alive but degraded," matching SysV
//! init.d. No rollback. The error block is non-ephemeral so the LLM sees
//! it on next hydrate.
//!
//! ## Recursion guard
//!
//! `KjCaller.rc_depth` is bumped before each rc-driven invocation (via
//! the `KJ_RC_DEPTH` overlay var, read by `KjBuiltin` when constructing
//! its caller). When depth exceeds `MAX_RC_DEPTH`, the script is skipped
//! and an error block is inserted in its place.
//!
//! ## `kj` from inside rc kaish (shipped 2026-05-02)
//!
//! `KjDispatcher` stores a `Weak<Self>` (set via `set_self_arc` after
//! `Arc::new`); `run_kai_script` upgrades it and registers `KjBuiltin`
//! into the rc session's tool registry. Test dispatchers that don't
//! call `set_self_arc` still run scripts — they just don't get `kj`
//! in scope.

use std::collections::HashMap;

use kaijutsu_types::{
    BlockKind, ContentType, ContextId, ForkKind, PrincipalId, Role, Status,
};

use crate::kernel_db::RcScriptRow;

use super::{KjCaller, KjDispatcher};

/// Hard cap on rc-driven recursion depth. A script that hits this limit
/// produces an error block and is skipped — its lifecycle does NOT run.
pub const MAX_RC_DEPTH: u8 = 4;

/// Last N bytes of stdout/stderr captured into the failure block.
const RC_FAILURE_OUTPUT_TAIL_BYTES: usize = 4096;

pub const VERB_CREATE: &str = "create";
pub const VERB_FORK: &str = "fork";
pub const VERB_ATTACH: &str = "attach";
pub const VERB_DRIFT: &str = "drift";

fn verb_is_wired(verb: &str) -> bool {
    matches!(verb, VERB_CREATE | VERB_FORK)
}

impl KjDispatcher {
    /// Run rc lifecycle scripts for `(context_type, verb)` against the
    /// **new** context. See module docs for failure semantics.
    pub async fn run_rc_lifecycle(
        &self,
        verb: &str,
        new_id: ContextId,
        parent_id: Option<ContextId>,
        fork_kind: Option<ForkKind>,
        caller: &KjCaller,
    ) -> Result<(), String> {
        if !verb_is_wired(verb) {
            log_unwired_verb_once(verb);
            return Ok(());
        }

        let (context_type, scripts) = {
            let db = self.kernel_db().lock();
            let ctx_type = match db.get_context(new_id) {
                Ok(Some(row)) => row.context_type,
                Ok(None) => {
                    return Err(format!(
                        "rc lifecycle: context {} not found",
                        new_id.short()
                    ));
                }
                Err(e) => return Err(format!("rc lifecycle: {e}")),
            };
            let scripts = db
                .list_rc_scripts(self.kernel_id(), &ctx_type, verb)
                .map_err(|e| format!("rc lifecycle: list scripts: {e}"))?;
            (ctx_type, scripts)
        };

        if scripts.is_empty() {
            return Ok(());
        }

        // The BlockStore document for this context may not exist yet —
        // context_create commits the KernelDb document but doesn't seed
        // the in-memory BlockStore (LLM stream / RPC handler creates it
        // lazily on first block). rc scripts insert blocks now, so we
        // must ensure the BlockStore doc exists.
        match self
            .block_store()
            .create_document(new_id, kaijutsu_types::DocKind::Conversation, None)
        {
            Ok(()) => {}
            Err(crate::block_store::BlockStoreError::DocumentAlreadyExists(_)) => {}
            Err(e) => {
                tracing::warn!("rc lifecycle: create_document failed: {e}");
            }
        }

        if caller.rc_depth >= MAX_RC_DEPTH {
            insert_rc_failure_block(
                self,
                new_id,
                "<recursion-guard>",
                "S00",
                None,
                format!(
                    "rc depth limit exceeded ({} >= {}); refusing to run /etc/rc/{}/{}/* scripts",
                    caller.rc_depth, MAX_RC_DEPTH, context_type, verb
                ),
                caller.principal_id,
            );
            return Ok(());
        }

        let child_depth = caller.rc_depth + 1;

        for script in &scripts {
            match script.extension.as_str() {
                "md" => run_md_script(self, new_id, script, caller.principal_id),
                "kai" => {
                    run_kai_script(
                        self,
                        new_id,
                        parent_id,
                        fork_kind,
                        verb,
                        script,
                        child_depth,
                        caller.principal_id,
                    )
                    .await
                }
                other => {
                    insert_rc_failure_block(
                        self,
                        new_id,
                        &script.path,
                        &script.sort_key,
                        None,
                        format!("rc lifecycle: unknown extension '{other}'"),
                        caller.principal_id,
                    );
                }
            }
        }

        Ok(())
    }
}

fn run_md_script(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    script: &RcScriptRow,
    principal: PrincipalId,
) {
    let after = dispatcher.block_store().last_block_id(new_id);
    let result = dispatcher.block_store().insert_block_as(
        new_id,
        None,
        after.as_ref(),
        Role::System,
        BlockKind::Text,
        script.content.clone(),
        Status::Done,
        ContentType::Markdown,
        Some(principal),
    );
    if let Err(e) = result {
        insert_rc_failure_block(
            dispatcher,
            new_id,
            &script.path,
            &script.sort_key,
            None,
            format!("rc .md insert failed: {e}"),
            principal,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_kai_script(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    parent_id: Option<ContextId>,
    fork_kind: Option<ForkKind>,
    verb: &str,
    script: &RcScriptRow,
    child_depth: u8,
    principal: PrincipalId,
) {
    use crate::runtime::context_engine::session_context_map;
    use crate::runtime::embedded_kaish::EmbeddedKaish;
    use kaijutsu_types::SessionId;

    let session_id = SessionId::new();
    let kernel_id = dispatcher.kernel_id();
    let session_contexts = session_context_map();
    session_contexts.insert(session_id, new_id);

    let blocks = dispatcher.block_store().clone();
    let kernel = dispatcher.kernel().clone();

    // Register `kj` so scripts can introspect/mutate the new context.
    // Falls back to no tools if `set_self_arc` was never called (test
    // dispatchers may not bother, in which case scripts get the bare
    // kaish surface only — same as hunk #1).
    let dispatcher_arc = dispatcher.self_arc();
    let configure_tools = move |scm: crate::runtime::context_engine::SessionContextMap,
                                sid: kaijutsu_types::SessionId,
                                tools: &mut kaish_kernel::ToolRegistry| {
        if let Some(d) = dispatcher_arc {
            tools.register(crate::runtime::kj_builtin::KjBuiltin::new(
                d,
                scm,
                principal,
                sid,
                None,
                std::sync::Arc::new(NoopBlockSource),
            ));
        }
    };

    let kaish = match EmbeddedKaish::with_identity(
        "rc",
        blocks,
        kernel,
        None,
        principal,
        new_id,
        session_id,
        kernel_id,
        session_contexts,
        configure_tools,
    ) {
        Ok(k) => k,
        Err(e) => {
            insert_rc_failure_block(
                dispatcher,
                new_id,
                &script.path,
                &script.sort_key,
                None,
                format!("rc lifecycle: kaish init failed: {e}"),
                principal,
            );
            return;
        }
    };

    let mut vars: HashMap<String, kaish_kernel::ast::Value> = HashMap::new();
    vars.insert(
        "KJ_CONTEXT".into(),
        kaish_kernel::ast::Value::String(new_id.to_hex()),
    );
    vars.insert(
        "KJ_VERB".into(),
        kaish_kernel::ast::Value::String(verb.to_string()),
    );
    vars.insert(
        "KJ_RC_DEPTH".into(),
        kaish_kernel::ast::Value::String(child_depth.to_string()),
    );
    if let Some(pid) = parent_id {
        vars.insert(
            "KJ_PARENT_CONTEXT".into(),
            kaish_kernel::ast::Value::String(pid.to_hex()),
        );
    }
    if let Some(fk) = fork_kind {
        let json = serde_json::json!({
            "kind": fk.as_str(),
            "parent": parent_id.map(|p| p.to_hex()),
        });
        vars.insert(
            "KJ_FORK_INFO".into(),
            kaish_kernel::ast::Value::String(json.to_string()),
        );
    }

    match kaish.execute_with_vars(&script.content, vars).await {
        Ok(exec) if exec.code == 0 => {}
        Ok(exec) => {
            let stdout = exec.text_out().into_owned();
            insert_rc_failure_block(
                dispatcher,
                new_id,
                &script.path,
                &script.sort_key,
                Some(exec.code as i32),
                tail_output(&stdout, &exec.err),
                principal,
            );
        }
        Err(e) => {
            insert_rc_failure_block(
                dispatcher,
                new_id,
                &script.path,
                &script.sort_key,
                None,
                format!("rc kaish exec error: {e}"),
                principal,
            );
        }
    }
}

fn tail_output(stdout: &str, stderr: &str) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str("--- stdout ---\n");
        combined.push_str(stdout);
        combined.push('\n');
    }
    if !stderr.is_empty() {
        combined.push_str("--- stderr ---\n");
        combined.push_str(stderr);
    }
    if combined.len() <= RC_FAILURE_OUTPUT_TAIL_BYTES {
        return combined;
    }
    let cut = combined.len() - RC_FAILURE_OUTPUT_TAIL_BYTES;
    let mut start = cut;
    while start < combined.len() && !combined.is_char_boundary(start) {
        start += 1;
    }
    format!("[truncated]\n{}", &combined[start..])
}

fn insert_rc_failure_block(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    rc_path: &str,
    sort_key: &str,
    exit_code: Option<i32>,
    detail: String,
    principal: PrincipalId,
) {
    // Hunk #1: emit a plain BlockKind::Error block with the diagnostic in
    // content. Structured ErrorPayload requires a parent block, which the
    // freshly-created context may not have. Tracked as a follow-up.
    let summary = match exit_code {
        Some(code) => format!(
            "rc {sort_key} exit {code}: {rc_path}\nrc_path: {rc_path}\nsort_key: {sort_key}\nexit_code: {code}\n\n{detail}"
        ),
        None => format!(
            "rc {sort_key} failed: {rc_path}\nrc_path: {rc_path}\nsort_key: {sort_key}\nexit_code: n/a\n\n{detail}"
        ),
    };
    let after = dispatcher.block_store().last_block_id(new_id);
    if let Err(e) = dispatcher.block_store().insert_block_as(
        new_id,
        None,
        after.as_ref(),
        Role::System,
        BlockKind::Error,
        summary,
        Status::Error,
        ContentType::Plain,
        Some(principal),
    ) {
        tracing::error!(
            "rc lifecycle: could not insert failure block for {rc_path}: {e}"
        );
    }
}

/// Stub `BlockSource` for `KjBuiltin`'s synthesis wiring. rc and hook
/// kaish sessions don't need semantic search; passing a real source
/// would require kaijutsu-index plumbing that doesn't belong here.
pub(crate) struct NoopBlockSource;

impl kaijutsu_index::BlockSource for NoopBlockSource {
    fn block_snapshots(
        &self,
        _ctx: kaijutsu_types::ContextId,
    ) -> Result<Vec<kaijutsu_types::BlockSnapshot>, String> {
        Ok(Vec::new())
    }
}

fn log_unwired_verb_once(verb: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    static ATTACH_LOGGED: OnceLock<AtomicBool> = OnceLock::new();
    static DRIFT_LOGGED: OnceLock<AtomicBool> = OnceLock::new();
    let flag = match verb {
        VERB_ATTACH => ATTACH_LOGGED.get_or_init(|| AtomicBool::new(false)),
        VERB_DRIFT => DRIFT_LOGGED.get_or_init(|| AtomicBool::new(false)),
        _ => return,
    };
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::info!(
            target: "kaijutsu::rc",
            "rc lifecycle verb '{verb}' is reserved but not yet wired; \
             scripts under /etc/rc/*/{verb}/ will not run"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::RcScriptRow;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    fn install_script(
        dispatcher: &KjDispatcher,
        path: &str,
        context_type: &str,
        verb: &str,
        sort_key: &str,
        name: &str,
        ext: &str,
        content: &str,
    ) {
        let row = RcScriptRow {
            kernel_id: dispatcher.kernel_id(),
            context_type: context_type.into(),
            verb: verb.into(),
            sort_key: sort_key.into(),
            name: name.into(),
            extension: ext.into(),
            content: content.into(),
            path: path.into(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: PrincipalId::system(),
        };
        let db = dispatcher.kernel_db().lock();
        db.insert_rc_script(&row).expect("install rc script");
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// Caller with no joined context — `kj context create` without
    /// `--parent` resolves to `None` rather than the test caller's fake
    /// id, avoiding a FK violation on the forked_from column.
    fn unjoined_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
        }
    }

    fn block_kinds_in(dispatcher: &KjDispatcher, ctx: ContextId) -> Vec<kaijutsu_types::BlockKind> {
        dispatcher
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.kind)
            .collect()
    }

    fn block_contents_in(dispatcher: &KjDispatcher, ctx: ContextId) -> Vec<String> {
        dispatcher
            .block_store()
            .block_snapshots(ctx)
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.content)
            .collect()
    }

    /// Resolve a context by label so tests don't have to scrape the
    /// "created context 'X' (id)" message.
    fn lookup_context_id(dispatcher: &KjDispatcher, label: &str) -> ContextId {
        let db = dispatcher.kernel_db().lock();
        db.find_context_by_label(dispatcher.kernel_id(), label)
            .expect("get_context_by_label")
            .expect("context exists")
            .context_id
    }

    #[tokio::test]
    async fn rc_create_md_inserts_block() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-prompt.md",
            "test",
            "create",
            "S00",
            "prompt",
            "md",
            "You are a test context. Be terse.",
        );
        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-md", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-md");
        let contents = block_contents_in(&d, new_id);
        assert!(
            contents.iter().any(|c| c.contains("You are a test context")),
            "expected .md content as block, got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn rc_create_kai_runs_script() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-noop.kai",
            "test",
            "create",
            "S00",
            "noop",
            "kai",
            "true",
        );
        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-kai", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-kai");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Error),
            "successful .kai should not insert error block, got kinds: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn rc_kai_can_call_kj() {
        // .kai scripts get `kj` registered when the dispatcher's
        // self-Arc is wired. Without `set_self_arc`, the test
        // dispatcher's scripts still run but can't reach kj.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();

        // Script asserts overlay vars are populated and that `kj` is
        // callable. Exit 0 → no error block; non-zero → error block.
        install_script(
            &d,
            "/etc/rc/test/create/S00-introspect.kai",
            "test",
            "create",
            "S00",
            "introspect",
            "kai",
            "test -n \"$KJ_CONTEXT\" && test -n \"$KJ_VERB\" && kj context list",
        );
        let caller = unjoined_caller();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-kj", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-kj");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Error),
            ".kai with kj invocation must exit 0 — got error block; kinds: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn rc_no_scripts_for_type_is_noop() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-empty", "--type", "nonexistent"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-empty");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            kinds.is_empty(),
            "no scripts should leave context block-free, got: {kinds:?}"
        );
    }

    #[tokio::test]
    async fn rc_script_failure_inserts_error_block_continues() {
        let d = test_dispatcher().await;
        // S00 returns non-zero; S10 is benign.
        install_script(
            &d,
            "/etc/rc/test/create/S00-fail.kai",
            "test",
            "create",
            "S00",
            "fail",
            "kai",
            "exit 17",
        );
        install_script(
            &d,
            "/etc/rc/test/create/S10-after.md",
            "test",
            "create",
            "S10",
            "after",
            "md",
            "ran-after-failure",
        );

        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-mixed", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-mixed");
        let kinds = block_kinds_in(&d, new_id);
        let contents = block_contents_in(&d, new_id);

        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "S00 failure must produce Error block, got kinds: {kinds:?}"
        );
        assert!(
            contents.iter().any(|c| c.contains("ran-after-failure")),
            "S10 must run after S00 fails, got contents: {contents:?}"
        );
        // Sanity: the error content should mention the failing path.
        assert!(
            contents
                .iter()
                .any(|c| c.contains("/etc/rc/test/create/S00-fail.kai")),
            "error block should reference rc path, got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn rc_attach_drift_install_but_no_op() {
        let d = test_dispatcher().await;
        // Reserved verbs install successfully and dispatch as no-op.
        install_script(
            &d,
            "/etc/rc/test/drift/S00-noop.md",
            "test",
            "drift",
            "S00",
            "noop",
            "md",
            "drift-script-content",
        );
        // Direct lifecycle invocation with a fake context — should noop.
        let dummy_id = ContextId::new();
        let caller = unjoined_caller();
        let res = d.run_rc_lifecycle("drift", dummy_id, None, None, &caller).await;
        assert!(res.is_ok(), "drift verb should no-op, got: {res:?}");
        assert!(
            block_kinds_in(&d, dummy_id).is_empty(),
            "drift no-op should not touch the context"
        );
    }

    #[tokio::test]
    async fn rc_recursion_guard_caps_depth() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-noop.md",
            "test",
            "create",
            "S00",
            "noop",
            "md",
            "would-run",
        );
        let mut caller = unjoined_caller();
        caller.rc_depth = MAX_RC_DEPTH; // simulate already-deep invocation

        // Construct a fresh context manually via the dispatch path.
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-recur", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok());
        let new_id = lookup_context_id(&d, "ctx-recur");
        let kinds = block_kinds_in(&d, new_id);
        // Recursion guard fires: error block, no .md block.
        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "guard should insert Error block, got: {kinds:?}"
        );
        let contents = block_contents_in(&d, new_id);
        assert!(
            !contents.iter().any(|c| c.contains("would-run")),
            "guarded run must not insert .md block, got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn rc_fork_does_not_trigger_create_scripts() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-only-create.md",
            "test",
            "create",
            "S00",
            "only-create",
            "md",
            "CREATE-MARKER",
        );
        install_script(
            &d,
            "/etc/rc/test/fork/S00-only-fork.md",
            "test",
            "fork",
            "S00",
            "only-fork",
            "md",
            "FORK-MARKER",
        );

        // Step 1: create parent (CREATE-MARKER appears in parent).
        let caller = unjoined_caller();
        d.dispatch(&argv(&["context", "create", "parent", "--type", "test"]), &caller)
            .await;
        let parent_id = lookup_context_id(&d, "parent");
        let parent_contents = block_contents_in(&d, parent_id);
        assert!(
            parent_contents.iter().any(|c| c.contains("CREATE-MARKER")),
            "parent should have CREATE-MARKER, got: {parent_contents:?}"
        );

        // Step 2: fork the parent. Use a caller bound to the parent so
        // fork resolves "."  to the parent context.
        let fork_caller = caller_with_context(parent_id);
        let fork_result = d
            .dispatch(&argv(&["fork", "--name", "child"]), &fork_caller)
            .await;
        assert!(fork_result.is_ok(), "fork failed: {}", fork_result.message());

        let child_id = lookup_context_id(&d, "child");
        let child_contents = block_contents_in(&d, child_id);
        assert!(
            child_contents.iter().any(|c| c.contains("FORK-MARKER")),
            "child should have FORK-MARKER, got: {child_contents:?}"
        );
        // The forked document inherits parent blocks (which include
        // CREATE-MARKER from the parent's create lifecycle), so we
        // can't assert CREATE-MARKER is absent. The verb-isolation
        // guarantee is: no NEW CREATE-MARKER is inserted at fork time.
        // Count occurrences instead.
        let create_marker_count = child_contents
            .iter()
            .filter(|c| c.contains("CREATE-MARKER"))
            .count();
        assert_eq!(
            create_marker_count, 1,
            "fork must not run create-side scripts (would duplicate marker), got: {child_contents:?}"
        );
    }
}

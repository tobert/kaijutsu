//! Run-control (rc) lifecycle dispatch.
//!
//! Fires at context lifecycle moments (`create`, `fork`; `attach` and
//! `drift` reserved). Looks up scripts at `/etc/rc/<context_type>/<verb>/`,
//! runs them in lexical sort order. `.md` scripts become blocks; `.kai`
//! scripts execute via `kaish_kernel::Kernel::execute_with_options` with the
//! kernel's `TimeoutPolicy::rc_script_timeout` applied per call.
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
    BlockKind, ContentType, ContextId, DriftKind, ForkKind, PrincipalId, Role, Status,
};

use super::{KjCaller, KjDispatcher};

/// One rc script resolved from the `/etc/rc` file tree for a single
/// lifecycle run. The path is canonical (`/etc/rc/<type>/<verb>/SXX-name.ext`);
/// `sort_key` and `extension` are parsed from the filename for ordering and
/// dispatch. Content is read through the kernel's `FileDocumentCache`.
pub(crate) struct RcScript {
    pub path: String,
    pub sort_key: String,
    pub extension: String,
    pub content: String,
}

/// Split an rc filename `SXX-name.ext` into `(sort_key, extension)`.
/// `sort_key` is everything before the first `-`; `extension` is everything
/// after the last `.`. Canonical seed/installed paths always match
/// `parse_rc_path`, so these are well-formed; a stray file that doesn't is
/// handled gracefully (empty sort_key / extension → skipped or errored by
/// the caller's extension match).
fn parse_rc_filename(name: &str) -> (String, String) {
    let sort_key = name.split('-').next().unwrap_or("").to_string();
    let extension = name.rsplit('.').next().unwrap_or("").to_string();
    (sort_key, extension)
}

/// Per-drift metadata surfaced to rc scripts via `KJ_DRIFT_INFO`. Built by
/// drift call sites and passed into `run_rc_lifecycle` as `drift_info`.
#[derive(Clone, Debug)]
pub struct DriftInfo {
    pub kind: DriftKind,
    pub source_ctx: ContextId,
    pub target_ctx: ContextId,
    pub source_model: Option<String>,
}

/// Hard cap on rc-driven recursion depth. A script that hits this limit
/// produces an error block and is skipped — its lifecycle does NOT run.
pub const MAX_RC_DEPTH: u8 = 4;

/// Last N bytes of stdout/stderr captured into the failure block.
const RC_FAILURE_OUTPUT_TAIL_BYTES: usize = 4096;

pub const VERB_CREATE: &str = "create";
pub const VERB_FORK: &str = "fork";
pub const VERB_ATTACH: &str = "attach";
pub const VERB_DRIFT: &str = "drift";
/// The beat verb: fired by the kernel beat scheduler on a context's coarse OODA
/// cadence (e.g. every N bars for a composer). Its scripts are the per-beat work
/// hook — typically `kj drive` to request the next OODA turn. Materialized the
/// same throwaway-kaish way the other verbs are; no new runtime.
pub const VERB_TICK: &str = "tick";

fn verb_is_wired(verb: &str) -> bool {
    matches!(verb, VERB_CREATE | VERB_FORK | VERB_DRIFT | VERB_ATTACH | VERB_TICK)
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
        drift_info: Option<DriftInfo>,
        caller: &KjCaller,
    ) -> Result<(), String> {
        self.run_rc_lifecycle_inner(
            verb, new_id, parent_id, fork_kind, drift_info, &HashMap::new(), caller,
        )
        .await
    }

    /// Like [`run_rc_lifecycle`](Self::run_rc_lifecycle), but seeds `extra_vars`
    /// into every `.kai` script's kaish environment alongside the standard
    /// `KJ_*` vars. The composer beat scheduler uses this to hand the `tick`
    /// lifecycle its transport heartbeat (`$TICK` / `$PHRASE` / `$TEMPO`) so
    /// `S10-drive.kai` can compose the turn's transport report. Bare names (no
    /// `KJ_` prefix) per the heartbeat-var taxonomy in `docs/chameleon.md`.
    #[allow(clippy::too_many_arguments)] // mirrors the lifecycle param shape
    pub async fn run_rc_lifecycle_with_vars(
        &self,
        verb: &str,
        new_id: ContextId,
        parent_id: Option<ContextId>,
        fork_kind: Option<ForkKind>,
        drift_info: Option<DriftInfo>,
        extra_vars: &HashMap<String, String>,
        caller: &KjCaller,
    ) -> Result<(), String> {
        self.run_rc_lifecycle_inner(
            verb, new_id, parent_id, fork_kind, drift_info, extra_vars, caller,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // mirrors the lifecycle param shape
    #[tracing::instrument(
        skip(self, drift_info, extra_vars, caller),
        fields(verb = %verb, ctx = %new_id.short(), rc_depth = caller.rc_depth),
    )]
    async fn run_rc_lifecycle_inner(
        &self,
        verb: &str,
        new_id: ContextId,
        parent_id: Option<ContextId>,
        fork_kind: Option<ForkKind>,
        drift_info: Option<DriftInfo>,
        extra_vars: &HashMap<String, String>,
        caller: &KjCaller,
    ) -> Result<(), String> {
        if !verb_is_wired(verb) {
            log_unwired_verb_once(verb);
            return Ok(());
        }

        let context_type = {
            let db = self.kernel_db().lock();
            match db.get_context(new_id) {
                Ok(Some(row)) => row.context_type,
                Ok(None) => {
                    return Err(format!(
                        "rc lifecycle: context {} not found",
                        new_id.short()
                    ));
                }
                Err(e) => return Err(format!("rc lifecycle: {e}")),
            }
        };

        let scripts = self.load_rc_scripts(&context_type, verb).await?;

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
                        drift_info.as_ref(),
                        verb,
                        script,
                        child_depth,
                        extra_vars,
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

    /// Load the rc scripts for `(context_type, verb)` from the `/etc/rc`
    /// file tree, ordered lexically by filename (which is exactly
    /// `(sort_key, name)` order). A missing directory means "no scripts for
    /// this verb" — the common case — and returns empty, not an error. A
    /// read failure on a present file *is* surfaced: per the
    /// crash-over-corruption stance an unreadable stance script is
    /// corruption, not an empty default.
    pub(crate) async fn load_rc_scripts(
        &self,
        context_type: &str,
        verb: &str,
    ) -> Result<Vec<RcScript>, String> {
        use crate::vfs::{VfsError, VfsOps};

        let dir = format!("/etc/rc/{context_type}/{verb}");
        let vfs = self.kernel().vfs();
        let entries = match vfs.readdir(std::path::Path::new(&dir)).await {
            Ok(e) => e,
            // Directory absent → no scripts for this (type, verb).
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(format!("rc lifecycle: readdir {dir}: {e}")),
        };

        let mut names: Vec<String> = entries
            .into_iter()
            .filter(|e| e.kind.is_file())
            .map(|e| e.name)
            .filter(|n| n.ends_with(".kai") || n.ends_with(".md"))
            .collect();
        // Lexical filename sort == (sort_key, name) order: the filename is
        // `{sort_key}-{name}.{ext}`, so S00 < S10 and ties break on name.
        names.sort();

        let cache = self.kernel().file_cache(self.block_store());
        let mut scripts = Vec::with_capacity(names.len());
        for name in names {
            let path = format!("{dir}/{name}");
            let content = cache
                .read_content(&path)
                .await
                .map_err(|e| format!("rc lifecycle: read {path}: {e}"))?;
            let (sort_key, extension) = parse_rc_filename(&name);
            scripts.push(RcScript {
                path,
                sort_key,
                extension,
                content,
            });
        }
        Ok(scripts)
    }
}

fn run_md_script(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    script: &RcScript,
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
    drift_info: Option<&DriftInfo>,
    verb: &str,
    script: &RcScript,
    child_depth: u8,
    extra_vars: &HashMap<String, String>,
    principal: PrincipalId,
) {
    use kaijutsu_types::SessionId;

    // Each rc script runs in its own single-use context shell — a snapshot of
    // the context's durable state (env + cwd). Scripts evolve durable state
    // only through the explicit `kj context set` channel, so later scripts in
    // the phase see earlier ones' deliberate writes, never their transients.
    // rc uses the bare kj surface (no semantic index): `NoopBlockSource`.
    let kaish = match dispatcher
        .materialize_context_kaish_rc(
            "rc",
            principal,
            new_id,
            SessionId::new(),
            None,
            std::sync::Arc::new(NoopBlockSource),
        )
        .await
    {
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
        // Parent's block count at fork time = the number of blocks
        // copied into the child (for shallow/full forks; for compact
        // forks the child has a summary, so this is the
        // pre-summarization size). rc-on-fork scripts use it to
        // compute `MessageIndex(KJ_PARENT_BLOCK_COUNT - 1)` for the
        // fork-point cache breakpoint without parsing JSON. Captured
        // from the *parent's* BlockStore because the child's count
        // already includes the fork-marker block injected before
        // this rc hook fires (see kj/fork.rs:274).
        if let Some(pid) = parent_id {
            let count = dispatcher
                .block_store()
                .block_snapshots(pid)
                .map(|b| b.len())
                .unwrap_or(0);
            vars.insert(
                "KJ_PARENT_BLOCK_COUNT".into(),
                kaish_kernel::ast::Value::String(count.to_string()),
            );
        }
    }
    if let Some(di) = drift_info {
        let json = serde_json::json!({
            "kind": di.kind.as_str(),
            "source": di.source_ctx.to_hex(),
            "target": di.target_ctx.to_hex(),
            "source_model": di.source_model,
        });
        vars.insert(
            "KJ_DRIFT_INFO".into(),
            kaish_kernel::ast::Value::String(json.to_string()),
        );
    }

    // Caller-supplied vars (the composer's transport heartbeat: $TICK/$PHRASE/
    // $TEMPO). Folded in last; a deliberate KJ_* collision would override, but
    // the heartbeat names don't use that prefix.
    for (k, v) in extra_vars {
        vars.insert(k.clone(), kaish_kernel::ast::Value::String(v.clone()));
    }

    // Every `.kai` script runs under the kernel-wide `rc_script_timeout`
    // budget. (Per-script overrides were dropped with the move to files;
    // a `kj` knob can re-introduce them later via frontmatter/sidecar.)
    let timeout = kaish.timeouts().rc_script_timeout;
    let opts = kaish_kernel::ExecuteOptions::new()
        .with_vars(vars)
        .with_timeout(timeout);
    match kaish.execute_with_options(&script.content, opts).await {
        Ok(exec) if exec.code == 0 => {
            // Capture stdout/stderr from a successful run into a Trace
            // block. Hidden from the LLM (Trace skips hydrate) but kept
            // in the conversation document for operator debugging and
            // potential downstream UI surfaces. No block at all when the
            // script was silent — avoids littering the doc with empties.
            let stdout = exec.text_out();
            if !stdout.is_empty() || !exec.err.is_empty() {
                insert_rc_trace_block(
                    dispatcher,
                    new_id,
                    &script.path,
                    &script.sort_key,
                    tail_output(&stdout, &exec.err),
                    principal,
                );
            }
        }
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

/// Insert a `BlockKind::Trace` block capturing the stdout/stderr of a
/// successful rc `.kai` script. Hidden from the LLM (the hydrator skips
/// `Trace` unconditionally) but available in the conversation document
/// for operator inspection.
fn insert_rc_trace_block(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    rc_path: &str,
    sort_key: &str,
    detail: String,
    principal: PrincipalId,
) {
    let summary = format!(
        "rc {sort_key} trace: {rc_path}\nrc_path: {rc_path}\nsort_key: {sort_key}\n\n{detail}"
    );
    let after = dispatcher.block_store().last_block_id(new_id);
    if let Err(e) = dispatcher.block_store().insert_block_as(
        new_id,
        None,
        after.as_ref(),
        Role::System,
        BlockKind::Trace,
        summary,
        Status::Done,
        ContentType::Plain,
        Some(principal),
    ) {
        tracing::error!(
            "rc lifecycle: could not insert trace block for {rc_path}: {e}"
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

/// Real `BlockSource` over the kernel's `SharedBlockStore`: hands `kj`'s
/// synthesis/search tools the context's block snapshots, hydrating from the DB
/// on an in-memory miss. This is what the model's `shell` / `read_only_shell`
/// materialize with (via [`crate::kj::KjDispatcher::block_source`]), so the
/// model gets full `kj search`/synthesis. rc and hook control-plane scripts
/// keep [`NoopBlockSource`] — they don't search.
///
/// Mirrors the server's `BlockStoreSource` (rpc.rs), kept kernel-local so the
/// in-kernel shell path doesn't reach across crates for a 10-line adapter; both
/// wrap the same `SharedBlockStore` API.
pub(crate) struct BlockStoreSource(pub(crate) crate::block_store::SharedBlockStore);

impl kaijutsu_index::BlockSource for BlockStoreSource {
    fn block_snapshots(
        &self,
        ctx: kaijutsu_types::ContextId,
    ) -> Result<Vec<kaijutsu_types::BlockSnapshot>, String> {
        use crate::block_store::BlockStore;
        // In-memory first; hydrate from the DB on demand for a cold context.
        if !self.0.contains(ctx) {
            let _ = self.0.load_one_from_db(ctx);
        }
        BlockStore::block_snapshots(&self.0, ctx).map_err(|e| e.to_string())
    }
}

fn log_unwired_verb_once(verb: &str) {
    // All four verbs (create / fork / drift / attach) are now wired.
    // Reserved-verb logging stays as a no-op for now so a future
    // not-yet-wired verb can plug in here without touching the call
    // site at lifecycle.rs:86.
    let _ = verb;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    /// Install an rc script as a file in the mounted `/etc/rc` tree. The
    /// structural args (type/verb/sort/name/ext) are redundant now that the
    /// path encodes them — kept so existing call sites stay unchanged — and
    /// only `path` + `content` are used.
    async fn install_script(
        dispatcher: &KjDispatcher,
        path: &str,
        _context_type: &str,
        _verb: &str,
        _sort_key: &str,
        _name: &str,
        _ext: &str,
        content: &str,
    ) {
        install_rc_script_file(dispatcher, path, content).await;
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// Caller with no joined context — `kj context create` without
    /// `--parent` resolves to `None` rather than the test caller's fake
    /// id, avoiding a FK violation on the forked_from column.
    /// Privileged so these rc-lifecycle tests can `kj context create` (now
    /// Operator-gated) as the trusted bootstrap/control plane would. The
    /// `context_id: None` models dispatching before a context is joined.
    fn unjoined_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: None,
            session_id: kaijutsu_types::SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: true,
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
        db.find_context_by_label(label)
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
        ).await;
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
        ).await;
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

    /// A successful `.kai` script that prints to stdout lands its output
    /// in a `BlockKind::Trace` block (model-hidden, operator-visible).
    /// Silent scripts must NOT produce a Trace block — only emit when
    /// there's something to capture.
    #[tokio::test]
    async fn rc_kai_stdout_captured_as_trace_block() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-echo.kai",
            "test",
            "create",
            "S00",
            "echo",
            "kai",
            "echo \"hello from rc\"",
        ).await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-echo", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-echo");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace_blocks: Vec<_> = snapshots
            .iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .collect();
        assert_eq!(
            trace_blocks.len(),
            1,
            "expected 1 trace block from echoing script; kinds: {:?}",
            snapshots.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
        let body = &trace_blocks[0].content;
        assert!(
            body.contains("hello from rc"),
            "trace block must contain the stdout, got: {body}"
        );
        assert!(
            body.contains("S00-echo.kai") || body.contains("S00"),
            "trace block must reference the script path or sort key, got: {body}"
        );
    }

    /// The composer transport seam: `run_rc_lifecycle_with_vars` must seed the
    /// extra vars into the `.kai` env so a `tick` script can read `$TICK` /
    /// `$PHRASE` / `$TEMPO` and compose the turn's transport report. Echoes the
    /// vars and asserts they round-trip through the captured Trace block.
    #[tokio::test]
    async fn rc_lifecycle_with_vars_seeds_kai_env() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/tick/S00-report.kai",
            "test",
            "tick",
            "S00",
            "report",
            "kai",
            "echo \"tick=$TICK phrase=$PHRASE tempo=$TEMPO\"",
        )
        .await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-tick", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-tick");

        let vars: std::collections::HashMap<String, String> = [
            ("TICK".to_string(), "128".to_string()),
            ("PHRASE".to_string(), "8".to_string()),
            ("TEMPO".to_string(), "120".to_string()),
        ]
        .into_iter()
        .collect();
        d.run_rc_lifecycle_with_vars("tick", new_id, None, None, None, &vars, &caller)
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing tick script produced a trace block");
        assert!(
            trace.content.contains("tick=128 phrase=8 tempo=120"),
            "heartbeat vars must reach the .kai env, got: {}",
            trace.content
        );
    }

    /// Non-vacuity guard for the seam above: with NO extra vars, the same script
    /// sees empty `$TICK`/`$PHRASE`/`$TEMPO` — proving the assertion pins the
    /// seeding, not an always-populated env.
    #[tokio::test]
    async fn rc_lifecycle_without_vars_leaves_heartbeat_empty() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/tick/S00-report.kai",
            "test",
            "tick",
            "S00",
            "report",
            "kai",
            "echo \"tick=$TICK phrase=$PHRASE tempo=$TEMPO\"",
        )
        .await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-novars", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());
        let new_id = lookup_context_id(&d, "ctx-novars");

        // The plain lifecycle (no extra vars) leaves the heartbeat unset.
        d.run_rc_lifecycle("tick", new_id, None, None, None, &caller)
            .await
            .expect("tick lifecycle");

        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        let trace = snapshots
            .iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Trace)
            .expect("the echoing tick script produced a trace block");
        assert!(
            trace.content.contains("tick= phrase= tempo="),
            "without seeded vars the heartbeat must be empty, got: {}",
            trace.content
        );
    }

    /// Helper: seed a child context with one block so the default hydration
    /// marker (`last_block_id`) resolves, then run the fork lifecycle with the
    /// given fork kind against the REAL shipped composer fork-hydrate script.
    /// Returns the child's hydration policy after the lifecycle.
    async fn run_composer_fork_hydrate(
        fork_kind: ForkKind,
    ) -> (
        std::sync::Arc<KjDispatcher>,
        ContextId,
        Option<(kaijutsu_crdt::BlockId, u32)>,
    ) {
        // Arc + set_self_arc so the .kai script can reach the `kj` builtin
        // (the script runs `kj context hydrate`); see `rc_kai_can_call_kj`.
        let d = std::sync::Arc::new(test_dispatcher().await);
        d.set_self_arc();
        // Install the real shipped script under a test type so the fork verb
        // dispatches it — pins the test to the actual seeded body, not a copy.
        install_rc_script_file(
            &d,
            "/etc/rc/test/fork/S40-hydrate.kai",
            include_str!("../../../../assets/defaults/rc/composer/fork/S40-hydrate.kai"),
        )
        .await;

        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        let child = register_context(&d, Some("child"), Some(parent), principal);
        set_context_type(&d, child, "test");
        // The child needs a block for the default prefix marker to resolve.
        d.block_store()
            .create_document(child, crate::DocumentKind::Conversation, None)
            .unwrap();
        d.block_store()
            .insert_block_as(
                child,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "seed".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(principal),
            )
            .unwrap();

        let caller = caller_with_context(child);
        d.run_rc_lifecycle("fork", child, Some(parent), Some(fork_kind), None, &caller)
            .await
            .expect("fork lifecycle");
        // The script must not have errored (e.g. a denied/failed `kj` call).
        assert!(
            !block_kinds_in(&d, child).contains(&BlockKind::Error),
            "fork-hydrate script errored: {:?}",
            block_contents_in(&d, child)
        );
        let policy = d.kernel_db().lock().get_hydration_policy(child).unwrap();
        (d, child, policy)
    }

    /// A THIN fork (shallow) is the player-spawn path: the fork-hydrate script
    /// re-establishes the window on the lean child (it would otherwise drive at
    /// tempo with full history — the create-side script doesn't run on fork).
    #[tokio::test]
    async fn composer_fork_hydrate_windows_a_thin_fork() {
        let (_d, _child, policy) = run_composer_fork_hydrate(ForkKind::Shallow).await;
        match policy {
            Some((_marker, window)) => assert_eq!(window, 16, "thin fork gets the --window 16 guard"),
            None => panic!("a thin (shallow) fork must set a hydration window"),
        }
    }

    /// A FULL fork is a deliberate clone, not a player spawn: windowing it at the
    /// tail would pin its whole inherited log (the cost-bomb). The script leaves
    /// it un-windowed — full history stays live, no policy set.
    #[tokio::test]
    async fn composer_fork_hydrate_skips_a_full_clone() {
        let (_d, _child, policy) = run_composer_fork_hydrate(ForkKind::Full).await;
        assert!(
            policy.is_none(),
            "a full clone must NOT be windowed (it would pin the whole inherited log); got {policy:?}"
        );
    }

    #[tokio::test]
    async fn rc_kai_silent_success_inserts_no_trace_block() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-silent.kai",
            "test",
            "create",
            "S00",
            "silent",
            "kai",
            "true",
        ).await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-silent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-silent");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            !kinds.contains(&kaijutsu_types::BlockKind::Trace),
            "silent script must not produce trace block, got kinds: {kinds:?}"
        );
    }

    /// Trace blocks have `Role::System` but `BlockKind::Trace` — the
    /// hydrator must skip them so the model never sees rc operator
    /// telemetry. (Belt-and-suspenders against a future regression that
    /// widens the System-role carve-out to all kinds.)
    #[tokio::test]
    async fn rc_kai_trace_block_is_hidden_from_llm_hydrate() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-echo.kai",
            "test",
            "create",
            "S00",
            "echo",
            "kai",
            "echo MODEL_MUST_NOT_SEE_THIS",
        ).await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-hidden", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-hidden");
        let snapshots = d.block_store().block_snapshots(new_id).expect("snapshots");
        // Sanity: the trace block exists in the document.
        assert!(
            snapshots
                .iter()
                .any(|b| b.kind == kaijutsu_types::BlockKind::Trace
                    && b.content.contains("MODEL_MUST_NOT_SEE_THIS")),
            "trace block with sentinel must exist in document"
        );
        // The hydrator must not surface it.
        let msgs = crate::llm::hydrate_from_blocks(&snapshots);
        for m in &msgs {
            let rendered = format!("{:?}", m);
            assert!(
                !rendered.contains("MODEL_MUST_NOT_SEE_THIS"),
                "trace content leaked into hydrated message: {rendered}"
            );
        }
    }

    /// End-to-end: a slow rc `.kai` script must time out, terminate any
    /// running child, and land a failure block — without hanging the
    /// `context create` RPC reply. Exercises the full chain:
    ///   `kaijutsu_types::TimeoutPolicy`
    ///     → `Kernel::timeouts()`
    ///     → `EmbeddedKaish::with_identity` (kaish KernelConfig::request_timeout)
    ///     → `run_kai_script` (per-call ExecuteOptions::with_timeout)
    ///     → `kaish::Kernel::execute_with_options` (124 + wait_or_kill)
    ///     → `insert_rc_failure_block`.
    #[tokio::test]
    async fn rc_kai_script_timeout_inserts_failure_block() {
        let policy = kaijutsu_types::TimeoutPolicy {
            rc_script_timeout: std::time::Duration::from_millis(150),
            ..Default::default()
        };
        let d = test_dispatcher_with_timeouts(policy).await;

        // Sleep well past the 150ms bound so the timeout MUST fire. The
        // kaish `sleep` builtin honors `ctx.cancel`, so the timer-induced
        // cancel surfaces as exit 130; the kernel then maps the elapsed
        // timeout to exit 124 with a "timed out" message in stderr.
        install_script(
            &d,
            "/etc/rc/test/create/S00-slow.kai",
            "test",
            "create",
            "S00",
            "slow",
            "kai",
            "sleep 10",
        ).await;

        let caller = unjoined_caller();
        let started = std::time::Instant::now();
        let result = d
            .dispatch(
                &argv(&["context", "create", "ctx-slow", "--type", "test"]),
                &caller,
            )
            .await;
        let elapsed = started.elapsed();

        // Context creation succeeded (rc failures don't block creation —
        // the new context is "alive but degraded" per the SysV-style
        // semantics documented in lifecycle.rs).
        assert!(
            result.is_ok(),
            "context create should succeed even when rc script times out: {}",
            result.message()
        );

        // Did NOT block 10 seconds waiting for sleep — the timeout cut in.
        // Generous upper bound to absorb CI jitter; the actual budget is ~150ms.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "rc timeout must not block context create: elapsed={:?}",
            elapsed
        );

        // The new context now carries an Error block describing the timeout.
        let new_id = lookup_context_id(&d, "ctx-slow");
        let snapshots = d
            .block_store()
            .block_snapshots(new_id)
            .expect("block_snapshots");
        let error_blocks: Vec<_> = snapshots
            .iter()
            .filter(|b| b.kind == kaijutsu_types::BlockKind::Error)
            .collect();
        assert_eq!(
            error_blocks.len(),
            1,
            "expected exactly one error block, got {}: kinds={:?}",
            error_blocks.len(),
            snapshots.iter().map(|b| b.kind).collect::<Vec<_>>()
        );
        let body = &error_blocks[0].content;
        assert!(
            body.contains("S00-slow.kai") || body.contains("slow"),
            "error block should reference the failing script path: {body}"
        );
        assert!(
            body.to_lowercase().contains("timed out") || body.contains("124"),
            "error block should mention timeout (exit 124 or 'timed out'): {body}"
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
        ).await;
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
        ).await;
        install_script(
            &d,
            "/etc/rc/test/create/S10-after.md",
            "test",
            "create",
            "S10",
            "after",
            "md",
            "ran-after-failure",
        ).await;

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
    async fn rc_attach_fires_scripts_on_target() {
        let d = test_dispatcher().await;
        // `.md` script lands its content as a block on the target.
        install_script(
            &d,
            "/etc/rc/test/attach/S00-banner.md",
            "test",
            "attach",
            "S00",
            "banner",
            "md",
            "attach-banner-content",
        ).await;

        let principal = PrincipalId::new();
        let target = register_context(&d, Some("attach-target"), None, principal);
        set_context_type(&d, target, "test");

        let caller = caller_with_context(target);
        let res = d
            .run_rc_lifecycle("attach", target, None, None, None, &caller)
            .await;
        assert!(res.is_ok(), "attach lifecycle should succeed, got: {res:?}");

        let contents = block_contents_in(&d, target);
        assert!(
            contents.iter().any(|c| c.contains("attach-banner-content")),
            "attach .md script must land its content as a block; got: {contents:?}"
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
        ).await;
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

    /// Set a registered context's type to `t` so rc dispatch finds scripts
    /// under `/etc/rc/<t>/...`. `register_context` defaults to "default".
    fn set_context_type(d: &KjDispatcher, ctx: ContextId, t: &str) {
        let db = d.kernel_db().lock();
        db.update_context_type(ctx, t).expect("update_context_type");
    }

    /// Count blocks whose content contains `needle`.
    fn count_blocks_containing(d: &KjDispatcher, ctx: ContextId, needle: &str) -> usize {
        block_contents_in(d, ctx)
            .iter()
            .filter(|c| c.contains(needle))
            .count()
    }

    #[tokio::test]
    async fn rc_drift_pull_inserts_drift_then_runs_script() {
        let d = test_dispatcher().await;
        // .kai script asserts overlay vars look right for a Pull drift.
        // No `kj` calls — so set_self_arc is unnecessary.
        install_script(
            &d,
            "/etc/rc/test/drift/S00-introspect.kai",
            "test",
            "drift",
            "S00",
            "introspect",
            "kai",
            r#"
test -n "$KJ_VERB" || exit 1
test -n "$KJ_CONTEXT" || exit 2
test -n "$KJ_DRIFT_INFO" || exit 3
case "$KJ_VERB" in
  drift) ;;
  *) exit 4 ;;
esac
case "$KJ_DRIFT_INFO" in
  *'"kind":"pull"'*) ;;
  *) exit 5 ;;
esac
"#,
        ).await;

        let principal = PrincipalId::new();
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        let src = register_context(&d, Some("src"), None, principal);

        let caller = caller_with_context(dst);
        let res = d
            .run_rc_lifecycle(
                "drift",
                dst,
                None,
                None,
                Some(DriftInfo {
                    kind: DriftKind::Pull,
                    source_ctx: src,
                    target_ctx: dst,
                    source_model: Some("claude-opus-4-7".into()),
                }),
                &caller,
            )
            .await;
        assert!(res.is_ok(), "drift rc errored: {res:?}");

        let kinds = block_kinds_in(&d, dst);
        assert!(
            !kinds.contains(&BlockKind::Error),
            ".kai overlay-var assertions failed; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, dst),
        );
    }

    #[tokio::test]
    async fn rc_drift_merge_runs_with_target_overlay() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/drift/S00-introspect.kai",
            "test",
            "drift",
            "S00",
            "introspect",
            "kai",
            r#"
test -n "$KJ_VERB" || exit 1
case "$KJ_VERB" in
  drift) ;;
  *) exit 2 ;;
esac
case "$KJ_DRIFT_INFO" in
  *'"kind":"merge"'*) ;;
  *) exit 3 ;;
esac
"#,
        ).await;

        let principal = PrincipalId::new();
        let parent = register_context(&d, Some("parent"), None, principal);
        set_context_type(&d, parent, "test");
        let child = register_context(&d, Some("child"), Some(parent), principal);

        let caller = caller_with_context(child);
        let res = d
            .run_rc_lifecycle(
                "drift",
                parent,
                None,
                None,
                Some(DriftInfo {
                    kind: DriftKind::Merge,
                    source_ctx: child,
                    target_ctx: parent,
                    source_model: None,
                }),
                &caller,
            )
            .await;
        assert!(res.is_ok(), "drift rc errored: {res:?}");

        let kinds = block_kinds_in(&d, parent);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "merge .kai assertions failed; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, parent),
        );
    }

    #[tokio::test]
    async fn rc_drift_flush_fires_per_item() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/drift/S00-marker.md",
            "test",
            "drift",
            "S00",
            "marker",
            "md",
            "DRIFT-MARKER",
        ).await;

        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        // Flush requires a BlockStore document for the destination.
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let caller = caller_with_context(src);
        for content in ["one", "two", "three"] {
            let r = d
                .dispatch(
                    &argv(&["drift", "push", "dst", content]),
                    &caller,
                )
                .await;
            assert!(r.is_ok(), "push '{content}' failed: {}", r.message());
        }

        let r = d.dispatch(&argv(&["drift", "flush"]), &caller).await;
        assert!(r.is_ok(), "flush failed: {}", r.message());
        assert!(
            r.message().contains("flushed 3 drift"),
            "expected all 3 flushed, got: {}",
            r.message()
        );

        let marker_count = count_blocks_containing(&d, dst, "DRIFT-MARKER");
        assert_eq!(
            marker_count, 3,
            "expected 3 marker blocks, contents: {:?}",
            block_contents_in(&d, dst)
        );
    }

    #[tokio::test]
    async fn rc_drift_script_failure_inserts_error_continues_flush() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/drift/S00-fail.kai",
            "test",
            "drift",
            "S00",
            "fail",
            "kai",
            "exit 17",
        ).await;
        install_script(
            &d,
            "/etc/rc/test/drift/S10-after.md",
            "test",
            "drift",
            "S10",
            "after",
            "md",
            "AFTER-MARKER",
        ).await;

        let principal = PrincipalId::new();
        let src = register_context(&d, Some("src"), None, principal);
        let dst = register_context(&d, Some("dst"), None, principal);
        set_context_type(&d, dst, "test");
        d.block_store()
            .create_document(dst, crate::DocumentKind::Conversation, None)
            .unwrap();

        let caller = caller_with_context(src);
        for content in ["one", "two"] {
            d.dispatch(&argv(&["drift", "push", "dst", content]), &caller)
                .await;
        }

        let r = d.dispatch(&argv(&["drift", "flush"]), &caller).await;
        assert!(r.is_ok(), "flush failed: {}", r.message());
        assert!(
            r.message().contains("flushed 2 drift"),
            "expected both items reported injected (drift block landed; rc \
             failure does not block delivery), got: {}",
            r.message()
        );

        let kinds = block_kinds_in(&d, dst);
        let error_count = kinds
            .iter()
            .filter(|k| **k == BlockKind::Error)
            .count();
        assert_eq!(error_count, 2, "expected 1 Error per item; kinds: {kinds:?}");
        let after_count = count_blocks_containing(&d, dst, "AFTER-MARKER");
        assert_eq!(
            after_count, 2,
            "S10 must run after S00 fails per-item; contents: {:?}",
            block_contents_in(&d, dst)
        );
    }

    #[tokio::test]
    async fn rc_drift_compact_fork_does_not_double_fire() {
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/fork/S00-fork.md",
            "test",
            "fork",
            "S00",
            "fork-marker",
            "md",
            "FORK-MARKER",
        ).await;
        install_script(
            &d,
            "/etc/rc/test/drift/S00-drift.md",
            "test",
            "drift",
            "S00",
            "drift-marker",
            "md",
            "DRIFT-MARKER",
        ).await;

        let caller = unjoined_caller();
        let r = d
            .dispatch(
                &argv(&["context", "create", "parent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create parent failed: {}", r.message());
        let parent_id = lookup_context_id(&d, "parent");

        // `kj context create` writes the KernelDb context+document but
        // doesn't seed a BlockStore document unless the rc create
        // lifecycle inserted blocks. With no `create` script for
        // `test`, the BlockStore doc isn't created — seed it explicitly
        // so insert_block_as has somewhere to land.
        d.block_store()
            .create_document(parent_id, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Insert a block so --compact has something to distill (otherwise
        // the distillation path errors out before reaching rc).
        d.block_store()
            .insert_block_as(
                parent_id,
                None,
                None,
                Role::User,
                BlockKind::Text,
                "seed content for compact-fork distillation".to_string(),
                Status::Done,
                ContentType::Plain,
                Some(PrincipalId::system()),
            )
            .unwrap();

        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let r = d
            .dispatch(
                &argv(&["fork", "--name", "child", "--compact"]),
                &fork_caller,
            )
            .await;
        // Compact may fail in tests if no LLM is wired; if so, the rc
        // call doesn't fire and the test premise is moot. Skip cleanly.
        if !r.is_ok() {
            eprintln!(
                "skipping compact-fork rc check — fork --compact unavailable in test: {}",
                r.message()
            );
            return;
        }

        let child_id = lookup_context_id(&d, "child");
        let fork_marker = count_blocks_containing(&d, child_id, "FORK-MARKER");
        let drift_marker = count_blocks_containing(&d, child_id, "DRIFT-MARKER");
        assert!(
            fork_marker >= 1,
            "child should have FORK-MARKER from fork rc, got contents: {:?}",
            block_contents_in(&d, child_id)
        );
        assert_eq!(
            drift_marker, 0,
            "compact-fork must NOT fire drift rc on the new context; got {drift_marker} marker(s) in: {:?}",
            block_contents_in(&d, child_id)
        );
    }

    #[tokio::test]
    async fn rc_fork_exposes_parent_block_count() {
        // Verifies KJ_PARENT_BLOCK_COUNT carries the parent's
        // BlockStore size at fork time — the number rc-on-fork scripts
        // need to compute the MessageIndex(N - 1) fork-point cache
        // breakpoint. Captured from the parent's BlockStore (not the
        // child's) because the child's count already includes the
        // fork-marker block by the time this rc hook fires.
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/fork/S00-assert-parent-count.kai",
            "test",
            "fork",
            "S00",
            "assert-parent-count",
            "kai",
            // Three explicit assertions, each with a distinct exit code
            // so a regression points at the right one:
            //   exit 1 — env var missing
            //   exit 2 — env var not a positive integer
            //   exit 3 — env var doesn't match the seeded count of 3
            r#"
test -n "$KJ_PARENT_BLOCK_COUNT" || exit 1
case "$KJ_PARENT_BLOCK_COUNT" in
  ''|*[!0-9]*) exit 2 ;;
esac
case "$KJ_PARENT_BLOCK_COUNT" in
  3) ;;
  *) exit 3 ;;
esac
"#,
        ).await;

        // Parent context, typed "test" so the fork hook above fires.
        let caller = unjoined_caller();
        let r = d
            .dispatch(
                &argv(&["context", "create", "parent", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create parent failed: {}", r.message());
        let parent_id = lookup_context_id(&d, "parent");

        // `kj context create` writes KernelDb but doesn't seed the
        // BlockStore unless an rc-on-create script does. Seed it
        // explicitly so we can insert exactly the count we want.
        d.block_store()
            .create_document(parent_id, crate::DocumentKind::Conversation, None)
            .unwrap();

        // Seed exactly 3 blocks. The rc script's case match pins this.
        for content in ["a", "b", "c"] {
            d.block_store()
                .insert_block_as(
                    parent_id,
                    None,
                    None,
                    Role::User,
                    BlockKind::Text,
                    content.to_string(),
                    Status::Done,
                    ContentType::Plain,
                    Some(PrincipalId::system()),
                )
                .unwrap();
        }
        assert_eq!(
            d.block_store()
                .block_snapshots(parent_id)
                .unwrap()
                .len(),
            3,
            "parent must have exactly 3 blocks before fork — drives the rc script's case match"
        );

        // Fork. Parent's blocks copy into the child, then the fork
        // marker injects (taking the child's count to >3), then
        // rc-on-fork runs and reads KJ_PARENT_BLOCK_COUNT.
        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
        let r = d
            .dispatch(&argv(&["fork", "--name", "child"]), &fork_caller)
            .await;
        assert!(r.is_ok(), "fork failed: {}", r.message());

        let child_id = lookup_context_id(&d, "child");
        let kinds = block_kinds_in(&d, child_id);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "rc-on-fork assertions tripped; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, child_id)
        );
    }

    #[tokio::test]
    async fn rc_create_omits_parent_block_count() {
        // KJ_PARENT_BLOCK_COUNT is fork-only — rc-on-create has no
        // parent, so the var must be absent (not "0", not "").
        let d = test_dispatcher().await;
        install_script(
            &d,
            "/etc/rc/test/create/S00-no-parent-count.kai",
            "test",
            "create",
            "S00",
            "no-parent-count",
            "kai",
            // Exit 99 if the var is set to anything. Empty/unset env
            // vars in kaish expand to empty string under `$VAR`, so
            // `test -z` catches both.
            r#"
test -z "$KJ_PARENT_BLOCK_COUNT" || exit 99
"#,
        ).await;

        let caller = unjoined_caller();
        let r = d
            .dispatch(
                &argv(&["context", "create", "solo", "--type", "test"]),
                &caller,
            )
            .await;
        assert!(r.is_ok(), "create failed: {}", r.message());

        let id = lookup_context_id(&d, "solo");
        let kinds = block_kinds_in(&d, id);
        assert!(
            !kinds.contains(&BlockKind::Error),
            "create rc must not see KJ_PARENT_BLOCK_COUNT; kinds: {kinds:?}, contents: {:?}",
            block_contents_in(&d, id)
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
        ).await;
        install_script(
            &d,
            "/etc/rc/test/fork/S00-only-fork.md",
            "test",
            "fork",
            "S00",
            "only-fork",
            "md",
            "FORK-MARKER",
        ).await;

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
        // Privileged: the parent was made via `kj context create` (deny-by-
        // default), so a plain caller would be refused by the `fork` gate. This
        // test exercises fork mechanics, not the capability check.
        let fork_caller = KjCaller {
            privileged: true,
            ..caller_with_context(parent_id)
        };
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

    /// All `.kai` scripts run under the kernel-wide `rc_script_timeout`
    /// (per-script overrides were dropped with the move to files). Pin it
    /// to 200ms, well under the script's 1s sleep, and confirm the runaway
    /// script is killed with an Error block and never completes.
    #[tokio::test]
    async fn rc_kernel_default_timeout_kills_runaway_script() {
        let mut policy = kaijutsu_types::TimeoutPolicy::default();
        policy.rc_script_timeout = std::time::Duration::from_millis(200);
        let d = crate::kj::test_helpers::test_dispatcher_with_timeouts(policy).await;

        install_script(
            &d,
            "/etc/rc/test/create/S00-slow.kai",
            "test",
            "create",
            "S00",
            "slow",
            "kai",
            "sleep 1 && echo never-reached",
        )
        .await;

        let caller = unjoined_caller();
        let result = d
            .dispatch(&argv(&["context", "create", "ctx-default-kills", "--type", "test"]), &caller)
            .await;
        assert!(result.is_ok(), "create failed: {}", result.message());

        let new_id = lookup_context_id(&d, "ctx-default-kills");
        let kinds = block_kinds_in(&d, new_id);
        assert!(
            kinds.contains(&kaijutsu_types::BlockKind::Error),
            "200ms kernel default must kill 1s sleep; kinds: {kinds:?}"
        );
        let contents = block_contents_in(&d, new_id);
        assert!(
            !contents.iter().any(|c| c.contains("never-reached")),
            "script body must not have completed; contents: {contents:?}"
        );
    }
}

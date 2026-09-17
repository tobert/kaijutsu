//! Run-control (rc) lifecycle dispatch.
//!
//! Runs at create, fork, attach, drift, tick, rotate, and submit. Scripts under
//! `/config/rc/<context_type>/<verb>/SXX-name.kai` run in lexical order.
//! Other files are data. Scripts use the shared runtime constructor
//! with rc authority and `TimeoutPolicy::rc_script_timeout`.
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

use crate::runtime::context_shell::{ShellCwd, ShellIdentity, ShellPolicy};
use crate::runtime::embedded_kaish::EmbeddedKaish;
use std::collections::HashMap;
use crate::runtime::synthesis::NoopBlockSource;

use approval_ledger::rc_runs;
use approval_ledger::types::RcOutcome;
use kaijutsu_types::paths;
use kaijutsu_types::{
    BlockId, BlockKind, ContentType, ContextId, DriftKind, ForkKind, PrincipalId, Role, Status,
};

use crate::kj::{KjCaller, KjDispatcher};

mod script_path;
pub use script_path::{RcPathParts, is_rc_script_filename, parse_rc_path};

/// Authority to construct the lifecycle control-plane shell.
///
/// The private field keeps ordinary execution callers from selecting rc policy.
///
/// ```compile_fail
/// use kaijutsu_kernel::rc::RcAuthority;
/// let authority = RcAuthority { _private: () };
/// ```
pub struct RcAuthority {
    _private: (),
}

#[cfg(test)]
impl RcAuthority {
    pub(crate) fn for_test() -> Self { Self { _private: () } }
}

/// One rc script resolved from the `/config/rc` file tree for a single
/// lifecycle run. Bodies are read through the VFS before execution starts.
pub(crate) struct RcScript {
    pub path: String,
    pub sort_key: String,
    pub content: String,
}

/// Per-drift metadata surfaced to rc scripts via `KJ_DRIFT_INFO`. Built by
/// drift call sites and carried by [`RcInvocation`].
#[derive(Clone, Debug)]
pub struct DriftInfo {
    pub kind: DriftKind,
    pub source_ctx: ContextId,
    pub target_ctx: ContextId,
    pub source_model: Option<String>,
}

/// The facts a chat submit hands its rc scripts. See docs/prompts.md,
/// "The submit verb".
#[derive(Clone, Debug)]
pub struct SubmitInfo {
    /// The user block the draft became.
    pub input_block: BlockId,
    /// The newest block the client had shown when the player submitted, if it said.
    pub edge_block: Option<BlockId>,
    /// Characters of `edge_block` shown, if it was still streaming.
    pub edge_shown: Option<u64>,
    /// The newest durable block in the log before `input_block`, if any.
    pub log_tail: Option<BlockId>,
    /// Whether a model turn was running when the submit arrived.
    pub turn_live: bool,
}

impl SubmitInfo {
    /// The script environment: KJ_INPUT_BLOCK, KJ_EDGE_BLOCK, KJ_EDGE_SHOWN,
    /// KJ_LOG_TAIL (each a block key via `BlockId::to_key`, "" when absent),
    /// KJ_TURN_LIVE ("true"/"false"). Every name is always set so a script
    /// can test emptiness without guarding definedness.
    pub fn vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("KJ_INPUT_BLOCK".to_string(), self.input_block.to_key());
        vars.insert(
            "KJ_EDGE_BLOCK".to_string(),
            self.edge_block.map(|b| b.to_key()).unwrap_or_default(),
        );
        vars.insert(
            "KJ_EDGE_SHOWN".to_string(),
            self.edge_shown.map(|n| n.to_string()).unwrap_or_default(),
        );
        vars.insert(
            "KJ_LOG_TAIL".to_string(),
            self.log_tail.map(|b| b.to_key()).unwrap_or_default(),
        );
        vars.insert("KJ_TURN_LIVE".to_string(), self.turn_live.to_string());
        vars
    }
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
/// cadence (e.g. every N bars for a musician). Its scripts are the per-beat work
/// hook — typically `kj drive` to request the next OODA turn. Materialized the
/// same throwaway-kaish way the other verbs are; no new runtime.
pub const VERB_TICK: &str = "tick";
/// The page-turn verb: fired by the beat scheduler when a context hits its rotate
/// horizon (`phrase % rotate_every == 0`). The scheduler has ALREADY stopped the
/// parent synchronously, so the rotate scripts (`kj fork --preset spawn` + arm +
/// play the child) run race-free — fork-lineage becomes song form
/// (`docs/chameleon.md`).
pub const VERB_ROTATE: &str = "rotate";
/// The submit verb: fired by the server after a chat submit has promoted the
/// player's draft to a durable user block. Scripts see the submit facts as
/// `KJ_*` variables ([`SubmitInfo::vars`]). Runs awaited inline like `drift`,
/// so anything a script writes is durable before `submitInput` returns.
pub const VERB_SUBMIT: &str = "submit";

/// Canonical verbs shared by lifecycle dispatch and script path validation.
pub const RC_VERBS: &[&str] = &[
    VERB_CREATE,
    VERB_FORK,
    VERB_ATTACH,
    VERB_DRIFT,
    VERB_TICK,
    VERB_ROTATE,
    VERB_SUBMIT,
];

fn verb_is_wired(verb: &str) -> bool {
    RC_VERBS.contains(&verb)
}

/// Facts captured for one lifecycle run. Extra variables are scoped to this run.
pub struct RcInvocation<'a> {
    pub verb: &'a str,
    pub context: ContextId,
    pub parent: Option<ContextId>,
    pub fork_kind: Option<ForkKind>,
    pub drift: Option<DriftInfo>,
    pub vars: HashMap<String, String>,
}

impl<'a> RcInvocation<'a> {
    pub fn new(verb: &'a str, context: ContextId) -> Self {
        Self { verb, context, parent: None, fork_kind: None, drift: None, vars: HashMap::new() }
    }
}

/// Run a context type's scripts after the triggering state change is committed.
/// Script failures are recorded and later scripts continue; invalid verbs and
/// context or discovery failures return an error to the caller.
#[tracing::instrument(
    skip(dispatcher, invocation, caller),
    fields(verb = %invocation.verb, ctx = %invocation.context.short(), rc_depth = caller.rc_depth),
)]
pub async fn run(
    dispatcher: &KjDispatcher,
    invocation: RcInvocation<'_>,
    caller: &KjCaller,
) -> Result<(), String> {
    let verb = invocation.verb;
    let new_id = invocation.context;
    if !verb_is_wired(verb) {
        return Err(format!("rc lifecycle: unknown verb '{verb}'; expected {}", RC_VERBS.join(", ")));
    }

    // Lifecycle diagnostics belong to the context creator. Commands inside
    // scripts author their blocks as the invoking performer.
    let (context_type, owner) = {
        let db = dispatcher.kernel_db().lock();
        match db.get_context(new_id) {
            Ok(Some(row)) => (row.context_type, row.created_by),
            Ok(None) => {
                return Err(format!(
                    "rc lifecycle: context {} not found",
                    new_id.short()
                ));
            }
            Err(e) => return Err(format!("rc lifecycle: {e}")),
        }
    };

    // Start the durable run record before discovery. RunGuard finishes it on
    // every exit, including cancellation. Ledger errors are logged while the
    // lifecycle continues; script execution is not conditional on bookkeeping.
    let run_id = {
        let db = dispatcher.kernel_db().lock();
        match rc_runs::start_run(db.conn_for_ledger(), new_id.as_bytes(), &context_type, verb) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(
                    "rc lifecycle: run log start_run failed (continuing unlogged): {e}"
                );
                None
            }
        }
    };
    let mut run_guard = RunGuard::new(dispatcher, run_id);

    let scripts = match load_scripts(dispatcher, &context_type, verb).await {
        Ok(s) => s,
        Err(e) => {
            // A loader error happens before the ordinary script path below
            // ensures the in-memory document. Create it now so the
            // diagnostic survives context creation and makes `rebind`
            // actionable instead of disappearing into tracing.
            match dispatcher
                .block_store()
                .create_document(new_id, kaijutsu_types::DocKind::Conversation, None)
            {
                Ok(()) | Err(crate::block_store::BlockStoreError::DocumentAlreadyExists(_)) => {
                    insert_rc_failure_block(
                        dispatcher,
                        new_id,
                        &paths::rc_dir(&context_type, verb),
                        "load",
                        None,
                        e.clone(),
                        owner,
                    );
                    run_guard.finish(RcOutcome::Failed);
                    return Err(e);
                }
                Err(document_error) => {
                    run_guard.finish(RcOutcome::Failed);
                    return Err(format!(
                        "{e}; could not create the document for its diagnostic: {document_error}"
                    ));
                }
            }
        }
    };

    // Recorded once, here: the set is snapshotted, so this is how many
    // scripts the run intends to execute. Fewer script rows than this at
    // read time means the run stopped early rather than that a script
    // failed — the two are otherwise identical in the log, both landing
    // as `Failed`.
    run_guard.record_script_count(scripts.len());

    if scripts.is_empty() {
        run_guard.finish(RcOutcome::Ok);
        return Ok(());
    }

    // The BlockStore document for this context may not exist yet —
    // context_create commits the KernelDb document but doesn't seed
    // the in-memory BlockStore (LLM stream / RPC handler creates it
    // lazily on first block). rc scripts insert blocks now, so we
    // must ensure the BlockStore doc exists.
    match dispatcher
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
            dispatcher,
            new_id,
            "<recursion-guard>",
            "S00",
            None,
            format!(
                "rc depth limit exceeded ({} >= {}); refusing to run {}/* scripts",
                caller.rc_depth,
                MAX_RC_DEPTH,
                paths::rc_dir(&context_type, verb)
            ),
            owner,
        );
        // The recursion guard already inserted a failure block and ran
        // NO scripts — that is a failed run, not a no-op.
        run_guard.finish(RcOutcome::Failed);
        return Ok(());
    }

    let mut any_script_failed = false;

    for script in &scripts {
        let script_started_at = now_millis();
        let result = run_kai_script(dispatcher, &invocation, &context_type, script, caller, owner).await;
        if matches!(result, ScriptRunResult::Failed { .. }) {
            any_script_failed = true;
        }
        run_guard.record_script(script, script_started_at, &result);
    }

    // SysV init.d semantics: one script failing does not stop the rest
    // (module docs), so the run's own outcome is "did anything fail
    // across the whole phase", not "did the last script fail".
    run_guard.finish(if any_script_failed { RcOutcome::Failed } else { RcOutcome::Ok });
    Ok(())
}

/// Load the rc scripts for `(context_type, verb)` from the `/config/rc`
/// file tree, ordered lexically by filename (which is exactly
/// `(sort_key, name)` order). A missing directory means "no scripts for
/// this verb" — the common case — and returns empty, not an error. A
/// read failure on a present file *is* surfaced: per the
/// crash-over-corruption stance an unreadable stance script is
/// corruption, not an empty default.
async fn load_scripts(
    dispatcher: &KjDispatcher,
    context_type: &str,
    verb: &str,
) -> Result<Vec<RcScript>, String> {
    use crate::vfs::{VfsError, VfsOps};

    let dir = paths::rc_dir(context_type, verb);
    let vfs = dispatcher.kernel().vfs();
    let entries = match vfs.readdir(std::path::Path::new(&dir)).await {
        Ok(e) => e,
        // Directory absent → no scripts for this (type, verb).
        Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(format!("rc lifecycle: readdir {dir}: {e}")),
    };

    // Include symlinks alongside regular files: an init.d-style link
    // (`coder/create/S10-binding.kai → lib/create/binding.kai`) composes a
    // shared script into this verb directory. The link name governs ordering;
    // read_all follows the link to capture the executable body.
    let candidates = entries
        .into_iter()
        .filter(|e| e.kind.is_file() || e.kind.is_symlink())
        .map(|e| e.name)
        .filter(|n| n.ends_with(".kai"));

    // Reject invalid executable names before running anything. Other files,
    // including Markdown, are ordinary data and are not read by discovery.
    let mut names: Vec<String> = Vec::new();
    for name in candidates {
        if !is_rc_script_filename(&name) {
            return Err(format!(
                "rc lifecycle: {dir}/{name} is not a valid rc script name; \
                 expected SXX-name.kai — use another extension for data"
            ));
        }
        names.push(name);
    }
    // Lexical filename sort == (sort_key, name) order: the filename is
    // `{sort_key}-{name}.{ext}`, so S00 < S10 and ties break on name.
    names.sort();

    let mut scripts = Vec::with_capacity(names.len());
    for name in names {
        let path = paths::rc_script_path(context_type, verb, &name);
        // Read straight through the VFS to the host rc tree (no
        // FileDocumentCache mirror). Any read failure on a file we just enumerated is
        // corruption — boot WITHOUT the stance is worse than failing loud
        // (stance = the model's ethical posture), so this stays fatal.
        let bytes = match vfs.read_all(std::path::Path::new(&path)).await {
            Ok(b) => b,
            Err(e) => return Err(format!("rc lifecycle: read {path}: {e}")),
        };
        let content = String::from_utf8(bytes)
            .map_err(|e| format!("rc lifecycle: read {path}: not valid UTF-8: {e}"))?;
        let sort_key = name.split_once('-').expect("validated rc name").0.to_string();
        scripts.push(RcScript {
            path,
            sort_key,
            content,
        });
    }
    Ok(scripts)
}
/// Whether one rc script's execution succeeded, for the run log
/// (`RunGuard::record_script`) and for the whole run's own pass/fail outcome.
/// `exit_code` is `None` when initialization or execution fails without one.
enum ScriptRunResult {
    Ok,
    Failed { exit_code: Option<i32> },
}

async fn run_kai_script(
    dispatcher: &KjDispatcher,
    invocation: &RcInvocation<'_>,
    context_type: &str,
    script: &RcScript,
    caller: &KjCaller,
    principal: PrincipalId,
) -> ScriptRunResult {
    use kaijutsu_types::SessionId;

    let new_id = invocation.context;
    let parent_id = invocation.parent;
    let fork_kind = invocation.fork_kind;
    let drift_info = invocation.drift.as_ref();
    let verb = invocation.verb;
    let child_depth = caller.rc_depth + 1;
    let extra_vars = &invocation.vars;

    // Each rc script runs in its own single-use context shell — a snapshot of
    // the context's durable state (env + cwd). Scripts evolve durable state
    // only through the explicit `kj context set` channel, so later scripts in
    // the phase see earlier ones' deliberate writes, never their transients.
    // rc uses the bare kj surface (no semantic index): `NoopBlockSource`.
    let kaish = match EmbeddedKaish::for_context(
        dispatcher,
        "rc",
        ShellIdentity {
            requester: principal, performer: caller.actor_id, reviewer: caller.reviewer_id,
            context: new_id, session: SessionId::new(),
        },
        ShellPolicy::Rc(RcAuthority { _private: () }), ShellCwd::Context,
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
            return ScriptRunResult::Failed { exit_code: None };
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
        "KJ_CONTEXT_TYPE".into(),
        kaish_kernel::ast::Value::String(context_type.to_string()),
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

    // Invocation overlays include transport and submit facts. Supplied values
    // take precedence over the standard lifecycle variables.
    for (k, v) in extra_vars {
        vars.insert(k.clone(), kaish_kernel::ast::Value::String(v.clone()));
    }

    kaish.set_positional(&script.path, Vec::new()).await;

    // Apply the kernel's rc timeout independently to each script.
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
            ScriptRunResult::Ok
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
            ScriptRunResult::Failed { exit_code: Some(exec.code as i32) }
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
            ScriptRunResult::Failed { exit_code: None }
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

/// Current time as Unix milliseconds, for the run log's per-script
/// `started_at`/`finished_at`. `approval_ledger::time::now_millis` (what
/// `rc_runs::finish_run` uses for the run row itself) is `pub(crate)` to
/// that crate, so this is a small local twin rather than a cross-crate
/// visibility change for one call site. Both timestamps are captured here
/// in Rust, not left to `rc_run_scripts.started_at`'s SQL `DEFAULT` — that
/// default only fires at INSERT time, which is after the script already
/// ran, so relying on it would make `started_at` always >= `finished_at`.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Finish a run explicitly or mark it failed when execution drops the guard.
/// `run_id` is absent when starting the ledger record failed.
struct RunGuard<'a> {
    dispatcher: &'a KjDispatcher,
    run_id: Option<String>,
}

impl<'a> RunGuard<'a> {
    fn new(dispatcher: &'a KjDispatcher, run_id: Option<String>) -> Self {
        Self { dispatcher, run_id }
    }

    /// Record how many scripts this run intends to execute, best-effort for
    /// the same reason [`RunGuard::record_script`] is: the run log rides
    /// alongside the lifecycle and never gates it.
    fn record_script_count(&self, count: usize) {
        let Some(run_id) = self.run_id.as_deref() else {
            return;
        };
        let db = self.dispatcher.kernel_db().lock();
        if let Err(e) = rc_runs::set_run_script_count(db.conn_for_ledger(), run_id, count) {
            tracing::warn!("rc lifecycle: run log set_run_script_count failed for {run_id}: {e}");
        }
    }

    /// Record one script's execution in the run log, best-effort. A failure
    /// here degrades to a `tracing::warn!` for the same reason `start_run`'s
    /// does: this is observability riding alongside the lifecycle, not
    /// gating it, so a second database's hiccup must never cost a context
    /// its rc scripts.
    ///
    /// `started_at` must be captured by the caller immediately before the
    /// script ran — this method (and the INSERT it drives) only happens
    /// *after* the script has already finished, so leaving `started_at` to
    /// the SQL `DEFAULT` would stamp it at insert time and make it
    /// impossible for `started_at` to precede `finished_at`.
    fn record_script(&self, script: &RcScript, started_at: i64, result: &ScriptRunResult) {
        let Some(run_id) = self.run_id.as_deref() else {
            return;
        };
        let db = self.dispatcher.kernel_db().lock();
        let conn = db.conn_for_ledger();
        let sha256 = match rc_runs::insert_script_body(conn, &script.content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "rc lifecycle: run log insert_script_body failed for {}: {e}",
                    script.path
                );
                return;
            }
        };
        let exit_code = match result {
            ScriptRunResult::Ok => Some(0i64),
            ScriptRunResult::Failed { exit_code } => exit_code.map(i64::from),
        };
        if let Err(e) = rc_runs::record_run_script(
            conn,
            run_id,
            &script.path,
            &sha256,
            exit_code,
            started_at,
            Some(now_millis()),
        ) {
            tracing::warn!(
                "rc lifecycle: run log record_run_script failed for {}: {e}",
                script.path
            );
        }
    }

    /// Finish the run with `outcome`. Idempotent — the first call clears
    /// `run_id`, so a later call (including the one from `Drop` on the
    /// ordinary path) is a no-op rather than a second write against an
    /// already-finished row (which `finish_run` refuses loudly; swallowing
    /// that here would be exactly the silent-fallback CLAUDE.md warns
    /// against, so this avoids it structurally instead).
    fn finish(&mut self, outcome: RcOutcome) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        let db = self.dispatcher.kernel_db().lock();
        if let Err(e) = rc_runs::finish_run(db.conn_for_ledger(), &run_id, outcome) {
            tracing::warn!("rc lifecycle: run log finish_run failed for {run_id}: {e}");
        }
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        // Only reached if some exit path forgot to call `finish` explicitly —
        // see the struct doc for why the backstop outcome is `Failed`.
        self.finish(RcOutcome::Failed);
    }
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
    insert_rc_output_block(
        dispatcher,
        new_id,
        BlockKind::Error,
        summary,
        Status::Error,
        principal,
        rc_path,
        "failure",
    );
}

/// Insert one rc capture block, projecting ANSI out of it first.
///
/// The RC boot aesthetic (docs/ansi-and-beyond.md) means these are the blocks
/// most likely to be *deliberately* colorful — an rc script printing `[ OK ]`
/// in green is the feature, not an accident. So the whole assembled `summary`
/// (header lines plus the script's captured output) is what gets projected and
/// what gets stored as the original: span offsets address block content, and
/// the header prefix is part of that content. Projecting the detail alone
/// would leave every offset short by the header's length.
///
/// The spans arrive as a follow-up `set_style_spans` rather than riding the
/// inserted snapshot. That is one extra journal op on a path that runs once
/// per context creation, and it buys the ordering rule stated in
/// [`crate::ansi_ingest`] — text first, spans second — without every insert
/// helper in the kernel needing to grow a spans argument.
#[allow(clippy::too_many_arguments)]
fn insert_rc_output_block(
    dispatcher: &KjDispatcher,
    new_id: ContextId,
    kind: BlockKind,
    summary: String,
    status: Status,
    principal: PrincipalId,
    rc_path: &str,
    what: &str,
) {
    let projection = crate::ansi_ingest::project(summary.as_bytes());
    let original = projection.as_ref().map(|_| summary.clone());
    let content = match projection {
        Some(ref p) => p.text.clone(),
        None => summary,
    };
    let after = dispatcher.block_store().last_block_id(new_id);
    match dispatcher.block_store().insert_block_as(
        new_id,
        None,
        after.as_ref(),
        Role::System,
        kind,
        content,
        status,
        ContentType::Plain,
        Some(principal),
    ) {
        Ok(block_id) => {
            if let (Some(p), Some(original)) = (projection, original) {
                crate::ansi_ingest::record(
                    dispatcher.block_store(),
                    new_id,
                    &block_id,
                    p.spans,
                    original.as_bytes(),
                );
            }
        }
        Err(e) => {
            tracing::error!(
                "rc lifecycle: could not insert {what} block for {rc_path}: {e}"
            );
        }
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
    insert_rc_output_block(
        dispatcher,
        new_id,
        BlockKind::Trace,
        summary,
        Status::Done,
        principal,
        rc_path,
        "trace",
    );
}

#[cfg(test)]
mod tests;

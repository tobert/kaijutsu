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
//! exit code, and captured stderr/stdout. Subsequent scripts continue
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
use crate::runtime::admission::ContextAdmission;
use crate::runtime::embedded_kaish::EmbeddedKaish;
use std::collections::HashMap;
use crate::runtime::synthesis::NoopBlockSource;
use tokio_util::sync::CancellationToken;

use approval_ledger::rc_runs;
use approval_ledger::types::RcOutcome;
use kaijutsu_types::paths;
use kaijutsu_types::{
    BlockId, BlockKind, ContentType, ContextId, DriftKind, ForkKind, PrincipalId, Role, Status,
};

use crate::kj::{KjCaller, KjDispatcher};

pub(crate) mod settlement;
use settlement::ScriptExecution;

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
/// `KJ_*` variables ([`SubmitInfo::vars`]). Turn startup awaits script execution
/// and settlement before starting provider work.
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
    /// Proof that the lifecycle trigger was accepted while the context was live.
    pub admission: &'a ContextAdmission,
    pub parent: Option<ContextId>,
    pub fork_kind: Option<ForkKind>,
    pub drift: Option<DriftInfo>,
    pub vars: HashMap<String, String>,
    /// Signals active execution and stops discovery or later scripts.
    pub cancel: CancellationToken,
}

impl<'a> RcInvocation<'a> {
    pub fn new(
        verb: &'a str,
        admission: &'a ContextAdmission,
        owner: &CancellationToken,
    ) -> Self {
        Self {
            verb,
            admission,
            parent: None,
            fork_kind: None,
            drift: None,
            vars: HashMap::new(),
            cancel: owner.child_token(),
        }
    }
}

/// Run a context type's scripts after the triggering state change is committed.
/// Script failures are recorded and later scripts continue. Cancellation stops
/// the lifecycle after active execution cleans up. Invalid verbs, unavailable
/// context state, and discovery or settlement failures return an error.
#[tracing::instrument(
    skip(dispatcher, invocation, caller),
    fields(verb = %invocation.verb, ctx = %invocation.admission.context().short(), rc_depth = caller.rc_depth),
)]
pub async fn run(
    dispatcher: &KjDispatcher,
    invocation: RcInvocation<'_>,
    caller: &KjCaller,
) -> Result<(), String> {
    let verb = invocation.verb;
    let new_id = invocation.admission.context();
    if !verb_is_wired(verb) {
        return Err(format!("rc lifecycle: unknown verb '{verb}'; expected {}", RC_VERBS.join(", ")));
    }

    dispatcher.kernel().rc_settlements().retry_pending()?;

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

    let run_id = {
        let db = dispatcher.kernel_db().lock();
        rc_runs::start_run(db.conn_for_ledger(), new_id.as_bytes(), &context_type, verb)
            .map_err(|error| format!("rc lifecycle {verb}: could not record admission: {error}"))?
    };
    let mut guard = RunGuard::new(dispatcher, run_id.clone());
    let result = run_lifecycle(dispatcher, &invocation, caller, &context_type, owner, &run_id).await;
    let outcome = result.as_ref().copied().unwrap_or(RcOutcome::Failed);
    let settled = guard.finish(outcome);
    match (result, settled) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(settlement)) => Err(format!("{error}; {settlement}")),
    }
}

async fn run_lifecycle(
    dispatcher: &KjDispatcher,
    invocation: &RcInvocation<'_>,
    caller: &KjCaller,
    context_type: &str,
    owner: PrincipalId,
    run_id: &str,
) -> Result<RcOutcome, String> {
    let verb = invocation.verb;
    let new_id = invocation.admission.context();
    if invocation.cancel.is_cancelled() {
        return Err(format!("rc lifecycle {verb} cancelled before script discovery"));
    }

    let scripts = match tokio::select! {
        biased;
        _ = invocation.cancel.cancelled() => {
            return Err(format!("rc lifecycle {verb} cancelled during script discovery"));
        }
        scripts = load_scripts(dispatcher, context_type, verb) => scripts,
    } {
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
                    )?;
                    return Err(e);
                }
                Err(document_error) => {
                    return Err(format!(
                        "{e}; could not create the document for its diagnostic: {document_error}"
                    ));
                }
            }
        }
    };

    // The count records the snapshotted source set before execution. Readers
    // compare it with begun script rows to distinguish an early stop from a
    // completed lifecycle whose scripts returned nonzero exits.
    {
        let db = dispatcher.kernel_db().lock();
        rc_runs::set_run_script_count(db.conn_for_ledger(), run_id, scripts.len())
            .map_err(|error| format!("rc lifecycle {verb}: could not record script count: {error}"))?;
    }

    if invocation.cancel.is_cancelled() {
        return Err(format!("rc lifecycle {verb} cancelled after script discovery"));
    }

    if scripts.is_empty() {
        return Ok(RcOutcome::Ok);
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
        Err(e) => return Err(format!("rc lifecycle: could not prepare the context document: {e}")),
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
        )?;
        // The recursion guard already inserted a failure block and ran
        // NO scripts — that is a failed run, not a no-op.
        return Ok(RcOutcome::Failed);
    }

    let mut any_script_failed = false;

    for script in &scripts {
        if invocation.cancel.is_cancelled() {
            return Err(format!(
                "rc lifecycle {verb} cancelled before {}",
                script.path
            ));
        }
        let script_started_at = now_millis();
        let seq = {
            let db = dispatcher.kernel_db().lock();
            rc_runs::begin_run_script(db.conn_for_ledger(), run_id, &script.path, &script.content, script_started_at)
                .map_err(|error| format!("rc lifecycle {verb}: could not record {} before execution: {error}", script.path))?
        };
        let result = run_kai_script(dispatcher, invocation, context_type, script, caller, owner).await;
        let cancelled = result.cancelled();
        any_script_failed |= result.failed();
        dispatcher.kernel().rc_settlements().retain_and_project(run_id, seq, result)?;
        if cancelled {
            return Err(format!("rc lifecycle {verb} cancelled while running {}", script.path));
        }
        if invocation.cancel.is_cancelled() {
            return Err(format!("rc lifecycle {verb} cancelled after {}", script.path));
        }
    }

    // Ordinary script failures do not stop later scripts. Settlement faults do.
    Ok(if any_script_failed { RcOutcome::Failed } else { RcOutcome::Ok })
}

/// Load the rc scripts for `(context_type, verb)` from the `/config/rc`
/// file tree, ordered lexically by filename (which is exactly
/// `(sort_key, name)` order). A missing directory means "no scripts for
/// this verb" — the common case — and returns empty, not an error. A
/// read failure on a present file returns an error; it cannot mean an empty
/// lifecycle.
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
        scripts.push(RcScript {
            path,
            content,
        });
    }
    Ok(scripts)
}
async fn run_kai_script(
    dispatcher: &KjDispatcher,
    invocation: &RcInvocation<'_>,
    context_type: &str,
    script: &RcScript,
    caller: &KjCaller,
    principal: PrincipalId,
) -> ScriptExecution {
    let new_id = invocation.admission.context();
    let parent_id = invocation.parent;
    let fork_kind = invocation.fork_kind;
    let drift_info = invocation.drift.as_ref();
    let verb = invocation.verb;
    let child_depth = caller.rc_depth + 1;
    let extra_vars = &invocation.vars;

    if invocation.cancel.is_cancelled() {
        return ScriptExecution::NotRun { message: "rc lifecycle cancelled before contextual shell construction".into(), cancelled: true };
    }

    // Each rc script runs in its own single-use context shell — a snapshot of
    // the context's durable state (env + cwd). Scripts evolve durable state
    // only through the explicit `kj context set` channel, so later scripts in
    // the phase see earlier ones' deliberate writes, never their transients.
    // rc uses the bare kj surface (no semantic index): `NoopBlockSource`.
    let kaish = match tokio::select! {
        biased;
        _ = invocation.cancel.cancelled() => {
            return ScriptExecution::NotRun { message: "rc lifecycle cancelled during contextual shell construction".into(), cancelled: true };
        }
        kaish = EmbeddedKaish::for_context(
            dispatcher,
            "rc",
            ShellIdentity {
                requester: principal, performer: caller.actor_id, reviewer: caller.reviewer_id,
                context: new_id, session: caller.session_id,
            },
            ShellPolicy::Rc(RcAuthority { _private: () }), ShellCwd::Context,
            None,
            std::sync::Arc::new(NoopBlockSource),
        ) => kaish,
    } {
        Ok(k) => k,
        Err(e) => return ScriptExecution::NotRun { message: format!("rc lifecycle: kaish init failed: {e}"), cancelled: false },
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

    if invocation.cancel.is_cancelled() {
        return ScriptExecution::NotRun { message: "rc lifecycle cancelled before script execution".into(), cancelled: true };
    }

    // Apply the kernel's rc timeout independently to each script.
    let timeout = kaish.timeouts().rc_script_timeout;
    let opts = kaish_kernel::ExecuteOptions::new()
        .with_vars(vars)
        .with_cancel_token(invocation.cancel.clone())
        .with_timeout(timeout);
    // Do not select cancellation against this future: kaish owns child cleanup
    // (TERM, grace, KILL) and must finish it before the lifecycle reports done.
    let execution = kaish.execute_with_options(&script.content, opts).await;
    let cancelled = invocation.cancel.is_cancelled();
    match execution {
        Ok(result) => ScriptExecution::Completed { result, cancelled },
        Err(error) => ScriptExecution::Fault { message: format!("rc kaish exec error: {error}"), cancelled },
    }
}

/// Capture script start time before execution rather than at result insertion.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Finish only after script results are retained and projected. The kernel keeps
/// ownership of failed settlement writes; dropping execution records abandonment.
struct RunGuard<'a> {
    dispatcher: &'a KjDispatcher,
    run_id: Option<String>,
}

impl<'a> RunGuard<'a> {
    fn new(dispatcher: &'a KjDispatcher, run_id: String) -> Self {
        Self { dispatcher, run_id: Some(run_id) }
    }

    fn finish(&mut self, outcome: RcOutcome) -> Result<(), String> {
        let Some(run_id) = self.run_id.take() else { return Ok(()); };
        self.dispatcher.kernel().rc_settlements().finish_run(&run_id, outcome)
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish(RcOutcome::Abandoned) {
            tracing::error!("rc lifecycle abandonment remains unsettled: {error}");
        }
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
) -> Result<(), String> {
    // Pre-execution diagnostics have no parent command block. Keep them as
    // plain Error blocks; executed-script results use retained settlement.
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
    )
}

/// Commit a pre-execution diagnostic and any ANSI provenance together.
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
) -> Result<(), String> {
    let projection = crate::ansi_ingest::project(summary.as_bytes());
    let content = projection.as_ref().map_or_else(|| summary.clone(), |value| value.text.clone());
    let ansi = projection.as_ref().map(|value| (value.spans.clone(), summary.as_bytes()));
    dispatcher.block_store().append_block_recorded(new_id,
        crate::block_store::RecordedBlockInsert {
            role: Role::System, kind, content, status, content_type: ContentType::Plain,
            author: principal, ansi, shell: None,
        }, |_, _| Ok(())
    ).map(|_| ()).map_err(|error| format!(
        "{summary}\nrc lifecycle: could not retain {what} diagnostic for {rc_path}: {error}"
    ))
}

#[cfg(test)]
mod tests;

//! Embedded kaish executor using MountBackend + VFS adapters.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, routing I/O through the kaijutsu kernel's MountTable
//! for real filesystem access and VFS adapters for kernel blocks.
//!
//! # Architecture
//!
//! ```text
//! Kaijutsu runtime
//!     │
//!     └── EmbeddedKaish
//!             │
//!             ├── kaish::Kernel (in-process)
//!             │       │
//!             │       ├── /v/docs → KaijutsuFilesystem (kernel blocks)
//!             │       ├── /v/swap → SwapFilesystem (dirty file buffers, read-only)
//!             │       ├── /v/jobs, /v/cas → kaish builtins
//!             │       └── everything else → MountBackend
//!             │               │
//!             │               ├── File ops → MountTable → LocalBackend
//!             │               └── Tool calls → KaijutsuBackend
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to access both kernel blocks and real files,
//! with tool dispatch routed through the kernel's tool registry.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::Tool;
use kaish_kernel::output_limit::OutputLimitConfig;
use kaish_kernel::{
    ExecuteOptions, IgnoreConfig, Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig,
};

use crate::Kernel as KaijutsuKernel;
use crate::block_store::SharedBlockStore;
use kaijutsu_types::paths::{DOCS_ROOT, SWAP_ROOT};
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use super::docs_filesystem::KaijutsuFilesystem;
use super::kaish_backend::KaijutsuBackend;
use super::mount_backend::MountBackend;
use super::read_only_fs::ReadOnlyFs;
use super::swap_filesystem::SwapFilesystem;
use super::context_shell::ShellIdentity;
use super::context_engine::{SessionContextExt, SessionContextMap};

/// Embedded kaish executor backed by kernel blocks.
///
/// File access uses `MountBackend`; document access and tool dispatch use
/// `KaijutsuBackend`.
pub struct EmbeddedKaish {
    /// The embedded kaish kernel.
    kernel: KaishKernel,
    /// Kernel name/id.
    name: String,
    /// Invocation-local session map for context tracking.
    session_contexts: SessionContextMap,
    session_id: SessionId,
    /// Snapshot of the kaijutsu kernel's `TimeoutPolicy` at construction.
    /// Callers use `timeouts()` when supplying per-invocation `ExecuteOptions`.
    timeouts: kaijutsu_types::TimeoutPolicy,
}

/// Refuse tools whose effects cannot be limited to observation. Register these
/// after the invocation's tools so a read-only shell cannot replace the refusal.
struct ReadOnlyDeniedBuiltin {
    name: &'static str,
    reason: &'static str,
}

#[async_trait::async_trait]
impl Tool for ReadOnlyDeniedBuiltin {
    fn name(&self) -> &str { self.name }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(self.name, "Not available in a read-only kaijutsu shell; use shell_write.")
    }

    async fn execute(&self, _args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        ExecResult::failure(1, format!(
            "{} is not available in a read-only kaijutsu shell: {}; use shell_write", self.name, self.reason,
        ))
    }
}

/// Read-only replacement for kaish's `jobs`: listing is observation, while
/// cleanup changes the shared manager and therefore stays unavailable.
struct ReadOnlyJobsBuiltin;

#[async_trait::async_trait]
impl Tool for ReadOnlyJobsBuiltin {
    fn name(&self) -> &str { "jobs" }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new("jobs", "List context shell operations without changing them.")
    }

    async fn execute(&self, _args: ToolArgs, ctx: &mut dyn ToolCtx) -> ExecResult {
        let Some(ctx) = ctx.as_any_mut().downcast_mut::<kaish_kernel::tools::ExecContext>() else {
            return ExecResult::failure(1, "internal error: kernel builtin requires ExecContext");
        };
        let Some(manager) = &ctx.job_manager else {
            return ExecResult::success("(no job manager)");
        };
        let jobs = manager.list().await;
        let mut output = String::new();
        for job in jobs {
            output.push_str(&format!("[{}] {:?}: {}\n", job.id, job.status, job.command));
        }
        ExecResult::success(output)
    }
}

/// Whether a materialized shell may spawn host subprocesses, and the `$PATH`
/// it sees. Decided at materialization from the context's loadout (the `exec`
/// authority — see `Capability::Exec`): kaish's `subprocess` feature is
/// compiled in workspace-wide, so *every* shell must pass an explicit policy —
/// deny-by-default, never inherited from kaish's feature-driven default.
#[derive(Clone, Debug, Default)]
pub enum ExternalExec {
    /// No host subprocesses: unknown commands fail fast as `command not found`.
    /// Builtins, `kj`, and backend tools are unaffected.
    #[default]
    Deny,
    /// Host subprocess exec enabled. `path` seeds `$PATH` in the shell's scope
    /// (kaish never reads OS env); absolute paths work regardless of `path`.
    Allow { path: Option<String> },
}

/// Output limits selected by the consumer.
///
/// Kaish replaces oversized output with a preview and sets exit code 3, retaining
/// the command's code in `original_code`. That remap also reaches script `$?`.
/// Internal consumers need complete text and a larger cap; callers must reject
/// spilled output when a preview would corrupt the result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputProfile {
    /// Model-facing output: kaish's 8 KB agent limit and head/tail preview.
    #[default]
    Agent,
    /// Rc, hook, and editor output: a 4 MiB memory ceiling.
    Internal,
}

/// Memory ceiling for internal consumers; spill still requires caller handling.
const INTERNAL_OUTPUT_LIMIT_BYTES: usize = 4 * 1024 * 1024;

impl OutputProfile {
    /// Build the kaish config for this profile.
    fn to_config(self) -> OutputLimitConfig {
        match self {
            Self::Agent => OutputLimitConfig::agent(),
            Self::Internal => {
                let mut cfg = OutputLimitConfig::agent();
                cfg.set_limit(Some(INTERNAL_OUTPUT_LIMIT_BYTES));
                cfg
            }
        }
    }
}

impl EmbeddedKaish {
    /// Create a new embedded kaish executor with default identity.
    ///
    /// Uses `PrincipalId::system()` and a fresh `ContextId` for engine tests.
    /// Production callers use `for_context` to apply contextual policy.
    pub fn new(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
    ) -> Result<Self> {
        Self::with_identity(
            name,
            blocks,
            kernel,
            project_root,
            ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(),
                reviewer: None, context: ContextId::new(), session: SessionId::new() },
            crate::runtime::context_engine::session_context_map(),
            ExternalExec::Deny,
            OutputProfile::Agent,
            |_, _, _| {},
        )
    }

    /// Build the interpreter and adapters with a complete invocation identity.
    /// Context switches share the supplied session map. The tool callback uses
    /// the same map; contextual policy and durable scope are applied by
    /// `for_context` before production execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_identity(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        identity: ShellIdentity,
        session_contexts: SessionContextMap,
        external_exec: ExternalExec,
        output: OutputProfile,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        Self::with_identity_mode(
            name,
            blocks,
            kernel,
            project_root,
            identity,
            session_contexts,
            false,
            external_exec,
            output,
            configure_tools,
        )
    }

    /// Like [`Self::with_identity`] but the materialized shell is **read-only**:
    /// every filesystem mutation and every external command is refused by
    /// construction, while reads — real files *and* the kernel document views at
    /// `/v/docs` — still work. Used by the model `shell` tool.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_identity_read_only(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        identity: ShellIdentity,
        session_contexts: SessionContextMap,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        Self::with_identity_mode(
            name,
            blocks,
            kernel,
            project_root,
            identity,
            session_contexts,
            true,
            // Read-only never spawns: external exec is the sandbox's fourth
            // lever, held Deny by construction (no caller choice to get wrong).
            ExternalExec::Deny,
            // The read-only model shell uses the agent output limit.
            OutputProfile::Agent,
            configure_tools,
        )
    }

    /// Shared builder for [`Self::with_identity`] /
    /// [`Self::with_identity_read_only`]. When `read_only` is set, the
    /// `MountBackend` refuses every mutation, the `/v/*` document mounts are wrapped
    /// read-only, and external command execution is disabled — three structural
    /// levers, mirroring kaibo's read-only sandbox recipe (`sandbox.rs`) adapted
    /// to kaijutsu's *shared*, kernel-owned mount table.
    #[allow(clippy::too_many_arguments)]
    fn with_identity_mode(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        identity: ShellIdentity,
        session_contexts: SessionContextMap,
        read_only: bool,
        external_exec: ExternalExec,
        output: OutputProfile,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        let ShellIdentity { context: context_id, session: session_id, .. } = identity;
        session_contexts.entry(session_id).or_insert(context_id);

        // The kernel's own file cache — the same instance the MCP file tools
        // use, built once at kernel construction. Routing MountBackend
        // through it is the whole point of kaish — shell scripting on the
        // same documents.
        let file_cache = kernel.file_cache().clone();
        let docs_backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel.clone(),
            identity,
            session_contexts.clone(),
                    ));
        let mount_table = kernel.vfs().clone();

        // Read-only mode refuses every mutation at the MountBackend boundary
        // (real files + the FileDocumentCache), regardless of whether the
        // shared mount is writable. The `/v/*` document mounts bypass MountBackend,
        // so they're wrapped separately below.
        let mount_backend: Arc<dyn KernelBackend> = if read_only {
            Arc::new(MountBackend::new_read_only(
                mount_table,
                docs_backend.clone(),
                file_cache,
            ))
        } else {
            Arc::new(MountBackend::new(
                mount_table,
                docs_backend.clone(),
                file_cache,
            ))
        };

        let docs_fs = Arc::new(KaijutsuFilesystem::new(docs_backend));

        // `/v/swap` (docs/file-buffers.md): a read-only view over unflushed
        // file buffers, mirrored by real path under this kernel's identity
        // segment. Read-only by construction (`SwapFilesystem` refuses every
        // mutation itself), so — unlike `docs_fs` — it needs no
        // conditional `ReadOnlyFs` wrap for the read-only shell mode.
        let swap_fs = Arc::new(SwapFilesystem::new(
            kernel.kernel_db().clone(),
            kernel.file_cache().clone(),
            kernel.id(),
        ));

        // KaishConfig primarily sets the cwd and kernel name. The VFS mode
        // in the config is secondary to kaijutsu's MountTable — real filesystem
        // access is routed through MountBackend → MountTable → LocalBackend,
        // not through kaish's own VFS modes.
        //
        // `project_root` sets the cwd to a specific project directory (used by
        // MCP sessions that operate on a particular repo). When None, cwd
        // defaults to $HOME via `KaishConfig::named()`. The context's persisted
        // cwd is selected before construction by `for_context`, which validates
        // it against the backend namespace before returning the shell.
        // kaijutsu overrides the backend with MountBackend (see below), so the
        // config's vfs_mode is moot — what matters is the cwd and the agent-grade
        // ignore/output-limit presets (gitignore-aware walks + capped output).
        // Build them explicitly via the builder chain rather than a bundled
        // constructor so this survives kaish config-API churn.
        let mut config = KaishConfig::named(name)
            .with_ignore_config(IgnoreConfig::agent())
            // On Linux, tie direct external children to their spawning OS
            // thread so abrupt server death does not leave them running.
            .with_kill_children_on_parent_death(true)
            // Output cap by consumer, not by trust — see `OutputProfile`.
            // Model-facing shells keep kaish's 8 KB agent preset; rc/hook/
            // editor shells use the larger internal ceiling. Both can spill.
            .with_output_limit(output.to_config());
        if let Some(root) = project_root {
            config = config.with_cwd(root);
        }

        // Apply kernel-wide kaish-script default timeout. Per-call sites
        // (rc lifecycle, hook bodies, init scripts) can override via
        // `ExecuteOptions::with_timeout` for stricter per-context bounds.
        config.request_timeout = Some(kernel.timeouts().kaish_request_timeout);
        // Shells materialized for one context share kaish's job table, while
        // every call retains its own identity, backend, and tool registry.
        // A background program therefore remains observable after this
        // throwaway shell instance drops without crossing context boundaries.
        let context_jobs = kernel.context_job_manager(context_id);
        config = config.with_job_manager(context_jobs.clone());

        config.initial_vars = super::context_shell::initial_environment(&external_exec);
        config = config.with_allow_external_commands(matches!(external_exec, ExternalExec::Allow { .. }));

        // The kernel document view (`/v/docs`) mounts directly on the kaish VFS,
        // bypassing MountBackend. ReadOnlyFs refuses writes in read-only mode.
        let docs_mount: Arc<dyn kaish_kernel::vfs::Filesystem> = if read_only {
            Arc::new(ReadOnlyFs::new(docs_fs))
        } else {
            docs_fs
        };

        let ctx_for_tools = session_contexts.clone();
        let sid_for_tools = session_id;
        let timeouts = kernel.timeouts().clone();
        let kaish_kernel = KaishKernel::with_backend(
            mount_backend,
            config,
            |vfs| {
                vfs.mount_arc(DOCS_ROOT, docs_mount);
                vfs.mount_arc(SWAP_ROOT, swap_fs);
                if read_only {
                    vfs.mount_arc("/v/jobs", Arc::new(ReadOnlyFs::new(
                        Arc::new(kaish_kernel::JobFs::new(context_jobs.clone())),
                    )));
                }
            },
            |tools| {
                configure_tools(ctx_for_tools, sid_for_tools, tools);
                if read_only {
                    for name in ["kill", "bg", "fg"] {
                        tools.register(ReadOnlyDeniedBuiltin { name,
                            reason: "job control can change a shared context operation" });
                    }
                    for name in ["vi", "edit", "curl"] {
                        tools.register(ReadOnlyDeniedBuiltin { name,
                            reason: "the tool can change shared or remote state" });
                    }
                    tools.register(ReadOnlyJobsBuiltin);
                }
            },
        )?;

        Ok(Self {
            kernel: kaish_kernel,
            name: name.to_string(),
            session_contexts,
            session_id,
            timeouts,
        })
    }

    /// Execute kaish code with the given options.
    ///
    /// Single canonical entry point: `ExecuteOptions` carries the per-call
    /// vars overlay, timeout, and external cancellation token. With no
    /// options (`ExecuteOptions::default()`), the kaish kernel falls back to
    /// the kernel-wide `request_timeout` set by this factory from
    /// `Kernel::timeouts().kaish_request_timeout`.
    ///
    /// Every call also parents the kaish kernel's execution span onto the
    /// embedder's active OTel trace (W3C `traceparent`/`tracestate` pulled from
    /// the current `tracing` span via [`kaijutsu_telemetry::inject_trace_context`])
    /// so kernel spans are not orphaned. The wiring is a no-op when OTel is
    /// inactive (empty carrier) or when a caller has already set its own
    /// `traceparent` — see [`merge_trace_context`].
    /// Returns kaish's typed [`kaish_kernel::KernelError`] rather than
    /// flattening it into `anyhow`. The distinction is the whole point: a
    /// `Parse`/`Validation` error means the program was refused and NOTHING
    /// RAN, while `Execution` means a statement started and faulted. A caller
    /// that reports the first as a fault teaches a model to retry a command
    /// that will never work. `is_rejected()` is the predicate; `Display` is
    /// byte-identical either way, so a caller that only prints needs nothing.
    pub async fn execute_with_options(
        &self,
        code: &str,
        opts: ExecuteOptions,
    ) -> std::result::Result<ExecResult, kaish_kernel::KernelError> {
        let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
        let context_id = self.context_id().map(|cid| cid.to_string());
        let opts = merge_trace_context(opts, traceparent, tracestate, context_id);
        self.kernel.execute_with_options(code, opts).await
    }

    /// Capture the final result and report each completed statement to the
    /// command owner's raw job stream. Hooks consume the final capture.
    pub(crate) async fn execute_with_options_streaming(
        &self, code: &str, opts: ExecuteOptions,
        on_output: &mut (dyn FnMut(&ExecResult) + Send),
    ) -> std::result::Result<ExecResult, kaish_kernel::KernelError> {
        let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
        let context_id = self.context_id().map(|cid| cid.to_string());
        let opts = merge_trace_context(opts, traceparent, tracestate, context_id);
        self.kernel.execute_with_options_streaming(code, opts, on_output).await
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<kaish_kernel::ast::Value> {
        self.kernel.get_var(name).await
    }

    /// Set the invoked script name (`$0`) and its positional arguments.
    pub async fn set_positional(&self, script_name: &str, args: Vec<String>) {
        self.kernel.set_positional(script_name, args).await
    }

    /// Set a variable value.
    pub async fn set_var(&self, name: &str, value: kaish_kernel::ast::Value) {
        self.kernel.set_var(name, value).await
    }

    /// List all variable names.
    pub async fn list_vars(&self) -> Vec<String> {
        self.kernel
            .list_vars()
            .await
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Snapshot the shell's exported (env) variables as `(name, value)` string
    /// pairs, coerced with the same `value_to_string` a child process sees.
    /// Used to diff a command's effect on durable `context_env`.
    pub async fn exported_vars(&self) -> Vec<(String, String)> {
        self.kernel
            .exported_vars()
            .await
            .into_iter()
            .map(|(name, value)| {
                (name, kaish_kernel::interpreter::value_to_string(&value))
            })
            .collect()
    }

    /// Get the kernel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Snapshot of the kaijutsu kernel's `TimeoutPolicy` taken at this
    /// `EmbeddedKaish`'s construction. Callers that build per-call
    /// `ExecuteOptions` (rc lifecycle, hook bodies) read their bound from here.
    pub fn timeouts(&self) -> &kaijutsu_types::TimeoutPolicy {
        &self.timeouts
    }

    /// Update the context ID (e.g., after a context switch).
    ///
    /// Propagates to `KaijutsuBackend` via the shared map.
    pub fn set_context_id(&self, id: ContextId) {
        self.session_contexts.insert(self.session_id, id);
    }

    /// Read the current context ID. Returns None if none active.
    pub fn context_id(&self) -> Option<ContextId> {
        self.session_contexts.current(&self.session_id)
    }

    /// Get current working directory.
    pub async fn cwd(&self) -> std::path::PathBuf {
        self.kernel.cwd().await
    }

    /// Set current working directory.
    pub async fn set_cwd(&self, path: std::path::PathBuf) {
        self.kernel.set_cwd(path).await
    }

    /// Set cwd only if `path` resolves to a directory in the shell's backend
    /// (the VFS namespace `cd` validates against). Returns whether it changed.
    pub async fn try_set_cwd(&self, path: std::path::PathBuf) -> bool {
        self.kernel.try_set_cwd(path).await
    }

    /// Get the last execution result ($?).
    pub async fn last_result(&self) -> Option<ExecResult> {
        Some(self.kernel.last_result().await)
    }

    /// Cancel all running kaish execution (best-effort).
    ///
    /// Signals the kaish cancellation token, which causes any active
    /// `execute()` or `execute_streaming()` call to abort at its next
    /// yield point. Background jobs within the same session are also
    /// terminated when their containing pipeline is cancelled.
    pub fn cancel(&self) {
        self.kernel.cancel();
    }

    /// Apply durable exports through the shared environment restore path.
    pub(crate) async fn export_env_vars(&self, vars: &[crate::kernel_db::ContextEnvRow]) -> Result<()> {
        let values = context_env_values(vars)?.into_iter()
            .map(|(name, value)| (name, Some(value))).collect();
        self.restore_env_values(values).await
    }
}

impl EmbeddedKaish {
    /// Restore the values an ask's free variables held when it escalated:
    /// export each recorded value into the root frame the way durable
    /// `context_env` is seeded, and `unset` each name the ask recorded as
    /// absent. After this, a variable set since the human read the statement
    /// cannot change what runs. Rejects a name that is not a legal kaish
    /// identifier before anything is executed. `docs/gate-shape-b.md`.
    pub async fn apply_ask_env(&self, rows: &[approval_ledger::types::AskEnvRow]) -> Result<()> {
        let values = rows.iter().map(|row| (row.name.clone(),
            row.value.clone().map(kaish_kernel::ast::Value::String))).collect();
        self.restore_env_values(values).await
    }

    /// Values cross through typed overlays, never script interpolation. Temporary
    /// names must differ from every target, including unset targets: export writes
    /// an existing scope entry, and unset removes one. Otherwise either operation
    /// could alter the temporary frame instead of the persistent environment.
    async fn restore_env_values(
        &self,
        values: Vec<(String, Option<kaish_kernel::ast::Value>)>,
    ) -> Result<()> {
        if values.is_empty() { return Ok(()); }
        let mut names = std::collections::HashSet::with_capacity(values.len());
        for (name, _) in &values {
            if !is_valid_env_key(name) {
                anyhow::bail!("environment name {name:?} is not a valid identifier");
            }
            if !names.insert(name.as_str()) {
                anyhow::bail!("duplicate environment name {name:?} in captured values");
            }
        }
        let mut overlay = std::collections::HashMap::new();
        let mut script = String::new();
        let mut next_temp = 0;
        for (name, value) in &values {
            match value {
                Some(value) => {
                    let tmp = loop {
                        let candidate = format!("__kj_env_{next_temp}__");
                        next_temp += 1;
                        if !names.contains(candidate.as_str()) { break candidate; }
                    };
                    overlay.insert(tmp.clone(), value.clone());
                    script.push_str(&format!("export {name}=\"${tmp}\"\n"));
                }
                None => script.push_str(&format!("unset {name}\n")),
            }
        }
        let result = self.execute_with_options(&script, ExecuteOptions::new().with_vars(overlay)).await?;
        if result.code != 0 {
            anyhow::bail!("environment restore exited {}: {}", result.code, result.err);
        }
        Ok(())
    }
}

/// Validate durable export names before contextual construction or switching.
/// Identifiers follow kaish's export builtin: ASCII letter/underscore first,
/// then ASCII alphanumeric/underscore. Values remain typed strings.
pub(crate) fn context_env_values(
    vars: &[crate::kernel_db::ContextEnvRow],
) -> Result<Vec<(String, kaish_kernel::ast::Value)>> {
    vars.iter()
        .map(|v| {
            if !is_valid_env_key(&v.key) {
                anyhow::bail!("context_env: key {:?} is not a valid identifier", v.key);
            }
            Ok((
                v.key.clone(),
                kaish_kernel::ast::Value::String(v.value.clone()),
            ))
        })
        .collect()
}

/// ASCII-identifier check mirroring kaish's `export` builtin's `check_name`:
/// letter/underscore first, then alphanumeric/underscore.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Merge ambient W3C trace context (and a context-id baggage tag) into per-call
/// `ExecuteOptions`.
///
/// `traceparent`/`tracestate` come from
/// [`kaijutsu_telemetry::inject_trace_context`], which yields empty strings when
/// no OTel context is active. The rules, mirroring the W3C spec and kaish's
/// `ExecuteOptions` contract:
///
/// - **A caller-set `traceparent` is a full hand-off.** When `opts.traceparent`
///   is already populated, the caller owns this call's telemetry context end to
///   end; we touch nothing — not the parent, not baggage. No current caller does
///   this (rc lifecycle and hook bodies build their own `opts` but leave
///   `traceparent` unset, so they still get ambient context + baggage below); the
///   branch reserves the seam for an embedder that threads an external trace.
/// - **No ambient context is a true no-op.** An empty `traceparent` means OTel is
///   inactive (or no span is entered); we add nothing — *including* baggage — so
///   an OTel-off build never spuriously seeds a local trace root via baggage.
/// - `tracestate` is meaningless without a `traceparent`, so it rides along only
///   when we set the parent, and only if non-empty.
/// - `context_id`, when present, is added as `kj.context_id` baggage so every
///   downstream kaish span carries the kaijutsu context it ran for. We don't
///   clobber an existing entry.
fn merge_trace_context(
    mut opts: ExecuteOptions,
    traceparent: String,
    tracestate: String,
    context_id: Option<String>,
) -> ExecuteOptions {
    // The caller owns its telemetry context — hands off entirely (incl. baggage).
    if opts.traceparent.is_some() {
        return opts;
    }
    // No ambient context (OTel inactive) — stay a true no-op.
    if traceparent.is_empty() {
        return opts;
    }
    opts.traceparent = Some(traceparent);
    if !tracestate.is_empty() {
        opts.tracestate = Some(tracestate);
    }
    if let Some(cid) = context_id {
        opts.baggage.entry("kj.context_id".to_string()).or_insert(cid);
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use kaijutsu_types::paths::{CAS_ROOT, RC_ROOT};

    /// `Kernel::new_ephemeral` already opens its own `KernelDb` and builds
    /// `file_cache()` over it, so `EmbeddedKaish::new`'s `MountBackend` has
    /// somewhere durable to mark unsaved buffers with no further wiring.
    async fn test_kernel(name: &str) -> Arc<KaijutsuKernel> {
        Arc::new(KaijutsuKernel::new_ephemeral(name).await)
    }

    const TP: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    const TS: &str = "vendor=value";

    #[test]
    fn merge_trace_context_no_ambient_is_noop() {
        // OTel inactive → inject yields empty strings → nothing is touched,
        // including baggage. The execution path must stay a true no-op.
        let opts = merge_trace_context(
            ExecuteOptions::default(),
            String::new(),
            String::new(),
            Some("ctx-123".to_string()),
        );
        assert!(opts.traceparent.is_none());
        assert!(opts.tracestate.is_none());
        assert!(opts.baggage.is_empty());
    }

    #[test]
    fn merge_trace_context_sets_parent_state_and_baggage() {
        let opts = merge_trace_context(
            ExecuteOptions::default(),
            TP.to_string(),
            TS.to_string(),
            Some("ctx-123".to_string()),
        );
        assert_eq!(opts.traceparent.as_deref(), Some(TP));
        assert_eq!(opts.tracestate.as_deref(), Some(TS));
        assert_eq!(opts.baggage.get("kj.context_id").map(String::as_str), Some("ctx-123"));
    }

    #[test]
    fn merge_trace_context_respects_caller_parent() {
        // A caller that already set a traceparent keeps it untouched, and we
        // don't smuggle baggage in behind their back.
        let caller = ExecuteOptions::default().with_traceparent("caller-parent");
        let opts = merge_trace_context(
            caller,
            TP.to_string(),
            TS.to_string(),
            Some("ctx-123".to_string()),
        );
        assert_eq!(opts.traceparent.as_deref(), Some("caller-parent"));
        assert!(opts.tracestate.is_none());
        assert!(opts.baggage.is_empty());
    }

    #[test]
    fn merge_trace_context_empty_tracestate_omitted() {
        // tracestate is meaningless without a parent and absent when the SDK
        // produced none — set the parent, leave tracestate unset.
        let opts = merge_trace_context(
            ExecuteOptions::default(),
            TP.to_string(),
            String::new(),
            None,
        );
        assert_eq!(opts.traceparent.as_deref(), Some(TP));
        assert!(opts.tracestate.is_none());
        assert!(opts.baggage.is_empty());
    }

    #[tokio::test]
    async fn test_embedded_kaish_creation() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-agent").await;

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
    }

    /// Canary for kaish #367/#368. A command substitution in a NON-LAST
    /// pipeline stage used to destroy that stage's entire output — silently,
    /// at exit 0 — so `echo $(git rev-parse HEAD) | cut -c1-8` handed a model
    /// an empty string it then reasoned on. Fixed in kaish 0.15.0.
    ///
    /// **If this test fails, the kaish dependency has gone backward.** Check
    /// the `kaish-*` versions in the workspace `Cargo.toml`: they must be
    /// 0.15 or later, and a `path` dep pointing at a pre-0.15 worktree
    /// reintroduces the bug with nothing else to signal it. Do not delete or
    /// weaken this assertion to make a downgrade build.
    #[tokio::test]
    async fn command_substitution_survives_a_non_last_pipeline_stage() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-subst-pipe").await;
        let kaish = EmbeddedKaish::new("test-subst-pipe", blocks, kernel, None).unwrap();

        let result = kaish
            .execute_with_options("echo $(echo sub) | cat", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "sub",
            "a command substitution in a non-last pipeline stage must survive; \
             empty output here means the kaish dependency regressed below 0.15",
        );

        // Quoting did not save it either, and the literal text around the
        // substitution was lost with it — so assert the whole stage survives,
        // not just the substituted part.
        let quoted = kaish
            .execute_with_options("echo \"x:[$(echo C)]\" | cat", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            quoted.text_out().trim(),
            "x:[C]",
            "the whole stage's output must survive, not only the substitution",
        );
    }

    #[tokio::test]
    async fn test_execute_with_options_feeds_stdin() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-stdin").await;
        let kaish = EmbeddedKaish::new("test-stdin", blocks, kernel, None).unwrap();

        // `cat` with no operands reads stdin; the embedder seam (`with_stdin`)
        // must feed it through, and the trace-context wrapper must not drop it.
        let result = kaish
            .execute_with_options("cat", ExecuteOptions::default().with_stdin("piped-in\n"))
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "piped-in",
            "stdin from ExecuteOptions::with_stdin should reach the first reader",
        );
    }

    /// The kaish surface for init.d-style rc composition: an agent shell does
    /// `ln -s` over the `/config/rc` mount with a host-relative target, and
    /// `cat` through the link returns the *target's* content. This proves
    /// the path is wired end-to-end — kaish `ln`/`cat` builtins →
    /// MountBackend → MountTable → `LocalBackend` — with no rc-specific
    /// shell code. (KaijutsuBackend's `/docs/` block scheme is a separate
    /// thing and keeps its honest "not supported" stub.)
    #[tokio::test]
    async fn ln_s_over_rc_mount_creates_followable_link() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-ln").await;
        // Mount the production rc shape — an ordinary host directory — over
        // the same block store the shell uses.
        let dir = tempfile::tempdir().unwrap();
        kernel
            .mount(RC_ROOT, crate::vfs::LocalBackend::new(dir.path()))
            .await;
        let kaish = EmbeddedKaish::new("test-ln", blocks, kernel.clone(), None).unwrap();

        let run = |cmd: &'static str| {
            let k = &kaish;
            async move {
                k.execute_with_options(cmd, ExecuteOptions::default())
                    .await
                    .unwrap_or_else(|e| panic!("`{cmd}` failed: {e}"))
            }
        };

        // A shared script body, written once under a `lib` type.
        let r = run("echo shared-body > /config/rc/lib/create/binding.kai").await;
        assert!(r.ok(), "echo>: {}", r.text_out());
        // Compose it into a context type by symlink. `LocalBackend::symlink`
        // writes the target string verbatim as a real host symlink — a
        // host-relative target (what `reseed_rc_files` writes for the
        // embedded seed's own composition) resolves; a VFS-absolute one
        // would not.
        let r = run(
            "ln -s ../../lib/create/binding.kai /config/rc/coder/create/S10-binding.kai",
        )
        .await;
        assert!(r.ok(), "ln -s: {}", r.text_out());
        // `cat` through the link follows to the target's content.
        let r = run("cat /config/rc/coder/create/S10-binding.kai").await;
        assert_eq!(r.text_out().trim(), "shared-body");
        // `readlink` reports the raw target through the VFS directly. Not the
        // kaish `readlink` builtin here: it pre-checks `is_symlink()` via
        // `lstat`, which for `LocalBackend` resolves through `getattr`'s
        // `canonicalize()` — that follows a *resolvable* symlink to its
        // target before lstat-ing it, so it reports "not a symbolic link"
        // for exactly the working links this test cares about
        // (`VfsOps::readlink` itself has no such bug — see its own
        // doc comment; only the `getattr`-based pre-check does).
        use crate::vfs::VfsOps;
        let target = kernel
            .vfs()
            .readlink(std::path::Path::new("/config/rc/coder/create/S10-binding.kai"))
            .await
            .expect("readlink the real host symlink");
        assert_eq!(target.to_string_lossy(), "../../lib/create/binding.kai");
    }

    /// `/v/cas` regression: kaish 0.11's `VirtualOverlayBackend` reserved
    /// every `/v/*` path for its own
    /// (always-empty here) overlay regardless of whether the embedder had a
    /// real mount there, so `ls`/`cat /v/cas/...` through kaish silently saw
    /// nothing even with a live `CasFs` mount on the kernel `MountTable` — SFTP
    /// and `kj cas`, which bypass kaish's VFS, were unaffected, so the bug was
    /// kaish-shell-only. kaish 0.12 made `is_virtual_path` purely mount-coverage
    /// based, so an unclaimed `/v/*` path now falls through to the embedder's
    /// backend. This pins the fix so a future kaish bump can't silently
    /// reintroduce the shadow.
    #[tokio::test]
    async fn kaish_ls_and_cat_reach_the_real_cas_mount_at_v_cas() {
        use kaijutsu_cas::{ContentStore, FileStore};
        use tempfile::TempDir;

        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-cas").await;

        let dir = TempDir::new().unwrap();
        let store = Arc::new(FileStore::at_path(dir.path()));
        let hash = store.store(b"the quick brown fox", "text/plain").unwrap();
        kernel.mount(CAS_ROOT, crate::vfs::CasFs::new(store)).await;

        let kaish = EmbeddedKaish::new("test-cas", blocks, kernel, None).unwrap();
        let run = |cmd: String| {
            let k = &kaish;
            async move {
                k.execute_with_options(&cmd, ExecuteOptions::default())
                    .await
                    .unwrap_or_else(|e| panic!("`{cmd}` failed: {e}"))
            }
        };

        let ls = run(format!("ls {CAS_ROOT}/{}", hash.prefix())).await;
        assert!(
            ls.text_out().contains(&hash.to_string()),
            "ls should list the stored object through kaish, got: {}",
            ls.text_out()
        );

        let cat = run(format!("cat {CAS_ROOT}/{}/{}", hash.prefix(), hash)).await;
        assert_eq!(cat.text_out(), "the quick brown fox");
    }

    /// The shell idioms our rc scripts rely on for **failure detection**, run
    /// through the real embedded kaish.
    ///
    /// This exists because the assistant seat's tick script was written with
    /// `x="$(cmd)" || x="READ-FAIL"` — the obvious idiom, correct in bash, and
    /// **dead code in kaish**: a command-substitution assignment always
    /// reports success, so the `||` branch never runs. That script's only
    /// health signal was built on it, so it could never raise a turn and would
    /// have logged "no turn requested" forever while looking healthy. Nothing
    /// in the suite could see it, because nothing executed the idiom.
    ///
    /// So this pins the working form (`rc=$?` on the next line) AND the broken
    /// one, deliberately asserting kaish's current divergence rather than the
    /// behavior we would prefer. The `||` case is filed with Amy for a scope
    /// call; **if it gets fixed, this test fails** — which is the point. It
    /// should fail loudly and be updated on purpose, not silently start
    /// passing for a new reason.
    ///
    /// Guards `assets/defaults/rc/assistant/tick/S10-checkin.kai`.
    #[tokio::test]
    async fn rc_failure_detection_idioms_behave_as_the_scripts_assume() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-idioms").await;
        let kaish = EmbeddedKaish::new("test-idioms", blocks, kernel, None).unwrap();
        let run = |cmd: &str| {
            let k = &kaish;
            let cmd = cmd.to_string();
            async move {
                k.execute_with_options(&cmd, ExecuteOptions::default())
                    .await
                    .unwrap_or_else(|e| panic!("`{cmd}` failed: {e}"))
                    .text_out()
                    .to_string()
            }
        };

        // The WORKING idiom: capture rc on the next line and branch on it.
        let out = run(
            r#"v="$(cat /definitely/not/here)"; rc=$?; if [[ "$rc" -ne 0 ]]; then v="READ-FAIL"; fi; echo "[$v]""#,
        )
        .await;
        assert!(
            out.contains("[READ-FAIL]"),
            "rc=$? must detect a failed command substitution — this is the idiom \
             every rc probe depends on; got: {out}"
        );

        // The idiom must NOT false-positive on success.
        let ok = run(
            r#"v="$(echo alive)"; rc=$?; if [[ "$rc" -ne 0 ]]; then v="READ-FAIL"; fi; echo "[$v]""#,
        )
        .await;
        assert!(
            ok.contains("[alive]"),
            "a successful substitution must keep its value; got: {ok}"
        );

        // **FIXED — this canary fired 2026-08-17 and is now flipped.** It used
        // to assert `[]`, pinning kaish's long-standing divergence: a bare
        // assignment returned success unconditionally, so `||` never saw the
        // substitution's failure and declined to fire. The test carried
        // instructions for exactly this moment and they were followed.
        //
        // The fix arrived when kaijutsu linked against the kaish lead's
        // integration worktree (`integration/kaijutsu-preview`, rev
        // `21642871…`). Verified against the POSIX reference semantics this
        // project probed in bash and recorded — a bare assignment takes the
        // status of the LAST command substitution performed, or 0 if none:
        //
        //   false; x=5              rc=0   not stale; no substitution ran
        //   x="$(false)$(true)"     rc=0   \ last wins — decisively NOT
        //   x="$(true)$(false)"     rc=1   / "any failed"
        //   x=$(false) true         rc=0   has a command NAME, so it's that
        //
        // All four match. The two middle rows are the ones that are easy to
        // get wrong from memory, so they are the ones worth having checked.
        //
        // **If this ever asserts `[]` again, the dependency moved BACKWARD** —
        // most likely someone reverted the `path` dep to a crates.io `"0.14"`
        // before 0.15 was actually released. That is a real regression signal,
        // not a test to relax.
        let fixed = run(
            r#"v="$(cat /definitely/not/here)" || v="READ-FAIL"; echo "[$v]""#,
        )
        .await;
        assert!(
            fixed.contains("[READ-FAIL]"),
            "`||` after a command-substitution assignment must fire — fixed in the \
             linked kaish. Getting `[]` means the kaish dependency regressed to a \
             version predating the fix. Got: {fixed}"
        );

        // Quiet-hours arithmetic on the UNPADDED hour the script now asks
        // for (`date '+%-H'`). Pins the 8/9 would-be-octal boundary.
        let hours = run(
            r#"for h in "3" "8" "14" "22"; do q=0; if [[ "$h" -ge 22 ]] || [[ "$h" -lt 6 ]]; then q=1; fi; echo "$h=$q"; done"#,
        )
        .await;
        for expected in ["3=1", "8=0", "14=0", "22=1"] {
            assert!(
                hours.contains(expected),
                "quiet-hours comparison wrong: expected {expected} in {hours}"
            );
        }

        // The other half, and the reason the script asks for `%-H`: kaish
        // reads no octal and refuses a zero-padded hour as a number rather
        // than guessing. `date '+%H'` into this comparison is a hard error
        // every hour before 10:00. If this stops erroring, kaish changed
        // its numeric parsing — revisit `%-H` in the rc scripts before
        // relaxing anything.
        let padded = kaish
            .execute_with_options(
                r#"h="03"; if [[ "$h" -lt 6 ]]; then echo "quiet"; fi"#,
                ExecuteOptions::default(),
            )
            .await;
        let err = padded.expect_err("a zero-padded hour must not compare numerically");
        let msg = err.to_string();
        assert!(
            msg.contains("leading zero"),
            "expected kaish's leading-zero type error, got: {msg}"
        );
    }

    /// The external-exec policy end to end: `Allow` + a Local-mounted cwd runs
    /// a real host binary through kaish's subprocess path; `Deny` fails fast
    /// with `command not found` (127) — no PATH, no absolute-path escape.
    /// Linux-shaped by design (the runner/CI are): `/usr/bin/id` is the probe.
    #[tokio::test]
    async fn external_exec_policy_gates_host_subprocesses() {
        let principal = kaijutsu_types::PrincipalId::system();
        let blocks = shared_block_store(principal);
        let kernel = test_kernel("test-exec").await;
        // Real host root so the shell's cwd resolves to a real directory —
        // the same shape as production's read-only "/" mount.
        kernel
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;

        let mk = |name: &str, exec: ExternalExec| {
            EmbeddedKaish::with_identity(
                name,
                blocks.clone(),
                kernel.clone(),
                Some(std::env::temp_dir()),
                crate::runtime::context_shell::ShellIdentity { requester: principal, performer: principal, reviewer: None, context: ContextId::new(), session: SessionId::new() },
                crate::runtime::context_engine::session_context_map(),
                exec,
                OutputProfile::Agent,
                |_, _, _| {},
            )
            .unwrap()
        };

        // Allow: absolute path spawns for real.
        let allowed = mk(
            "test-exec-allow",
            ExternalExec::Allow { path: Some("/usr/bin:/bin".to_string()) },
        );
        let r = allowed
            .execute_with_options("/usr/bin/id", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(r.ok(), "Allow + absolute path must spawn: {}", r.err);
        assert!(r.text_out().contains("uid="), "absolute id must execute: {}", r.text_out());

        // Allow + seeded PATH: bare names resolve too.
        let r = allowed
            .execute_with_options("id", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(r.ok(), "Allow + PATH must resolve bare names: {}", r.err);
        assert!(r.text_out().contains("uid="), "bare id must execute: {}", r.text_out());

        // Deny: the same absolute path fails fast as command-not-found.
        let denied = mk("test-exec-deny", ExternalExec::Deny);
        let r = denied
            .execute_with_options("/usr/bin/id", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!r.ok(), "Deny must refuse external exec");
        assert_eq!(r.code, 127, "fail-fast command-not-found: {}", r.err);
    }

    /// Output capping must not forge a failure inside the script.
    ///
    /// kaish remaps a capped command's exit code to 3 so the *embedder* can
    /// tell (`output_limit` module doc). That signal reaches the running
    /// program's `$?` as well, so on the Agent profile a successful command
    /// that merely printed a lot reads as failed — which is why rc scripts,
    /// hook bodies, and editor splices run Internal instead.
    ///
    /// Both halves are asserted deliberately. The Agent half pins kaish's
    /// current behaviour: if a future bump stops forging the code, this test
    /// fails and tells us the local workaround can go away, rather than the
    /// workaround quietly outliving its reason.
    #[tokio::test]
    async fn internal_profile_does_not_forge_a_failure_on_large_output() {
        let principal = kaijutsu_types::PrincipalId::system();
        let blocks = shared_block_store(principal);
        let kernel = test_kernel("test-outlimit").await;

        let mk = |name: &str, profile: OutputProfile| {
            EmbeddedKaish::with_identity(
                name,
                blocks.clone(),
                kernel.clone(),
                Some(std::env::temp_dir()),
                crate::runtime::context_shell::ShellIdentity { requester: principal, performer: principal, reviewer: None, context: ContextId::new(), session: SessionId::new() },
                crate::runtime::context_engine::session_context_map(),
                ExternalExec::Deny,
                profile,
                |_, _, _| {},
            )
            .unwrap()
        };

        // ~23 KB from a kaish builtin — no host exec, so this is the shell's
        // own captured-output path and nothing else.
        const BIG: &str = "seq 1 5000; echo \"status=$?\"";
        // Same command shape, small enough never to trip the cap: the control
        // that proves the profile is what differs and not the command.
        const SMALL: &str = "seq 1 100; echo \"status=$?\"";

        let agent = mk("test-outlimit-agent", OutputProfile::Agent);
        let r = agent
            .execute_with_options(SMALL, ExecuteOptions::default())
            .await
            .unwrap();
        assert!(
            r.text_out().contains("status=0"),
            "control: under the cap, $? must be the real exit — got: {}",
            r.text_out().lines().last().unwrap_or("")
        );

        let r = agent
            .execute_with_options(BIG, ExecuteOptions::default())
            .await
            .unwrap();
        assert!(
            r.text_out().contains("status=3"),
            "Agent profile is expected to forge $?=3 on a capped command \
             (kaish's did_spill remap). If this now reports status=0, kaish \
             changed and OutputProfile::Internal may no longer be needed — \
             check before deleting it. Got: {}",
            r.text_out().lines().last().unwrap_or("")
        );

        let internal = mk("test-outlimit-internal", OutputProfile::Internal);
        let r = internal
            .execute_with_options(BIG, ExecuteOptions::default())
            .await
            .unwrap();
        assert!(
            r.text_out().contains("status=0"),
            "Internal profile must report the command's REAL exit — an rc \
             script doing `cmd || escalate` must not escalate on a command \
             that worked and merely printed a lot. Got: {}",
            r.text_out().lines().last().unwrap_or("")
        );
        // And the output itself must be verbatim, not a head+tail splice —
        // the editor's `:r !cmd` splices this text into a document.
        assert!(
            r.text_out().contains("\n5000\n"),
            "Internal profile must not truncate: the last line of a 5000-line \
             run is missing, so the capture was spliced"
        );
    }

    /// `$HOME` and `~` must give the SAME answer, and it must be non-empty.
    /// kaish reads both from the session scope's `HOME` (never the host env), so
    /// an unseeded shell has `$HOME` empty *and* `~` left literal — `$HOME/path`
    /// then silently resolves wrong while `~/path` also fails. Seeding `HOME` at
    /// materialization makes the two agree by construction. Runs against the
    /// default `EmbeddedKaish::new` (a Deny, non-read-only shell) to prove the
    /// seeding is unconditional, not gated on the exec grant.
    #[tokio::test]
    async fn home_var_and_tilde_agree() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-home").await;
        let kaish = EmbeddedKaish::new("test-home", blocks, kernel, None).unwrap();

        let home = kaish
            .execute_with_options("echo $HOME", ExecuteOptions::default())
            .await
            .unwrap();
        let home = home.text_out().trim().to_string();
        assert!(
            !home.is_empty(),
            "$HOME must be seeded (non-empty) in the materialized shell",
        );

        let tilde = kaish
            .execute_with_options("echo ~", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            tilde.text_out().trim(),
            home,
            "`~` must expand to exactly $HOME — the variable and the tilde must agree",
        );

        // The concrete failure the seeding fixes: `~/sub` must root at the seeded
        // HOME (kaish rejects `$HOME/sub` token-pasting, so the bare tilde word
        // is the surface a user actually types).
        let tilde_sub = kaish
            .execute_with_options("echo ~/sub", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            tilde_sub.text_out().trim(),
            format!("{home}/sub"),
            "`~/sub` must root at the seeded HOME",
        );
    }

    #[tokio::test]
    async fn captured_exports_preserve_names_that_resemble_temporary_variables() {
        use kaish_kernel::ast::Value;
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kaish = EmbeddedKaish::new("env-name-collision", blocks,
            test_kernel("env-name-collision").await, None).unwrap();
        let names = ["__kj_env_0__", "__kj_env_1__", "PLAIN"];
        let rows: Vec<_> = names.iter().enumerate().map(|(i, name)| crate::kernel_db::ContextEnvRow {
            context_id: ContextId::new(), key: (*name).into(), value: format!("value {i} '$HOME\n"),
        }).collect();
        kaish.export_env_vars(&rows).await.unwrap();
        for row in &rows {
            assert_eq!(kaish.get_var(&row.key).await, Some(Value::String(row.value.clone())),
                "durable export {} was lost with the temporary overlay", row.key);
        }
        let restored = [
            approval_ledger::types::AskEnvRow { seq: 0, name: "__kj_ask_env_1__".into(), value: None },
            approval_ledger::types::AskEnvRow { seq: 1, name: "PLAIN".into(), value: Some("approved ' $value\n".into()) },
            approval_ledger::types::AskEnvRow { seq: 2, name: "__kj_ask_env_2__".into(), value: Some("keep this".into()) },
            approval_ledger::types::AskEnvRow { seq: 3, name: "__kj_env_2__".into(), value: None },
            approval_ledger::types::AskEnvRow { seq: 4, name: "__kj_env_3__".into(), value: Some("also keep this".into()) },
        ];
        kaish.set_var("__kj_ask_env_1__", Value::String("later value".into())).await;
        kaish.apply_ask_env(&restored).await.unwrap();
        for row in &restored {
            assert_eq!(kaish.get_var(&row.name).await, row.value.clone().map(Value::String),
                "approved export {} collided with a restore temporary", row.name);
        }
        let exports = kaish.exported_vars().await;
        for row in rows.iter().filter(|row| row.key != "PLAIN") {
            assert!(exports.contains(&(row.key.clone(), row.value.clone())));
        }
        assert!(exports.contains(&("PLAIN".into(), "approved ' $value\n".into())));
        assert!(!exports.iter().any(|(name, _)| name == "__kj_ask_env_1__"));
        let invalid = [
            approval_ledger::types::AskEnvRow { seq: 0, name: "PLAIN".into(), value: Some("must not apply".into()) },
            approval_ledger::types::AskEnvRow { seq: 1, name: "bad;name".into(), value: None },
        ];
        assert!(kaish.apply_ask_env(&invalid).await.unwrap_err().to_string().contains("not a valid identifier"));
        assert_eq!(kaish.get_var("PLAIN").await, Some(Value::String("approved ' $value\n".into())),
            "validate the whole capture before applying any entry");
        let duplicate = [
            approval_ledger::types::AskEnvRow { seq: 0, name: "PLAIN".into(), value: Some("first".into()) },
            approval_ledger::types::AskEnvRow { seq: 1, name: "PLAIN".into(), value: Some("second".into()) },
        ];
        assert!(kaish.apply_ask_env(&duplicate).await.unwrap_err().to_string().contains("duplicate"));
        assert_eq!(kaish.get_var("PLAIN").await, Some(Value::String("approved ' $value\n".into())),
            "ambiguous capture must not change the environment");
    }

    #[tokio::test]
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-vars").await;
        let kaish = EmbeddedKaish::new("test-vars", blocks, kernel, None).unwrap();

        // Set and get a variable
        kaish
            .set_var("X", kaish_kernel::ast::Value::String("hello".into()))
            .await;
        let val = kaish.get_var("X").await;
        assert!(val.is_some());

        match val.unwrap() {
            kaish_kernel::ast::Value::String(s) => assert_eq!(s, "hello"),
            _ => panic!("Expected String value"),
        }
    }

    #[tokio::test]
    async fn test_named_config_cwd_is_home() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-cwd-home").await;
        let kaish = EmbeddedKaish::new("test-cwd-home", blocks, kernel, None).unwrap();

        let cwd = kaish.cwd().await;
        // KaishConfig::named() sets cwd to home_dir(). We can't control HOME
        // in parallel tests, so just verify it's a real existing directory.
        assert!(
            cwd.is_dir(),
            "cwd should be an existing directory, got {:?}",
            cwd
        );
        assert!(cwd.is_absolute(), "cwd should be absolute, got {:?}", cwd);
    }

    #[tokio::test]
    async fn test_mcp_config_cwd_is_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = test_kernel("test-cwd-project").await;
        let kaish = EmbeddedKaish::new(
            "test-cwd-project",
            blocks,
            kernel,
            Some(tmp.path().to_path_buf()),
        )
        .unwrap();

        let cwd = kaish.cwd().await;
        // Canonicalize both to handle symlinks (e.g., /tmp → /private/tmp on macOS)
        let expected = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        let actual = cwd.canonicalize().unwrap_or(cwd.clone());
        assert_eq!(actual, expected, "cwd should be project root");
    }

}

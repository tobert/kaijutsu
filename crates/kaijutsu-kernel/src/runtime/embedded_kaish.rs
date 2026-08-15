//! Embedded kaish executor using MountBackend + VFS adapters.
//!
//! Instead of spawning kaish as a subprocess, this module embeds the kaish
//! interpreter directly, routing I/O through the kaijutsu kernel's MountTable
//! for real filesystem access and VFS adapters for CRDT blocks.
//!
//! # Architecture
//!
//! ```text
//! kaijutsu-server
//!     │
//!     └── EmbeddedKaish
//!             │
//!             ├── kaish::Kernel (in-process)
//!             │       │
//!             │       ├── /v/docs → KaijutsuFilesystem (CRDT blocks)
//!             │       ├── /v/jobs, /v/cas → kaish builtins
//!             │       └── everything else → MountBackend
//!             │               │
//!             │               ├── File ops → MountTable → LocalBackend
//!             │               └── Tool calls → KaijutsuBackend
//!             │
//!             └── Shared state with kaijutsu kernel
//! ```
//!
//! This enables kaish scripts to access both CRDT blocks and real files,
//! with tool dispatch routed through the kernel's tool registry.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::output_limit::OutputLimitConfig;
use kaish_kernel::{
    ExecuteOptions, IgnoreConfig, Kernel as KaishKernel, KernelBackend, KernelConfig as KaishConfig,
};

use crate::Kernel as KaijutsuKernel;
use crate::block_store::SharedBlockStore;
use crate::kernel_db::KernelDb;
use kaijutsu_types::paths::{DOCS_ROOT, INPUT_ROOT};
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use super::docs_filesystem::KaijutsuFilesystem;
use super::input_filesystem::InputFilesystem;
use super::kaish_backend::KaijutsuBackend;
use super::mount_backend::MountBackend;
use super::read_only_fs::ReadOnlyFs;
use super::context_engine::{SessionContextExt, SessionContextMap};

/// Embedded kaish executor backed by CRDT blocks.
///
/// Embeds the kaish interpreter directly and routes all I/O through
/// `KaijutsuBackend`.
pub struct EmbeddedKaish {
    /// The embedded kaish kernel.
    kernel: KaishKernel,
    /// Kernel name/id.
    name: String,
    /// Global session map for context tracking.
    session_contexts: SessionContextMap,
    session_id: SessionId,
    /// Snapshot of the kaijutsu kernel's `TimeoutPolicy` at construction.
    /// Read by `apply_context_config` for the init-script bound; read by
    /// the wrapper accessor `timeouts()` for callers that build their own
    /// `ExecuteOptions` (e.g. `KjDispatcher::run_kai_script`).
    timeouts: kaijutsu_types::TimeoutPolicy,
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

/// Who consumes this shell's output — which decides how hard we cap it.
///
/// kaish caps output by *replacing* the captured text with a head+tail preview
/// and **remapping the exit code to 3** (`did_spill`), keeping the real code in
/// `original_code` (`kaish-kernel`'s `output_limit` module doc). The remap is a
/// deliberate signal to the embedder, but it reaches the running script's `$?`
/// too — so inside a kaish program a command that *succeeded* and merely
/// printed a lot looks like it failed, and `set -e` / `cmd || fallback` /
/// `if cmd; then` all take the error branch. Measured live 2026-08-15; see
/// docs/issues.md, "kaish output limiting — REMEASURED".
///
/// `kj`/MCP callers never see this, because `mcp/servers/shell.rs` unwraps
/// `original_code` at the tool boundary. Scripts do. So the profile is chosen
/// by who reads the output, not by how much we trust the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputProfile {
    /// **Model-facing.** kaish's sandboxed-agent preset: an 8 KB cap with a
    /// head+tail preview. Bounded output is the point here — it protects the
    /// context window, and a model that hits the cap can re-run something
    /// narrower. The `$?` remap is a live wart on this path (a model writing
    /// `cmd || echo failed` gets bitten), and the real fix for it is upstream:
    /// a kaish knob that signals spill without forging the exit code. Filed.
    #[default]
    Agent,
    /// **Kernel-internal.** rc scripts, hook bodies, and editor `:r !cmd`
    /// splices: output is consumed by kaijutsu itself, not shipped to a model,
    /// so capping it buys nothing and costs correctness twice over — a forged
    /// `$?`, and (where the text is spliced into a document) a silent
    /// head+tail replacement of content that was supposed to be verbatim.
    ///
    /// Still an explicit cap rather than `none()`: a runaway rc script should
    /// hit a wall rather than eat the kernel's memory. It is set far above any
    /// legitimate rc output, so `did_spill` should never fire in practice —
    /// and if it does, that is a real problem worth the loud wrong answer.
    Internal,
}

/// The in-memory ceiling for [`OutputProfile::Internal`]. Two orders of
/// magnitude above kaish's 8 KB agent preset and far above any rc/hook body we
/// write, so it is a runaway backstop and not a working limit.
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
    /// Uses `PrincipalId::system()` and a fresh `ContextId`. For real connections,
    /// prefer `with_identity` which accepts the actual connection identity.
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
            PrincipalId::system(),
            ContextId::new(),
            SessionId::new(),
            crate::runtime::context_engine::session_context_map(),
            ExternalExec::Deny,
            OutputProfile::Agent,
            |_, _, _| {},
        )
    }

    /// Create an embedded kaish executor with explicit identity fields.
    ///
    /// Identity flows through to `ToolContext` for drift/whoami engines.
    /// The `context_id` is tracked via the shared `SessionContextMap`.
    ///
    /// The `configure_tools` callback receives the map and session ID so callers
    /// can register tools (like KjBuiltin) that need context awareness.
    #[allow(clippy::too_many_arguments)]
    pub fn with_identity(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
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
            principal_id,
            context_id,
            session_id,
            session_contexts,
            false,
            external_exec,
            output,
            configure_tools,
        )
    }

    /// Like [`Self::with_identity`] but the materialized shell is **read-only**:
    /// every filesystem mutation and every external command is refused by
    /// construction, while reads — real files *and* the CRDT document views at
    /// `/v/docs` / `/v/input` — still work. Backs the toolie's
    /// `read_only_shell` (see `mcp/servers/shell.rs`).
    #[allow(clippy::too_many_arguments)]
    pub fn with_identity_read_only(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        session_contexts: SessionContextMap,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        Self::with_identity_mode(
            name,
            blocks,
            kernel,
            project_root,
            principal_id,
            context_id,
            session_id,
            session_contexts,
            true,
            // Read-only never spawns: external exec is the sandbox's fourth
            // lever, held Deny by construction (no caller choice to get wrong).
            ExternalExec::Deny,
            // Read-only backs the toolie's `read_only_shell` — a model-facing
            // surface, so it takes the model-facing cap. Same reasoning as
            // exec: not a caller choice, because there is only one caller and
            // one right answer.
            OutputProfile::Agent,
            configure_tools,
        )
    }

    /// Shared builder for [`Self::with_identity`] /
    /// [`Self::with_identity_read_only`]. When `read_only` is set, the
    /// `MountBackend` refuses every mutation, the `/v/*` CRDT mounts are wrapped
    /// read-only, and external command execution is disabled — three structural
    /// levers, mirroring kaibo's read-only sandbox recipe (`sandbox.rs`) adapted
    /// to kaijutsu's *shared*, CRDT-backed mount table.
    #[allow(clippy::too_many_arguments)]
    fn with_identity_mode(
        name: &str,
        blocks: SharedBlockStore,
        kernel: Arc<KaijutsuKernel>,
        project_root: Option<PathBuf>,
        principal_id: PrincipalId,
        context_id: ContextId,
        session_id: SessionId,
        session_contexts: SessionContextMap,
        read_only: bool,
        external_exec: ExternalExec,
        output: OutputProfile,
        configure_tools: impl FnOnce(SessionContextMap, SessionId, &mut kaish_kernel::ToolRegistry),
    ) -> Result<Self> {
        // Initialize session map entry if missing
        session_contexts.entry(session_id).or_insert(context_id);

        let input_fs = Arc::new(InputFilesystem::new(
            blocks.clone(),
            session_contexts.clone(),
            session_id,
        ));
        // The shared CRDT file cache: same instance the MCP file tools use
        // (installed at server startup), or lazily built from this block store
        // in embedded/test paths. Routing MountBackend through it is the whole
        // point of kaish — shell scripting on the same CRDT substrate.
        let file_cache = kernel.file_cache(&blocks);
        let docs_backend = Arc::new(KaijutsuBackend::new(
            blocks,
            kernel.clone(),
            principal_id,
            session_contexts.clone(),
            session_id,
                    ));
        let mount_table = kernel.vfs().clone();

        // Read-only mode refuses every mutation at the MountBackend boundary
        // (real files + the CRDT FileDocumentCache), regardless of whether the
        // shared mount is writable. The `/v/*` CRDT mounts bypass MountBackend,
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

        // KaishConfig primarily sets the cwd and kernel name. The VFS mode
        // in the config is secondary to kaijutsu's MountTable — real filesystem
        // access is routed through MountBackend → MountTable → LocalBackend,
        // not through kaish's own VFS modes.
        //
        // `project_root` sets the cwd to a specific project directory (used by
        // MCP sessions that operate on a particular repo). When None, cwd
        // defaults to $HOME via `KaishConfig::named()`. The context's persisted
        // cwd (`context_shell.cwd`) is *not* restored here: it must be validated
        // against the shell's backend (the VFS namespace `cd` uses), which is
        // async, so `restore_cwd_from_db` does it post-construction — see
        // `materialize_context_kaish`.
        // kaijutsu overrides the backend with MountBackend (see below), so the
        // config's vfs_mode is moot — what matters is the cwd and the agent-grade
        // ignore/output-limit presets (gitignore-aware walks + capped output).
        // Build them explicitly via the builder chain rather than a bundled
        // constructor so this survives kaish config-API churn. (kaish 0.9 renamed
        // these `mcp()` presets to `agent()` — same sandboxed-agent behavior.)
        let mut config = KaishConfig::named(name)
            .with_ignore_config(IgnoreConfig::agent())
            // Output cap by consumer, not by trust — see `OutputProfile`.
            // Model-facing shells keep kaish's 8 KB agent preset; rc/hook/
            // editor-splice shells get a runaway backstop instead, so kaish's
            // `did_spill` exit-code remap never forges a failure in a script.
            .with_output_limit(output.to_config())
            // Latch nonces must outlive this per-execute shell: a nonce issued
            // by one command (e.g. `kj context retag`) is confirmed by the
            // *next* command in a fresh `EmbeddedKaish`. The store is keyed by
            // context on the long-lived kernel, so the `--confirm` lands in the
            // same table that issued the nonce instead of a fresh empty one.
            .with_nonce_store(kernel.nonce_store_for(context_id));
        if let Some(root) = project_root {
            config = config.with_cwd(root);
        }

        // Apply kernel-wide kaish-script default timeout. Per-call sites
        // (rc lifecycle, hook bodies, init scripts) can override via
        // `ExecuteOptions::with_timeout` for stricter per-context bounds.
        config.request_timeout = Some(kernel.timeouts().kaish_request_timeout);

        // Seed `HOME` for EVERY shell flavor (read-only included), not just
        // exec-granted ones. kaish is hermetic: it never reads the host
        // `std::env::var("HOME")`, so with empty `initial_vars` the scope has no
        // `HOME` — `$HOME` expands to empty AND `~` is left literal (both read
        // the same scope var via kaish's `scope_home()`). Seeding it here makes
        // the two agree by construction (`echo $HOME` == the `~` target) and
        // exports it to child processes. `KaishConfig::named()` already lands the
        // shell's default cwd on this same directory, so `~` == cwd in the common
        // case. A durable `context_env` HOME still wins: `apply_context_config`
        // exports it after construction.
        let home = kaish_kernel::home_dir().to_string_lossy().into_owned();
        config
            .initial_vars
            .insert("HOME".to_string(), kaish_kernel::ast::Value::String(home));

        // Host subprocess policy. With kaish's `subprocess` feature compiled
        // in, `allow_external_commands` would default to true — so every shell
        // states its policy explicitly, decided by the caller from the
        // context's loadout (`exec` authority). Deny keeps the old behavior:
        // unknown commands fail fast, builtins/kj unaffected. For read-only
        // shells Deny is structural (the constructor pins it), the sandbox's
        // fourth lever alongside the read-only MountBackend + `/v/*` wraps.
        match &external_exec {
            ExternalExec::Deny => {
                config = config.with_allow_external_commands(false);
            }
            ExternalExec::Allow { path } => {
                config = config.with_allow_external_commands(true);
                if let Some(p) = path {
                    config
                        .initial_vars
                        .insert("PATH".to_string(), kaish_kernel::ast::Value::String(p.clone()));
                }
            }
        }

        // The CRDT document views (`/v/docs`, `/v/input`) are mounted directly
        // on the kaish VFS, bypassing MountBackend — so in read-only mode they
        // get their own structural gate via `ReadOnlyFs` (reads delegate,
        // writes refuse). Otherwise they mount writable.
        let docs_mount: Arc<dyn kaish_kernel::vfs::Filesystem> = if read_only {
            Arc::new(ReadOnlyFs::new(docs_fs))
        } else {
            docs_fs
        };
        let input_mount: Arc<dyn kaish_kernel::vfs::Filesystem> = if read_only {
            Arc::new(ReadOnlyFs::new(input_fs))
        } else {
            input_fs
        };

        let ctx_for_tools = session_contexts.clone();
        let sid_for_tools = session_id;
        let timeouts = kernel.timeouts().clone();
        let kaish_kernel = KaishKernel::with_backend(
            mount_backend,
            config,
            |vfs| {
                vfs.mount_arc(DOCS_ROOT, docs_mount);
                vfs.mount_arc(INPUT_ROOT, input_mount);
            },
            |tools| {
                configure_tools(ctx_for_tools, sid_for_tools, tools);
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
    pub async fn execute_with_options(
        &self,
        code: &str,
        opts: ExecuteOptions,
    ) -> Result<ExecResult> {
        let (traceparent, tracestate) = kaijutsu_telemetry::inject_trace_context();
        let context_id = self.context_id().map(|cid| cid.to_string());
        let opts = merge_trace_context(opts, traceparent, tracestate, context_id);
        self.kernel.execute_with_options(code, opts).await
    }

    /// Get a variable value.
    pub async fn get_var(&self, name: &str) -> Option<kaish_kernel::ast::Value> {
        self.kernel.get_var(name).await
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

    /// Restore this context's persisted cwd (`context_shell.cwd`) into the
    /// shell, validating it against the shell's backend — the same namespace
    /// `cd` resolves against, *not* the host filesystem. Returns:
    ///   - `Ok(None)` — nothing persisted; the shell keeps its default cwd.
    ///   - `Ok(Some(path))` — the persisted cwd was restored.
    ///   - `Err(path)` — a cwd was persisted but no longer resolves to a
    ///     directory; the shell keeps its default. Callers should surface this
    ///     rather than swallow it (it would otherwise be a silent fallback).
    pub async fn restore_cwd_from_db(
        &self,
        kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
        context_id: ContextId,
    ) -> Result<Option<std::path::PathBuf>, std::path::PathBuf> {
        let persisted = {
            let db = kernel_db.lock();
            db.get_context_shell(context_id)
                .ok()
                .flatten()
                .and_then(|row| row.cwd)
        };
        let Some(cwd) = persisted else {
            return Ok(None);
        };
        let path = std::path::PathBuf::from(cwd);
        if self.try_set_cwd(path.clone()).await {
            Ok(Some(path))
        } else {
            Err(path)
        }
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

    /// Seed the shell with the context's durable env vars (`context_env`).
    ///
    /// The context shell is shared state that evolves over the context's
    /// lifetime; its durable identity is `env + cwd` in the DB. cwd is restored
    /// post-construction by `restore_cwd_from_db`; this applies the env half.
    /// Context-setup *scripting* is RC's job now (the former
    /// `context_shell.init_script` was a leftover and has been folded into the
    /// rc lifecycle), so this no longer runs any script.
    pub async fn apply_context_config(
        &self,
        db: &parking_lot::Mutex<KernelDb>,
        context_id: ContextId,
    ) {
        let env_vars = {
            let db_guard = db.lock();
            db_guard.get_context_env(context_id).unwrap_or_default()
        };

        // Export env vars so they propagate to child processes. Uses the
        // kernel-wide kaish default timeout — exports are tiny, no override.
        for var in &env_vars {
            // Shell-escape value to avoid injection.
            let escaped = var.value.replace('\'', "'\\''");
            if let Err(e) = self
                .execute_with_options(
                    &format!("export {}='{}'", var.key, escaped),
                    ExecuteOptions::default(),
                )
                .await
            {
                tracing::warn!(
                    key = %var.key,
                    error = %e,
                    "failed to apply context env var",
                );
            }
        }
    }
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-agent").await);

        let kaish = EmbeddedKaish::new("test-kernel", blocks, kernel, None);
        assert!(kaish.is_ok());

        let kaish = kaish.unwrap();
        assert_eq!(kaish.name(), "test-kernel");
    }

    #[tokio::test]
    async fn test_execute_with_options_feeds_stdin() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-stdin").await);
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
    /// `ln -s` over the `/etc/rc` CRDT mount, and `cat` through the link returns
    /// the *target's* content. This proves the path is wired end-to-end —
    /// kaish `ln`/`cat` builtins → MountBackend → MountTable → ConfigCrdtFs —
    /// with no rc-specific shell code. (KaijutsuBackend's `/docs/` block scheme
    /// is a separate thing and keeps its honest "not supported" stub.)
    #[tokio::test]
    async fn ln_s_over_rc_mount_creates_followable_link() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-ln").await);
        // Mount the production rc backend over the same block store the shell uses.
        let rc_fs =
            crate::runtime::config_crdt_fs::ConfigCrdtFs::new(blocks.clone(), RC_ROOT);
        kernel.mount(RC_ROOT, rc_fs).await;
        let kaish = EmbeddedKaish::new("test-ln", blocks, kernel, None).unwrap();

        let run = |cmd: &'static str| {
            let k = &kaish;
            async move {
                k.execute_with_options(cmd, ExecuteOptions::default())
                    .await
                    .unwrap_or_else(|e| panic!("`{cmd}` failed: {e}"))
            }
        };

        // A shared script body, written once under a `lib` type.
        let r = run("echo shared-body > /etc/rc/lib/create/binding.kai").await;
        assert!(r.ok(), "echo>: {}", r.text_out());
        // Compose it into a context type by symlink.
        let r = run(
            "ln -s /etc/rc/lib/create/binding.kai /etc/rc/coder/create/S10-binding.kai",
        )
        .await;
        assert!(r.ok(), "ln -s: {}", r.text_out());
        // `cat` through the link follows to the target's content.
        let r = run("cat /etc/rc/coder/create/S10-binding.kai").await;
        assert_eq!(r.text_out().trim(), "shared-body");
        // `readlink` reports the raw target.
        let r = run("readlink /etc/rc/coder/create/S10-binding.kai").await;
        assert_eq!(r.text_out().trim(), "/etc/rc/lib/create/binding.kai");
    }

    /// `/v/cas` regression (docs/issues.md "kaish `/v/cas` is shadowed"). kaish
    /// 0.11's `VirtualOverlayBackend` reserved every `/v/*` path for its own
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-cas").await);

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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-idioms").await);
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

        // The BROKEN idiom, pinned as currently-divergent. See the doc comment:
        // when kaish learns POSIX assignment status, this flips and should be
        // updated deliberately.
        let dead = run(
            r#"v="$(cat /definitely/not/here)" || v="READ-FAIL"; echo "[$v]""#,
        )
        .await;
        assert!(
            dead.contains("[]"),
            "kaish currently does NOT fire `||` after a command-substitution \
             assignment. If this now reports READ-FAIL the bug was fixed — good; \
             update this test and the trap comments in \
             assets/defaults/rc/assistant/tick/S10-checkin.kai. Got: {dead}"
        );

        // Quiet-hours arithmetic: `date '+%H'` is zero-padded, and bare numeric
        // tokens lose their leading zero in `case`, so the scripts use numeric
        // comparison. Pins the 08/09 would-be-octal boundary too.
        let hours = run(
            r#"for h in "03" "08" "14" "22"; do q=0; if [[ "$h" -ge 22 ]] || [[ "$h" -lt 6 ]]; then q=1; fi; echo "$h=$q"; done"#,
        )
        .await;
        for expected in ["03=1", "08=0", "14=0", "22=1"] {
            assert!(
                hours.contains(expected),
                "quiet-hours comparison wrong: expected {expected} in {hours}"
            );
        }
    }

    /// The external-exec policy end to end: `Allow` + a Local-mounted cwd runs
    /// a real host binary through kaish's subprocess path; `Deny` fails fast
    /// with `command not found` (127) — no PATH, no absolute-path escape.
    /// Linux-shaped by design (the runner/CI are): `/bin/sh` is the probe.
    #[tokio::test]
    async fn external_exec_policy_gates_host_subprocesses() {
        let principal = kaijutsu_types::PrincipalId::system();
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-exec").await);
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
                principal,
                ContextId::new(),
                SessionId::new(),
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
            .execute_with_options("/bin/sh -c true", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(r.ok(), "Allow + absolute path must spawn: {}", r.err);

        // Allow + seeded PATH: bare names resolve too.
        let r = allowed
            .execute_with_options("sh -c true", ExecuteOptions::default())
            .await
            .unwrap();
        assert!(r.ok(), "Allow + PATH must resolve bare names: {}", r.err);

        // Deny: the same absolute path fails fast as command-not-found.
        let denied = mk("test-exec-deny", ExternalExec::Deny);
        let r = denied
            .execute_with_options("/bin/sh -c true", ExecuteOptions::default())
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-outlimit").await);

        let mk = |name: &str, profile: OutputProfile| {
            EmbeddedKaish::with_identity(
                name,
                blocks.clone(),
                kernel.clone(),
                Some(std::env::temp_dir()),
                principal,
                ContextId::new(),
                SessionId::new(),
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-home").await);
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
    async fn test_embedded_kaish_variables() {
        let blocks = shared_block_store(kaijutsu_types::PrincipalId::system());
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-vars").await);
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-cwd-home").await);
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
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-cwd-project").await);
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

    /// Context env vars stored in KernelDb should be available after
    /// apply_context_config is called on a freshly-created EmbeddedKaish.
    #[tokio::test]
    async fn test_context_env_applied_on_creation() {
        use crate::kernel_db::{ContextRow, KernelDb};
        use kaijutsu_types::{ConsentMode, ContextState, now_millis};

        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();

        let ws_id = db
            .get_or_create_default_workspace(principal)
            .unwrap();

        db.insert_context_with_document(
            &ContextRow {
                context_id,
                                label: Some("test-env".into()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
                context_type: "default".to_string(),
                created_at: now_millis() as i64,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                cast_id: None,
                origin_host: None,
            },
            ws_id,
        )
        .unwrap();

        // Store env vars in DB.
        db.set_context_env(context_id, "KJ_TEST_FOO", "bar_value")
            .unwrap();
        db.set_context_env(context_id, "KJ_TEST_NUM", "42")
            .unwrap();

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-env").await);

        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity(
            "test-env",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            session_contexts,
            ExternalExec::Deny,
            OutputProfile::Agent,
            |_, _, _| {},
        )
        .unwrap();

        // Apply context config (durable env vars).
        kaish.apply_context_config(&kernel_db, context_id).await;

        // Verify env vars are accessible via kaish execution.
        let result = kaish
            .execute_with_options("echo $KJ_TEST_FOO", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "bar_value",
            "KJ_TEST_FOO should be set from context_env",
        );

        let result = kaish
            .execute_with_options("echo $KJ_TEST_NUM", ExecuteOptions::default())
            .await
            .unwrap();
        assert_eq!(
            result.text_out().trim(),
            "42",
            "KJ_TEST_NUM should be set from context_env",
        );
    }

    /// Regression test: a persisted cwd that is a directory in the shell's
    /// *backend* (a VFS mount) but does NOT exist on the host filesystem must
    /// still restore. The old restore gated on host-FS `PathBuf::is_dir()` and
    /// would silently drop it; `restore_cwd_from_db` validates against the same
    /// backend `cd` resolves against.
    #[tokio::test]
    async fn test_persisted_vfs_cwd_restored_against_backend() {
        use crate::kernel_db::{ContextRow, ContextShellRow, KernelDb};
        use crate::vfs::{MemoryBackend, VfsOps};
        use kaijutsu_types::{ConsentMode, ContextState, now_millis};
        use std::path::Path;

        // A VFS-only path: lives in the MemoryBackend mount below, never on disk.
        let vfs_cwd = "/scratch/work";
        assert!(
            !Path::new(vfs_cwd).is_dir(),
            "precondition: cwd must not exist on the host filesystem"
        );

        let context_id = ContextId::new();
        let principal = PrincipalId::system();
        let db = KernelDb::in_memory().unwrap();
        let ws_id = db.get_or_create_default_workspace(principal).unwrap();
        db.insert_context_with_document(
            &ContextRow {
                context_id,
                label: Some("test-restore-vfs".into()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: ConsentMode::default(),
                context_state: ContextState::Live,
                forked_from: None,
                fork_kind: None,
                created_by: principal,
                context_type: "default".to_string(),
                created_at: now_millis() as i64,
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
                last_activity_at: None,
                promoted_at: None,
                demoted_at: None,
                paused_at: None,
                cast_id: None,
                origin_host: None,
            },
            ws_id,
        )
        .unwrap();
        db.upsert_context_shell(&ContextShellRow {
            context_id,
            cwd: Some(vfs_cwd.to_string()),
            updated_at: now_millis() as i64,
        })
        .unwrap();

        let kernel_db = Arc::new(parking_lot::Mutex::new(db));
        let blocks = shared_block_store(principal);
        let kernel = Arc::new(KaijutsuKernel::new_ephemeral("test-restore-vfs").await);

        // Mount an in-memory FS and create the dir there — pure VFS, no host path.
        kernel.mount("/scratch", MemoryBackend::new()).await;
        kernel
            .vfs()
            .mkdir(Path::new(vfs_cwd), 0o755)
            .await
            .expect("mkdir in VFS mount");

        let sid = SessionId::new();
        let session_contexts = crate::runtime::context_engine::session_context_map();
        let kaish = EmbeddedKaish::with_identity(
            "test-restore-vfs",
            blocks,
            kernel,
            None,
            principal,
            context_id,
            sid,
            session_contexts,
            ExternalExec::Deny,
            OutputProfile::Agent,
            |_, _, _| {},
        )
        .unwrap();

        let restored = kaish
            .restore_cwd_from_db(&kernel_db, context_id)
            .await
            .expect("VFS cwd should restore via backend, not be rejected by a host-FS check");
        assert_eq!(restored.as_deref(), Some(Path::new(vfs_cwd)));
        assert_eq!(
            kaish.cwd().await,
            std::path::PathBuf::from(vfs_cwd),
            "shell cwd should be the restored VFS path"
        );
    }
}

//! `kj` command dispatcher — unified command interface for kernel operations.
//!
//! Three modalities, one implementation:
//! - kaish builtin (`kj context list --tree`)
//! - MCP tool (`shell("kj context list --tree")`)
//! - Future: standalone CLI binary
//!
//! All commands go through `KjDispatcher`, which holds Arc refs to shared
//! kernel state and is constructed once per server.

pub mod attach;
pub mod binding;
pub mod block;
pub mod cache;
pub mod cas;
pub mod compact;
pub mod config;
pub mod context;
pub mod context_shell;
pub mod doc;
pub mod drift;
pub mod drive;
pub mod fork;
pub mod format;
pub mod kv;
pub mod parse;
pub mod policy;
pub mod preset;
pub mod editor;
pub mod rc;
pub mod lifecycle;
pub mod model;
pub mod refs;
pub mod search;
pub mod stage;
pub mod transport;
pub mod workspace;

use std::sync::Arc;

use kaijutsu_types::{ContentType, ContextId, KernelId, PrincipalId, SessionId};

use crate::block_store::SharedBlockStore;
use crate::drift::{DISTILLATION_SYSTEM_PROMPT, SharedDriftRouter, build_distillation_prompt};
use crate::kernel::Kernel;
use crate::kernel_db::KernelDb;

// ============================================================================
// KjCaller — per-invocation identity
// ============================================================================

/// Per-invocation caller identity.
///
/// Constructed from an `ExecContext` at call time — NOT stored on KjDispatcher.
/// The `.` context reference resolves to `context_id`.
#[derive(Debug, Clone)]
pub struct KjCaller {
    pub principal_id: PrincipalId,
    pub context_id: Option<ContextId>,
    pub session_id: SessionId,
    /// True when the caller has verified a latch nonce (destructive op confirmed).
    pub confirmed: bool,
    /// Recursion depth from rc lifecycle dispatch. The rc runner increments
    /// this before invoking nested kj from a script, so an rc-driven
    /// `kj context create` runs at depth 1, etc. Capped at MAX_RC_DEPTH to
    /// prevent runaway recursion (see `kj/lifecycle.rs`).
    pub rc_depth: u8,
    /// True when this caller originates from the rc lifecycle's privileged
    /// kaish (the trusted control plane that assigns loadouts). Stamped at
    /// `KjBuiltin` construction by the rc runner — **never** derived from a
    /// shell var like `KJ_RC_DEPTH` (those are agent-settable, forgeable).
    /// Gates binding *writes*: only a privileged or `binding_admin` caller may
    /// widen a loadout; everyone else may only narrow their own.
    pub privileged: bool,
}

impl KjCaller {
    /// Return the active context or a friendly `KjResult::Err` for subcommands that
    /// cannot operate without one. Use with `?` inside any dispatch leaf that reads
    /// `context_id` — the dispatcher's early-return in `dispatch()` normally catches
    /// this case for non-context subcommands, but per-leaf guards document the
    /// invariant and keep the type-system honest.
    pub(crate) fn require_context(&self) -> Result<ContextId, KjResult> {
        self.context_id.ok_or_else(|| {
            KjResult::Err(
                "no active context joined. Use 'kj context switch <label>' to join one."
                    .to_string(),
            )
        })
    }
}

// ============================================================================
// KjResult — command output
// ============================================================================

/// Result from any kj subcommand.
#[derive(Debug, Clone)]
pub enum KjResult {
    /// Success — exit 0, stdout content.
    Ok {
        message: String,
        content_type: ContentType,
        /// When true, the output is for humans only — excluded from LLM context.
        ephemeral: bool,
        /// Optional structured data routed into `ExecResult.data` so that
        /// `for x in $(kj …)` iterates the value and `kaish-last` can read
        /// it. Conventions:
        /// - List commands: JSON array of identifier strings (block ids,
        ///   context labels) so naive iteration prints handles.
        /// - Inspect/info commands: JSON object with the full record.
        /// Independent of the rendered text — the same call may emit a
        /// human table and an iteration-friendly array.
        data: Option<serde_json::Value>,
    },
    /// Error — exit 1, stderr content.
    Err(String),
    /// Context switch — carries the resolved ContextId for the caller to act on.
    /// The dispatcher resolves the target; the caller (KjBuiltin) updates SharedContextId.
    Switch(ContextId, String),
    /// Destructive op needs confirmation. KjBuiltin converts to ExecResult code 2
    /// via kaish's latch/nonce system.
    Latch {
        /// Nonce scope: the kj subcommand path (e.g., "kj context archive").
        command: String,
        /// Nonce scope: the target label/identifier.
        target: String,
        /// Human-readable summary of what will be affected.
        message: String,
    },
}

impl KjResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, KjResult::Ok { .. } | KjResult::Switch(_, _))
    }

    pub fn is_latch(&self) -> bool {
        matches!(self, KjResult::Latch { .. })
    }

    pub fn message(&self) -> &str {
        match self {
            KjResult::Ok { message, .. }
            | KjResult::Err(message)
            | KjResult::Switch(_, message) => message,
            KjResult::Latch { message, .. } => message,
        }
    }

    /// Convenience: create a plain text Ok result.
    pub fn ok(msg: impl Into<String>) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: None,
        }
    }

    /// Convenience: create an Ok result with a content type hint.
    pub fn ok_typed(msg: impl Into<String>, ct: ContentType) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: false,
            data: None,
        }
    }

    /// Convenience: create an ephemeral Ok result (excluded from LLM hydration).
    pub fn ok_ephemeral(msg: impl Into<String>, ct: ContentType) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: true,
            data: None,
        }
    }

    /// Plain text result with structured data attached. Use a JSON *array*
    /// (e.g. of identifier strings) when callers should iterate the result
    /// in a `for x in $(kj …)` loop — kaish's command-substitution path
    /// spreads array elements per iteration. Use a JSON object for
    /// inspect-style single-record results.
    pub fn ok_with_data(msg: impl Into<String>, data: serde_json::Value) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ContentType::Plain,
            ephemeral: false,
            data: Some(data),
        }
    }

    /// Typed-content variant of [`ok_with_data`] for commands that render
    /// markdown/JSON text alongside their structured payload.
    pub fn ok_typed_with_data(
        msg: impl Into<String>,
        ct: ContentType,
        data: serde_json::Value,
    ) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: false,
            data: Some(data),
        }
    }

    /// Ephemeral result with structured data attached. Used for surfaces
    /// (e.g. `kj cas ls`) where the human text is for the kj prompt only
    /// — excluded from LLM context — but the data array remains useful
    /// for kaish iteration.
    pub fn ok_ephemeral_with_data(
        msg: impl Into<String>,
        ct: ContentType,
        data: serde_json::Value,
    ) -> Self {
        KjResult::Ok {
            message: msg.into(),
            content_type: ct,
            ephemeral: true,
            data: Some(data),
        }
    }
}

// ============================================================================
// KjDispatcher — core dispatcher
// ============================================================================

/// Core dispatcher for kj commands.
///
/// Holds Arc refs to shared kernel state. Constructed once per server,
/// shared across all connections.
pub struct KjDispatcher {
    drift: SharedDriftRouter,
    blocks: SharedBlockStore,
    kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    kernel_id: KernelId,
    kernel: Arc<Kernel>,
    /// Self-Weak so internal paths (rc lifecycle, hook kaish) can hand
    /// an `Arc<KjDispatcher>` to `KjBuiltin::new` without forcing every
    /// call site to thread an Arc through. Set via `set_self_arc` after
    /// `Arc::new(KjDispatcher::new(...))`.
    weak_self: parking_lot::RwLock<Option<std::sync::Weak<KjDispatcher>>>,
    /// The kernel's semantic index, installed by the server at bootstrap via
    /// [`Self::set_semantic_index`] (the index needs an ONNX embedder built in
    /// the server crate, so the kernel can't construct it itself). `None` when
    /// embeddings aren't configured. In-kernel shell materialization
    /// ([`crate::mcp::servers::ShellServer`]) reads it so the model's `kj`
    /// search/synthesis tools work — without it the model shell is degraded.
    semantic_index: parking_lot::RwLock<Option<Arc<kaijutsu_index::SemanticIndex>>>,
}

impl KjDispatcher {
    pub fn new(
        drift: SharedDriftRouter,
        blocks: SharedBlockStore,
        kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
        kernel: Arc<Kernel>,
    ) -> Self {
        let kernel_id = kernel_db
            .lock()
            .kernel_id()
            .expect("KernelDb singleton row must exist");
        Self {
            drift,
            blocks,
            kernel_db,
            kernel_id,
            kernel,
            weak_self: parking_lot::RwLock::new(None),
            semantic_index: parking_lot::RwLock::new(None),
        }
    }

    /// Wire a `Weak<Self>` so internal paths can construct `KjBuiltin`
    /// (which needs `Arc<KjDispatcher>`). Call once after `Arc::new`.
    pub fn set_self_arc(self: &Arc<Self>) {
        *self.weak_self.write() = Some(Arc::downgrade(self));
    }

    /// Install the kernel's semantic index (built in the server crate, which
    /// owns the ONNX embedder). Call once at bootstrap after `Arc::new`, like
    /// [`Self::set_self_arc`]. Pass `None` when embeddings aren't configured —
    /// the model shell then degrades to non-semantic `kj` search rather than
    /// failing.
    pub fn set_semantic_index(&self, index: Option<Arc<kaijutsu_index::SemanticIndex>>) {
        *self.semantic_index.write() = index;
    }

    /// The installed semantic index, if any. Read by in-kernel shell
    /// materialization so the model's `kj search`/synthesis tools work.
    pub fn semantic_index(&self) -> Option<Arc<kaijutsu_index::SemanticIndex>> {
        self.semantic_index.read().clone()
    }

    /// A real `BlockSource` over this dispatcher's block store — hands the
    /// synthesis tools a context's block snapshots (DB-hydrating on a miss).
    /// The model shell pairs this with [`Self::semantic_index`]; rc/hook paths
    /// deliberately pass a `NoopBlockSource` instead.
    pub fn block_source(&self) -> Arc<dyn kaijutsu_index::BlockSource> {
        Arc::new(crate::kj::lifecycle::BlockStoreSource(self.blocks.clone()))
    }

    /// Upgrade the stored `Weak<Self>` to `Arc<Self>`. Returns `None`
    /// if `set_self_arc` was never called (e.g. tests that build a bare
    /// dispatcher without wrapping it).
    pub fn self_arc(&self) -> Option<Arc<Self>> {
        self.weak_self.read().as_ref().and_then(|w| w.upgrade())
    }

    /// Dispatch a parsed argv to the appropriate subcommand.
    ///
    /// Expected argv: `["context", "list", "--tree"]` (no leading "kj").
    #[tracing::instrument(
        skip(self, argv, caller),
        fields(
            cmd = argv.first().map(|s| s.as_str()).unwrap_or(""),
            ctx = caller.context_id.map(|c| c.short()).as_deref().unwrap_or("-"),
            rc_depth = caller.rc_depth,
        ),
    )]
    pub async fn dispatch(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return KjResult::Err(self.help());
        }

        // Footgun guard: a trailing bare `help` (e.g. `kj context create help`)
        // would otherwise bind as a positional and silently mint a context named
        // "help". Rewrite it to `--help` so clap renders the leaf's help. Flag
        // values (`--name help`) and non-trailing positionals are left intact.
        let argv_owned = parse::normalize_trailing_help(argv);
        let argv = argv_owned.as_slice();

        let cmd = argv[0].as_str();

        // Commands that don't strictly require an active context
        if cmd == "help" || cmd == "--help" || cmd == "-h" {
            return KjResult::ok_ephemeral(self.help(), ContentType::Markdown);
        }

        // Most context/workspace/preset subcommands work without an active context
        if cmd == "context" || cmd == "ctx" {
            return self.dispatch_context(&argv[1..], caller).await;
        }
        if cmd == "workspace" || cmd == "ws" {
            return self.dispatch_workspace(&argv[1..], caller);
        }
        if cmd == "preset" {
            return self.dispatch_preset(&argv[1..], caller);
        }
        if cmd == "cas" {
            return self.dispatch_cas(&argv[1..], caller);
        }
        if cmd == "rc" {
            return self.dispatch_rc(&argv[1..], caller).await;
        }
        // `kj editor` drives kernel-owned editor sessions by path; the session
        // binds to whatever block owns the path, so no active context is needed
        // (same exemption as `kj rc`).
        if cmd == "editor" {
            return self.dispatch_editor(&argv[1..], caller).await;
        }
        if cmd == "config" {
            return self.dispatch_config(&argv[1..], caller).await;
        }
        // `kj block` operates by --context ref when given one, so it can
        // run without an active context.
        if cmd == "block" {
            return self.dispatch_block(&argv[1..], caller);
        }
        // `kj binding` / `kj policy` take an optional <ctx>/<instance> arg and
        // default to the active context, so rc scripts (which run with an
        // active context) and external callers both reach them without a
        // mandatory join.
        if cmd == "binding" {
            return self.dispatch_binding(&argv[1..], caller).await;
        }
        if cmd == "policy" {
            return self.dispatch_policy(&argv[1..], caller).await;
        }
        // `kj search` accepts --context ref or --all, no active context
        // required. Same exemption rationale as `kj block`.
        if cmd == "search" {
            return self.dispatch_search(&argv[1..], caller);
        }
        // `kj doc` operates on the storage layer (all documents, not just
        // contexts). No active context required — list/create/delete take
        // explicit ids.
        if cmd == "doc" {
            return self.dispatch_doc(&argv[1..], caller);
        }
        // `kj attach <ctx>` brings an existing context into the current
        // session and fires the rc `attach` lifecycle on it. Like
        // `kj context switch`, the user need not have an active context
        // to attach to one.
        if cmd == "attach" {
            return self.dispatch_attach(&argv[1..], caller).await;
        }
        // `kj transport <play|pause|stop|tempo|ooda>` controls a context's beat.
        // Exempt from the active-context gate so `--context <ref>` works from a
        // session with no joined context (the beat scheduler is global).
        if cmd == "transport" {
            return self.dispatch_transport(&argv[1..], caller);
        }
        // `kj kv` is the kernel-wide key–value store — global, no context.
        if cmd == "kv" {
            return self.dispatch_kv(&argv[1..], caller);
        }
        // `kj models` is pure discovery against the LLM registry — no context.
        if cmd == "models" {
            return self.dispatch_models(&argv[1..]).await;
        }
        // `kj model` reports a context's effective model; it defaults to the
        // current context but accepts `--context <ref>`, so like `kj transport`
        // it resolves its own target rather than relying on the active-context
        // gate below (which would reject `--context` from an unjoined session).
        if cmd == "model" {
            return self.dispatch_model(&argv[1..], caller).await;
        }

        // Everything else requires an active context
        if caller.context_id.is_none() {
            return KjResult::Err("no active context joined. Use 'kj context switch <label>' to join one.".to_string());
        }

        match cmd {
            "fork" => self.dispatch_fork(&argv[1..], caller).await,
            "drive" => self.dispatch_drive(&argv[1..], caller).await,
            "stage" => self.dispatch_stage(&argv[1..], caller).await,
            "drift" => self.dispatch_drift(&argv[1..], caller).await,
            "cache" => self.dispatch_cache(&argv[1..], caller),
            other => KjResult::Err(format!(
                "kj: unknown command '{}'\n\n{}",
                other,
                self.help()
            )),
        }
    }

    fn help(&self) -> String {
        include_str!("../../docs/help/kj.md").to_string()
    }

    // Accessors for subcommand modules
    pub(crate) fn drift_router(&self) -> &SharedDriftRouter {
        &self.drift
    }

    pub(crate) fn block_store(&self) -> &SharedBlockStore {
        &self.blocks
    }

    pub fn kernel_db(&self) -> &Arc<parking_lot::Mutex<KernelDb>> {
        &self.kernel_db
    }

    pub fn kernel_id(&self) -> KernelId {
        self.kernel_id
    }

    pub(crate) fn kernel(&self) -> &Arc<Kernel> {
        &self.kernel
    }

    /// Gate an escalation-relevant `kj` verb on the **caller's** loadout.
    ///
    /// `kj` is a kaish builtin that bypasses the broker `call_tool` / facade
    /// gates entirely, so without this check a context that can merely reach a
    /// shell could drive turns, fork, merge drift, etc. regardless of its
    /// binding. This is the third enforcement surface alongside the broker and
    /// the facade gate, and like them it is **deny-by-default**.
    ///
    /// The capability is checked against the caller's *own* context binding —
    /// the loadout the actor operates under (so a musician's `drive` grant
    /// gates its OODA tick even when it drives a sibling). The authoritative
    /// binding is read from `KernelDb` (which `broker.set_binding` always
    /// writes through), keeping this synchronous so both the sync and async
    /// dispatch leaves can call it.
    ///
    /// Privileged callers (the rc lifecycle's trusted kaish) bypass: the
    /// control plane assigns and exercises loadouts.
    pub(crate) fn require_cap(
        &self,
        caller: &KjCaller,
        cap: crate::mcp::Capability,
        verb: &str,
    ) -> Result<(), KjResult> {
        if caller.privileged {
            return Ok(());
        }
        let label = binding::cap_label(&cap);
        let Some(ctx) = caller.context_id else {
            return Err(KjResult::Err(format!(
                "kj {verb}: denied — no active context to authorize against; \
                 this verb requires the '{label}' capability"
            )));
        };
        let allowed = self
            .kernel_db()
            .lock()
            .get_context_binding(ctx)
            .ok()
            .flatten()
            .map(|b| b.allows(&cap))
            .unwrap_or(false);
        if allowed {
            Ok(())
        } else {
            Err(KjResult::Err(format!(
                "kj {verb}: denied — context {} lacks the '{label}' capability \
                 (grant with `kj binding allow \"{label}\"`)",
                ctx.short()
            )))
        }
    }

    /// Summarize a context's blocks via LLM.
    ///
    /// Used by `fork --compact`, `drift pull`, and `drift merge`.
    /// Resolves the model from the context's DriftRouter entry, falling back
    /// to the registry default.
    pub(crate) async fn summarize(
        &self,
        context_id: ContextId,
        directed_prompt: Option<&str>,
    ) -> Result<String, String> {
        self.summarize_with_model(context_id, directed_prompt, None).await
    }

    /// Same as [`summarize`] but with an explicit model override (M5-F5).
    /// Use a cheaper model for distillation than the source context's
    /// chat model — `kj fork --compact --distill-model haiku` style.
    /// Pass `None` to inherit from the source context (existing behavior).
    pub(crate) async fn summarize_with_model(
        &self,
        context_id: ContextId,
        directed_prompt: Option<&str>,
        distill_model: Option<&str>,
    ) -> Result<String, String> {
        let blocks = self
            .blocks
            .block_snapshots(context_id)
            .map_err(|e| e.to_string())?;
        if blocks.is_empty() {
            return Err("context has no blocks to summarize".into());
        }

        let user_prompt = build_distillation_prompt(&blocks, directed_prompt);

        // Resolution order: explicit override > source context's model >
        // registry default.
        let inherited = self
            .drift
            .read()
            .get(context_id)
            .and_then(|h| h.model.clone());
        let chosen = distill_model
            .map(|s| s.to_string())
            .or(inherited);
        let registry = self.kernel.llm().read().await;

        let (provider, model) = match &chosen {
            Some(m) => registry
                .resolve_model(m)
                .ok_or_else(|| format!("model '{}' not found in registry", m))?,
            None => {
                let p = registry.default_provider().ok_or("no LLM configured")?;
                let m = registry
                    .default_model()
                    .ok_or("no default model configured")?
                    .to_string();
                (p, m)
            }
        };

        provider
            .prompt_with_system(&model, Some(DISTILLATION_SYSTEM_PROMPT), &user_prompt)
            .await
            .map_err(|e| format!("LLM summarization failed: {e}"))
    }
}

/// Render the auto-generated clap help text for a parser without going
/// through `try_parse_from`. Used by the clap-migrated subcommands when argv
/// is empty so we return the command's full help instead of clap's parse-error
/// for a missing subcommand. Shared across `kj` subcommand modules.
pub(crate) fn clap_help_for<T: clap::CommandFactory>() -> KjResult {
    let mut cmd = T::command();
    KjResult::ok_ephemeral(cmd.render_help().to_string(), ContentType::Plain)
}

/// Compose the full `kj` clap `Command` tree from every subcommand's derived
/// `*Args`. Used **only** for schema reflection (`KjBuiltin::schema`) — routing
/// stays in `dispatch`. The single-source guarantee holds at the leaf level
/// because each `dispatch_*` parses the same `*Args`. The top-level name/alias
/// list below must mirror `dispatch`'s match arms (see
/// docs/monday-clap-upgrades.md §2.1). Aliases (`ctx`, `ws`) ride on the
/// respective `*Args` as `visible_alias` so kaish's leaf-walker matches them.
pub(crate) fn kj_command() -> clap::Command {
    use clap::CommandFactory;
    clap::Command::new("kj")
        .about("Kernel command interface. Run `kj help` or `kj <command> help` for detailed workflows.")
        // Root-level (global) flag: kj extracts `--confirm <nonce>` in
        // KjBuiltin::execute before dispatch, so it isn't on any subcommand's
        // clap struct. Declaring it here puts it in the reflected top-level
        // params, and kaish's binder merges root params onto every leaf so the
        // trailing `… retag a b --confirm <nonce>` form binds the value.
        .arg(
            clap::Arg::new("confirm")
                .long("confirm")
                .action(clap::ArgAction::Set)
                .help("Latch confirmation nonce"),
        )
        // Root-level (global) flag: like `--confirm`, kj extracts `--json` in
        // KjBuiltin::execute before dispatch (the per-subcommand clap parsers
        // don't declare it), so it isn't on any leaf's struct. Declaring it here
        // puts it in the reflected top-level params and kaish's binder merges
        // root params onto every leaf, so `kj context list --json` binds.
        .arg(
            clap::Arg::new("json")
                .long("json")
                .action(clap::ArgAction::SetTrue)
                .help("Emit a JSON envelope {ok, exit_code, message, data} instead of human text"),
        )
        .subcommand(context::ContextArgs::command())
        .subcommand(workspace::WorkspaceArgs::command())
        .subcommand(preset::PresetArgs::command())
        .subcommand(cas::CasArgs::command())
        .subcommand(rc::RcArgs::command())
        .subcommand(editor::EditorArgs::command())
        .subcommand(config::ConfigArgs::command())
        .subcommand(block::BlockArgs::command())
        .subcommand(binding::BindingArgs::command())
        .subcommand(policy::PolicyArgs::command())
        .subcommand(search::SearchArgs::command())
        .subcommand(doc::DocArgs::command())
        .subcommand(kv::KvArgs::command())
        .subcommand(attach::AttachArgs::command())
        .subcommand(transport::TransportArgs::command())
        .subcommand(model::ModelsArgs::command())
        .subcommand(model::ModelArgs::command())
        .subcommand(fork::ForkArgs::command())
        .subcommand(drive::DriveArgs::command())
        .subcommand(stage::StageArgs::command())
        .subcommand(drift::DriftArgs::command())
        .subcommand(cache::CacheArgs::command())
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use crate::block_store::{shared_block_store, shared_block_store_with_db};
    use crate::drift::shared_drift_router;
    use crate::kernel_db::KernelDb;

    /// Create a KjDispatcher with in-memory state for testing.
    ///
    /// Must be called from an async context (e.g., `#[tokio::test]`).
    pub async fn test_dispatcher() -> KjDispatcher {
        test_dispatcher_with_timeouts(kaijutsu_types::TimeoutPolicy::default()).await
    }

    /// Variant of `test_dispatcher` that installs a custom `TimeoutPolicy`
    /// before the kernel is wrapped in `Arc`. Used by tests that need
    /// per-call bounds (rc, hooks) tighter than the production defaults.
    pub async fn test_dispatcher_with_timeouts(
        policy: kaijutsu_types::TimeoutPolicy,
    ) -> KjDispatcher {
        let drift = shared_drift_router();
        let blocks = shared_block_store(PrincipalId::system());
        let kernel_db = Arc::new(parking_lot::Mutex::new(
            KernelDb::in_memory().expect("in-memory KernelDb"),
        ));
        // Create default workspace for test contexts
        {
            let db = kernel_db.lock();
            db.get_or_create_default_workspace(PrincipalId::system())
                .unwrap();
        }
        // One throwaway root holds BOTH the kernel data_dir and the seeded
        // /etc/rc tree, so the kernel's cleanup guard removes them together when
        // the dispatcher (and its kernel) drops — no leaked `/tmp` dirs across
        // repeated test runs. `Kernel::new` takes a required data_dir (no XDG
        // fallback), so a test can never resolve CAS to the user's real store.
        let root = std::env::temp_dir().join(format!("kj-test-{}", ContextId::new().to_hex()));
        let kernel_data = root.join("kernel");
        let rc_tmp = root.join("rc");
        std::fs::create_dir_all(&kernel_data).expect("create kernel data dir");
        std::fs::create_dir_all(&rc_tmp).expect("create rc test dir");
        crate::seed_scripts::ensure_rc_seed_files(&rc_tmp).expect("seed rc test files");
        let kernel = Arc::new(
            Kernel::new("test", &kernel_data)
                .await
                .with_timeouts(policy)
                .with_temp_cleanup(root),
        );
        // Mount a host-backed /etc/rc tree (LocalBackend). `kj rc` is now
        // VFS-direct, so it works over either backend; this keeps the broadly-
        // used dispatcher db-less (a DB-backed block store deadlocks unrelated
        // fork tests — see test_dispatcher_crdt_rc). The CRDT-native backend is
        // covered by its own unit tests + test_dispatcher_crdt_rc.
        kernel
            .mount("/etc/rc", crate::vfs::LocalBackend::new(&rc_tmp))
            .await;
        // Wire the KV store so `kj kv` (and any rc that touches it) works in
        // tests, mirroring the server's startup init_kv.
        kernel
            .init_kv(kernel_db.clone())
            .expect("init kernel KV for test dispatcher");
        KjDispatcher::new(drift, blocks, kernel_db, kernel)
    }

    /// A dispatcher whose `/etc/rc` is the **real CRDT-native backend**
    /// ([`ConfigCrdtFs`]), seeded from the embedded defaults — the production
    /// wiring. Use this for `kj rc` / lifecycle tests that must exercise the
    /// CRDT path end-to-end (readdir over the `documents` manifest + VFS-direct
    /// reads/writes), not just the backend-agnostic kj layer.
    ///
    /// It uses a **DB-backed block store** (the manifest needs the `documents`
    /// table populated). That is faithful to production but currently deadlocks
    /// the `kj::fork` tests under a shared in-memory DB handle — a latent
    /// lock-ordering issue tracked separately — so it is deliberately *not* the
    /// global `test_dispatcher`; only rc-scoped tests (which never fork) use it.
    ///
    /// [`ConfigCrdtFs`]: crate::runtime::config_crdt_fs::ConfigCrdtFs
    pub async fn test_dispatcher_crdt_rc() -> KjDispatcher {
        let drift = shared_drift_router();
        let kernel_db = Arc::new(parking_lot::Mutex::new(
            KernelDb::in_memory().expect("in-memory KernelDb"),
        ));
        let ws_id = {
            let db = kernel_db.lock();
            db.get_or_create_default_workspace(PrincipalId::system())
                .unwrap()
        };
        let blocks =
            shared_block_store_with_db(kernel_db.clone(), ws_id, PrincipalId::system());
        let root = std::env::temp_dir().join(format!("kj-rc-test-{}", ContextId::new().to_hex()));
        let kernel_data = root.join("kernel");
        std::fs::create_dir_all(&kernel_data).expect("create kernel data dir");
        let kernel = Arc::new(
            Kernel::new("test", &kernel_data)
                .await
                .with_temp_cleanup(root),
        );
        let rc_fs = crate::runtime::config_crdt_fs::ConfigCrdtFs::new(blocks.clone(), "/etc/rc");
        rc_fs.seed_from_embedded().expect("seed rc into CRDT");
        kernel.mount("/etc/rc", rc_fs).await;
        // Config files live on the same CRDT-native backend type at /etc/config
        // (slice 2) — seed it too so `kj config` tests exercise the real path.
        let config_fs =
            crate::runtime::config_crdt_fs::ConfigCrdtFs::new(blocks.clone(), "/etc/config");
        config_fs
            .seed_entries(crate::config_seed::config_seed_files())
            .expect("seed config into CRDT");
        kernel.mount("/etc/config", config_fs).await;
        kernel
            .init_kv(kernel_db.clone())
            .expect("init kernel KV for test dispatcher");
        KjDispatcher::new(drift, blocks, kernel_db, kernel)
    }

    /// Install an rc script in the mounted `/etc/rc` tree, through the same
    /// VFS-direct path `kj rc` uses (write straight to the CRDT-native backend,
    /// no FileDocumentCache mirror).
    pub async fn install_rc_script_file(dispatcher: &KjDispatcher, path: &str, content: &str) {
        use crate::vfs::VfsOps;
        dispatcher
            .kernel()
            .vfs()
            .write_all(std::path::Path::new(path), content.as_bytes())
            .await
            .expect("write rc script file");
    }

    /// Create a KjCaller with fresh IDs for testing.
    ///
    /// Privileged by default: this stands in for the trusted control plane in
    /// verb-mechanics tests (its context_id is a fresh, unregistered id with no
    /// binding, so it would otherwise be denied by the kj capability gates).
    /// Capability tests construct non-privileged callers explicitly via
    /// [`caller_with_context`].
    pub fn test_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(ContextId::new()),
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: true,
        }
    }

    /// Create a caller with a specific context_id.
    pub fn caller_with_context(context_id: ContextId) -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(context_id),
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        }
    }

    /// Create a confirmed caller (for testing destructive ops post-latch).
    /// Privileged for the same reason as [`test_caller`]: it's a trusted
    /// control-plane fixture, and some destructive-op tests target unregistered
    /// contexts that carry no binding.
    pub fn confirmed_caller(context_id: ContextId) -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: Some(context_id),
            session_id: SessionId::new(),
            confirmed: true,
            rc_depth: 0,
            privileged: true,
        }
    }

    /// Register a context in both KernelDb and DriftRouter.
    pub fn register_context(
        dispatcher: &KjDispatcher,
        label: Option<&str>,
        forked_from: Option<ContextId>,
        created_by: PrincipalId,
    ) -> ContextId {
        let id = ContextId::new();
        let _kernel_id = dispatcher.kernel_id();

        // Insert document + context into KernelDb
        {
            let mut db = dispatcher.kernel_db().lock();
            let ws_id = db
                .get_or_create_default_workspace(created_by)
                .unwrap();

            // Document row first (contexts FK to documents)
            db.insert_document(&crate::kernel_db::DocumentRow {
                document_id: id,
                workspace_id: ws_id,
                doc_kind: kaijutsu_types::DocKind::Conversation,
                language: None,
                path: None,
                created_at: kaijutsu_types::now_millis() as i64,
                created_by,
            })
            .unwrap();

            let row = crate::kernel_db::ContextRow {
                context_id: id,
                label: label.map(|s| s.to_string()),
                provider: None,
                model: None,
                system_prompt: None,
                consent_mode: kaijutsu_types::ConsentMode::Collaborative,
                context_state: kaijutsu_types::ContextState::Live,
                context_type: "default".to_string(),
                created_at: kaijutsu_types::now_millis() as i64,
                created_by,
                forked_from,
                fork_kind: forked_from.map(|_| kaijutsu_types::ForkKind::Full),
                archived_at: None,
                workspace_id: None,
                preset_id: None,
                concluded_at: None,
            };
            db.insert_context(&row).unwrap();

            // Grant test contexts a fully-capable loadout by default so the kj
            // capability gates don't trip verb-*mechanics* tests. Capability
            // tests (kj::binding, the *_denied gate tests) clear or narrow this
            // explicitly via `broker().clear_binding` / `set_binding`. Written
            // straight to the DB (the authoritative source `require_cap` reads).
            let mut binding = crate::mcp::ContextToolBinding::new();
            binding.grant(crate::mcp::Capability::AllInstances);
            binding.grant(crate::mcp::Capability::AllFacades);
            binding.grant(crate::mcp::Capability::Admin);
            binding.grant(crate::mcp::Capability::RcWrite);
            binding.grant(crate::mcp::Capability::Drive);
            binding.grant(crate::mcp::Capability::Fork);
            binding.grant(crate::mcp::Capability::Drift);
            binding.grant(crate::mcp::Capability::Transport);
            binding.grant(crate::mcp::Capability::Operator);
            binding.grant(crate::mcp::Capability::ConfigWrite);
            db.upsert_context_binding(id, &binding).unwrap();
        }

        // Register in DriftRouter
        dispatcher
            .drift_router()
            .write()
            .register(id, label, forked_from, created_by)
            .unwrap();

        id
    }
}

#[cfg(test)]
mod unjoined_context_tests {
    //! Regression tests for the "no active context joined" guards.
    //!
    //! These tests exist to catch future refactors that remove either the
    //! dispatcher early-return in `dispatch()` or the per-leaf `require_context()`
    //! guards in `fork.rs` / `stage.rs` / `drift.rs` / `prompt.rs`. Without them,
    //! a caller with `context_id: None` would panic inside the kernel instead
    //! of receiving a friendly error.

    use super::test_helpers::*;
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// A caller with no joined context — the state the kernel sees when the
    /// shell dispatches `kj <cmd>` before the user has run `kj context switch`.
    fn unjoined_caller() -> KjCaller {
        KjCaller {
            principal_id: PrincipalId::new(),
            context_id: None,
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        }
    }

    fn assert_unjoined_error(result: &KjResult, cmd: &str) {
        match result {
            KjResult::Err(msg) => assert!(
                msg.contains("no active context joined"),
                "{cmd}: expected friendly unjoined error, got: {msg}"
            ),
            other => panic!("{cmd}: expected KjResult::Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn require_context_returns_friendly_err() {
        let caller = unjoined_caller();
        let err = caller.require_context().unwrap_err();
        assert_unjoined_error(&err, "require_context");
    }

    #[tokio::test]
    async fn fork_without_context_errors_friendly() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(&[s("fork"), s("--name"), s("foo")], &caller)
            .await;
        assert_unjoined_error(&result, "kj fork");
    }

    #[tokio::test]
    async fn stage_status_without_context_errors_friendly() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d.dispatch(&[s("stage"), s("status")], &caller).await;
        assert_unjoined_error(&result, "kj stage status");
    }

    #[tokio::test]
    async fn drift_push_without_context_errors_friendly() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d
            .dispatch(&[s("drift"), s("push"), s("some-target"), s("body")], &caller)
            .await;
        assert_unjoined_error(&result, "kj drift push");
    }

    #[tokio::test]
    async fn help_without_context_still_works() {
        let d = test_dispatcher().await;
        let caller = unjoined_caller();
        let result = d.dispatch(&[s("help")], &caller).await;
        assert!(
            matches!(result, KjResult::Ok { .. }),
            "kj help should succeed without a joined context, got {result:?}"
        );
    }
}

#[cfg(test)]
mod help_footgun_tests {
    //! Regression: a trailing bare `help` after a creative/destructive verb must
    //! render the leaf's clap help, NOT bind as a positional value (which used to
    //! silently mint a context named "help").

    use super::test_helpers::*;
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn context_create_help_shows_help_and_creates_nothing() {
        let d = test_dispatcher().await;
        let caller = test_caller();

        let result = d
            .dispatch(&[s("context"), s("create"), s("help")], &caller)
            .await;

        // It rendered help (ephemeral Ok), not a creation/switch result.
        match &result {
            KjResult::Ok {
                message,
                ephemeral,
                ..
            } => {
                assert!(*ephemeral, "help output should be ephemeral: {result:?}");
                assert!(
                    message.to_lowercase().contains("usage"),
                    "expected clap help text, got: {message}"
                );
            }
            other => panic!("expected help Ok, got {other:?}"),
        }

        // The headline guarantee: no context named "help" was born.
        let created = d
            .kernel_db()
            .lock()
            .find_context_by_label("help")
            .expect("db query ok");
        assert!(
            created.is_none(),
            "`kj context create help` must not mint a context labelled 'help'"
        );
    }

    #[tokio::test]
    async fn context_bare_help_shows_subcommand_help() {
        let d = test_dispatcher().await;
        let caller = test_caller();
        let result = d.dispatch(&[s("context"), s("help")], &caller).await;
        assert!(
            matches!(&result, KjResult::Ok { ephemeral: true, .. }),
            "`kj context help` should render context help, got {result:?}"
        );
    }

    #[tokio::test]
    async fn name_flag_value_help_is_not_treated_as_help_request() {
        // `--name help` is a deliberate label, not a help request — the context
        // is actually created.
        let d = test_dispatcher().await;
        let parent = register_context(&d, Some("parent"), None, PrincipalId::new());
        let caller = caller_with_context(parent);
        let result = d
            .dispatch(&[s("context"), s("create"), s("--name"), s("help")], &caller)
            .await;
        assert!(
            !matches!(&result, KjResult::Ok { message, .. } if message.to_lowercase().contains("usage")),
            "`--name help` must not trigger help rendering, got {result:?}"
        );
        let created = d
            .kernel_db()
            .lock()
            .find_context_by_label("help")
            .expect("db query ok");
        assert!(
            created.is_some(),
            "`kj context create --name help` should mint a context labelled 'help'"
        );
    }
}

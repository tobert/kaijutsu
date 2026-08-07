//! Cap'n Proto RPC server implementation.
//!
//! One shared kernel is created at server startup ([`create_shared_kernel`]),
//! shared across all SSH connections via `Arc`. Per-connection state
//! (principal, kaish, command history) lives in [`ConnectionState`]. The
//! primary RPC surface is [`KernelImpl`], which implements the capnp
//! `kernel::Server` trait.
//!
//! # Why this file is large
//!
//! This file is intentionally kept as a single module rather than decomposed
//! into a `rpc/` directory of per-domain submodules. The `kernel::Server`
//! trait impl is ~6k lines on its own and must be contiguous — a capnp
//! constraint — so a mechanical split would produce one thin file of
//! delegating trait methods plus one-file-per-subject of inherent `impl`
//! blocks, doubling surface area without gaining real modularity. File-size
//! ergonomics matter less in an AI-assisted workflow where grep, LSP, and
//! the editor's outline view handle navigation. Parallel work resolves
//! cleanly per-hunk: moving methods into separate files doesn't reduce
//! merge conflict surface, it just reshuffles it.
//!
//! If a specific chunk of code grows its own identity (the LLM agentic
//! loop is the obvious example — see [`crate::llm_stream`]), extract it.
//! Don't decompose the file just to have smaller files.
//!
//! # Navigation
//!
//! Section banners use `// ========` and are grep-able with `rg '^// ='`.
//! Top-level sections, in order:
//!
//! - **Server State** — [`ConnectionState`], [`ConversationCache`],
//!   [`SharedKernelState`], execution tracking helpers.
//! - **Execute Output Dispatch** — background fan-out of `execute()`
//!   output events to subscribers.
//! - **Semantic Index Integration** — `BlockSource` adapter for kaijutsu-index.
//! - **Shared Kernel Creation** — [`create_shared_kernel`] (server startup).
//! - **World Implementation** — [`WorldImpl`] (top-level capability).
//! - **Kernel Implementation** — [`KernelImpl`], the main trait impl. Its
//!   internal subsections group ~80 RPC methods: lifecycle/info, shell
//!   execution (`execute`/`interrupt`/`complete`), VFS, tools, block CRDT
//!   ops, prompt/LLM, context ops, MCP, shell execution (kaish), shell
//!   state, blob placeholders, MCP resources, peer attachment, timeline
//!   navigation, config, drift, LLM configuration, tool filter
//!   configuration, per-context tool filter, shell variable introspection,
//!   input document operations, context interrupt.
//! - **Shell Value Conversion Helpers** — kaish `Value` ↔ Cap'n Proto.
//! - **OutputData Build Helpers** — structured command output builders.
//! - **Peer Helper Functions** — `PeerInfo` capnp converters.
//! - **Cap'n Proto ↔ Rust Type Helpers** — tool filter (de)serialization.
//! - **Shell Execution Dispatch** — [`execute_shell_command`], shared by
//!   the `shell_execute` RPC and `submit_input`.
//! - **Utility Functions** — block ID / filter / status converters.
//! - **VFS Implementation** — [`VfsImpl`] (filesystem capability).
//! - **Synthesis** — Rhai-driven keyword extraction.
//!
//! LLM streaming (`process_llm_stream` and its agentic loop) lives in
//! [`crate::llm_stream`], not in this file.

#![allow(refining_impl_trait)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;
// tokio::sync::Mutex used inside ConversationCache for per-context locking

use capnp::capability::Promise;
use capnp_rpc::pry;

use kaijutsu_kernel::runtime::embedded_kaish::EmbeddedKaish;
use crate::interrupt::ContextInterruptState;
use crate::kaijutsu_capnp::*;
use crate::llm_stream::spawn_llm_for_prompt;

use kaijutsu_crdt::{BlockKind, ContentType, Role, Status, TaskStatus};
// `derive_context_live_status` moved to kaijutsu-kernel (single source of
// truth shared with `BlockStore::live_status`'s incremental cache); only
// `live_status_tests` below still exercises it directly, so the import is
// test-only in this crate.
#[cfg(test)]
use kaijutsu_kernel::block_store::derive_context_live_status;
use kaijutsu_kernel::kernel_db::{
    context_used_pct, ContextRow, ContextShellRow, ContextUsageRow, DemoteOutcome, KernelDb,
    KernelDbError, PromoteOutcome,
};
use kaijutsu_kernel::{
    // FlowBus
    BlockFlow,
    // Conversation session
    ConversationMailbox,
    InputDocFlow,
    InvokeRequest,
    InvokeResponse,
    Kernel,
    LocalBackend,
    // Peers (drift navigation transport)
    peer_key,
    PeerConfig,
    PeerInfo,
    SharedBlockFlowBus,
    SharedBlockStore,
    SharedInputDocFlowBus,
    VfsOps,
    // VFS activity digest stream (Lane K, FSN slice-1)
    ActivityCursor,
    block_store::BlockStore,
    flows::EditorFlow,
    flows::FlowMessage,
    flows::{TurnFlow, TurnOrigin, TurnStopReason},
    shared_block_flow_bus,
    shared_input_doc_flow_bus,
};
// D-57 (docs/acp.md gap #2): the `HookAction::Ask` <-> `PermissionEvents`
// bridge. `kaijutsu-kernel` has no capnp dependency, so the trait + plain
// request/answer/outcome types live there; the capnp `Client`s they get
// bridged to live only here.
use kaijutsu_kernel::mcp::{PermissionAnswer, PermissionAskRequest, PermissionAsker, PermissionOutcome};
use kaijutsu_types::paths;
use kaijutsu_types::{ContextId, KernelId, Principal, PrincipalId, SessionId};
// Alias to avoid conflict with kaijutsu_capnp::ToolKind (glob-imported)
use kaijutsu_types::ToolKind as TypesToolKind;
use serde_json;
use tracing::Instrument;

/// Extract W3C Trace Context from a Cap'n Proto `TraceContext` reader.
///
/// Returns a tracing span linked to the remote parent (or a root span if empty).
/// Safe to call even when trace is not present — returns a detached span.
fn extract_rpc_trace(
    trace: capnp::Result<trace_context::Reader<'_>>,
    name: &'static str,
) -> tracing::Span {
    let (traceparent, tracestate) = match trace {
        Ok(t) => {
            let tp = t
                .get_traceparent()
                .ok()
                .and_then(|r| r.to_str().ok())
                .unwrap_or("");
            let ts = t
                .get_tracestate()
                .ok()
                .and_then(|r| r.to_str().ok())
                .unwrap_or("");
            (tp.to_string(), ts.to_string())
        }
        Err(_) => (String::new(), String::new()),
    };
    let span = kaijutsu_telemetry::extract_trace_context(&traceparent, &tracestate);
    // Override the default "rpc.request" name with the actual method name
    let named_span = tracing::info_span!(parent: &span, "rpc", method = name);
    named_span
}

// ============================================================================
// Server State
// ============================================================================

/// Per-context conversation sessions, each behind its own lock so
/// concurrent prompts to the same context serialize properly.
///
/// Each entry is a [`ConversationMailbox`] — the live, append-only
/// session for that context (see `docs/conversation-session.md`).
/// The LLM-stream path calls `mailbox.catch_up(&block_snapshots)` to
/// fold any blocks that landed since the last turn, then `snapshot()`
/// for the wire-history view. DashMap provides outer concurrent
/// access; LRU eviction keeps memory bounded — an evicted context
/// re-hydrates from blocks on next touch.
///
/// Also owns the per-hash [`ImageBase64Cache`] so resolving the same
/// screenshot every turn doesn't re-encode bytes. The image cache is
/// keyed by content hash (global) but shares lifetime with this
/// struct because both belong to the same LLM-stream subsystem.
pub struct ConversationCache {
    entries: dashmap::DashMap<ContextId, Arc<tokio::sync::Mutex<ConversationMailbox>>>,
    last_accessed: dashmap::DashMap<ContextId, std::time::Instant>,
    max_contexts: usize,
    image_cache: Arc<kaijutsu_kernel::llm::image_cache::ImageBase64Cache>,
}

impl ConversationCache {
    /// Create a new cache with the given capacity.
    ///
    /// `max_images` bounds the per-hash image cache; pick a value that covers
    /// the longest conversation you expect to keep hot (4× max_contexts is
    /// a defensible default).
    pub fn new(max_contexts: usize) -> Self {
        let max_images = max_contexts.saturating_mul(4).max(16);
        Self {
            entries: dashmap::DashMap::new(),
            last_accessed: dashmap::DashMap::new(),
            max_contexts,
            image_cache: Arc::new(kaijutsu_kernel::llm::image_cache::ImageBase64Cache::new(
                max_images,
            )),
        }
    }

    /// Borrow the per-hash image cache shared across contexts.
    pub fn image_cache(&self) -> &kaijutsu_kernel::llm::image_cache::ImageBase64Cache {
        &self.image_cache
    }

    /// Get or create the per-context mailbox lock. Returns an
    /// `Arc<Mutex<ConversationMailbox>>` — the caller holds the lock
    /// for the entire `process_llm_stream`, serializing concurrent
    /// prompts to the same context.
    pub fn get_or_create(
        &self,
        ctx: ContextId,
    ) -> Arc<tokio::sync::Mutex<ConversationMailbox>> {
        self.last_accessed.insert(ctx, std::time::Instant::now());

        if let Some(entry) = self.entries.get(&ctx) {
            return entry.clone();
        }

        // Evict LRU if at capacity (skip entries with strong_count > 1, they're in active use)
        if self.entries.len() >= self.max_contexts {
            let mut oldest: Option<(ContextId, std::time::Instant)> = None;
            for entry in self.last_accessed.iter() {
                let ctx_id = *entry.key();
                let accessed = *entry.value();
                // Skip entries in active use
                if let Some(e) = self.entries.get(&ctx_id)
                    && Arc::strong_count(&e) > 1
                {
                    continue;
                }
                if oldest.is_none() || accessed < oldest.unwrap().1 {
                    oldest = Some((ctx_id, accessed));
                }
            }
            if let Some((evict_id, _)) = oldest {
                self.entries.remove(&evict_id);
                self.last_accessed.remove(&evict_id);
            }
        }

        let lock = Arc::new(tokio::sync::Mutex::new(ConversationMailbox::new()));
        self.entries.insert(ctx, lock.clone());
        lock
    }
}

/// One live FlowBus block-event subscription in the per-(principal, instance)
/// dedupe registry — see [`SharedKernelState::subscription_registry`].
pub struct SubscriptionEntry {
    abort: tokio::task::AbortHandle,
    /// Which connection (SSH session) registered this subscription. Lets a
    /// replacement distinguish "this connection re-issuing the RPC" (quiet)
    /// from "a different connection claimed the same (principal, instance)
    /// slot" (logged) — see [`register_subscription`].
    session_id: SessionId,
    /// When this entry was registered. Reported as the replaced
    /// subscription's age on a cross-connection replacement: a young age
    /// (both connections live at once) smells like two clients trampling one
    /// hardcoded instance literal; an old age smells like an ordinary
    /// reconnect after a dead socket.
    registered_at: std::time::Instant,
}

/// Outcome of [`register_subscription`] inserting a new entry.
#[derive(Debug, PartialEq, Eq)]
enum SubscriptionRegistration {
    /// No prior subscription existed for this (principal, instance).
    Fresh,
    /// A prior subscription existed but its task had already finished
    /// naturally (its connection dropped, or `SubscriberHealth` reaped it) —
    /// nothing live was displaced.
    ReplacedFinished,
    /// A prior LIVE subscription from the SAME connection was displaced —
    /// this connection re-issuing `subscribeBlocksFiltered` (e.g. a changed
    /// filter). Ordinary; the caller should stay quiet or log at debug.
    ReplacedSameConnection,
    /// A prior LIVE subscription from a DIFFERENT connection was displaced.
    /// Could be a legitimate reconnect (same logical client, new SSH
    /// session) or two live clients sharing one hardcoded `instance` literal
    /// (docs/issues.md, "MCP shell delay" — the bug this registry exists to
    /// catch). The caller should warn, including `prior_session_id` and
    /// `age` so an operator can tell the two apart.
    ReplacedDifferentConnection {
        prior_session_id: SessionId,
        age: std::time::Duration,
    },
}

/// Insert `entry` for `key` in the dedupe registry, aborting whatever live
/// task was there before. Pure registry mutation with no tracing calls, so
/// the replace-vs-coexist decision is unit-testable independent of the RPC
/// plumbing; the caller logs based on the returned [`SubscriptionRegistration`].
fn register_subscription(
    registry: &parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>>,
    key: (PrincipalId, String),
    session_id: SessionId,
    abort: tokio::task::AbortHandle,
) -> SubscriptionRegistration {
    let new_entry = SubscriptionEntry {
        abort,
        session_id,
        registered_at: std::time::Instant::now(),
    };
    let prior = {
        let mut reg = registry.lock();
        reg.insert(key, new_entry)
    };
    match prior {
        None => SubscriptionRegistration::Fresh,
        Some(prior) if prior.abort.is_finished() => SubscriptionRegistration::ReplacedFinished,
        Some(prior) if prior.session_id == session_id => {
            prior.abort.abort();
            SubscriptionRegistration::ReplacedSameConnection
        }
        Some(prior) => {
            let age = prior.registered_at.elapsed();
            let prior_session_id = prior.session_id;
            prior.abort.abort();
            SubscriptionRegistration::ReplacedDifferentConnection {
                prior_session_id,
                age,
            }
        }
    }
}

/// Cross-thread dispatch message for one permission ask, mirroring
/// `peers::InvokeRequest`'s shape and for the same reason: capnp `Client`s
/// are `Rc`-based hooks (`!Send`), but `SharedKernelState` (and the
/// `PermissionAsker` trait object hung off the broker) must be `Send +
/// Sync` — a connection runs on its own dedicated thread + `LocalSet`
/// (`spawn_turn_driver`'s doc comment explains why). So the shared registry
/// can only ever hold `Send`-safe channel handles; the actual
/// `permission_events::Client` stays local to the connection thread that
/// registered it, inside `subscribe_permission_events`'s bridge task.
pub struct PermissionAskChannelMsg {
    request: PermissionAskRequest,
    reply: tokio::sync::oneshot::Sender<PermissionOutcome>,
}

/// Bridges `kaijutsu-kernel`'s `PermissionAsker` trait onto the kernel-wide
/// registry of per-connection dispatch channels (D-57, docs/acp.md gap #2).
/// Wired onto `Broker::set_permission_asker` in `create_shared_kernel`.
///
/// No internal timeout here: `Broker::run_permission_ask` already wraps the
/// whole `ask()` call in `tokio::time::timeout` (the "generous" D-57
/// budget). Dropping this future on timeout drops the oneshot receiver,
/// which the bridge task's next `reply.send()` then just fails to reach —
/// safe, no leak, same shape `subscribe_turn_events`'s per-callback timeout
/// relies on. This type only decides who to ask and what they said.
struct PermissionAskBridge {
    subscribers: Arc<TokioRwLock<Vec<tokio::sync::mpsc::Sender<PermissionAskChannelMsg>>>>,
}

#[async_trait::async_trait]
impl PermissionAsker for PermissionAskBridge {
    async fn ask(&self, request: PermissionAskRequest) -> PermissionOutcome {
        // Snapshot once, then walk it in subscribe order (oldest first).
        // A dead channel (its connection's bridge task ended) is pruned
        // from the shared registry via `same_channel` — `mpsc::Sender`
        // has no other identity to prune by. A capnp-level failure inside
        // a still-live bridge task answers `NoSubscriber` over its reply
        // channel instead (see `subscribe_permission_events`) rather than
        // closing the channel, so a live-but-momentarily-failing
        // subscriber isn't torn out of the registry — just skipped for
        // this one ask.
        let snapshot = self.subscribers.read().await.clone();
        if snapshot.is_empty() {
            return PermissionOutcome::NoSubscriber;
        }

        for tx in &snapshot {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let msg = PermissionAskChannelMsg {
                request: request.clone(),
                reply: reply_tx,
            };
            if tx.send(msg).await.is_err() {
                tracing::debug!(
                    target: "kaijutsu::hooks",
                    request_id = %request.request_id,
                    "permission ask subscriber's bridge task is gone; pruning and trying the next one",
                );
                let mut subs = self.subscribers.write().await;
                subs.retain(|s| !s.same_channel(tx));
                continue;
            }
            match reply_rx.await {
                Ok(PermissionOutcome::NoSubscriber) => continue,
                Ok(outcome) => return outcome,
                Err(_) => {
                    tracing::warn!(
                        target: "kaijutsu::hooks",
                        request_id = %request.request_id,
                        "permission ask bridge task dropped without answering; trying the next subscriber",
                    );
                    continue;
                }
            }
        }
        // Every attached subscriber was unreachable/declined to answer.
        PermissionOutcome::NoSubscriber
    }
}

/// Kernel state shared across all connections via Arc.
/// Created once at server startup.
pub struct SharedKernelState {
    pub id: KernelId,
    pub name: String,
    pub kernel: Arc<Kernel>,
    pub documents: SharedBlockStore,
    pub conversation_cache: Arc<ConversationCache>,
    /// SQLite persistence for context metadata, edges, presets, workspaces.
    /// Arc<parking_lot::Mutex> (not tokio) — shared with KjDispatcher, all ops sync and sub-ms.
    pub kernel_db: Arc<parking_lot::Mutex<KernelDb>>,
    /// Semantic vector index for context search/clustering.
    /// None if embedding model not configured or unavailable.
    pub semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
    /// Per-context interrupt state. Created fresh at the start of each
    /// `process_llm_stream` call; looked up by `interruptContext` RPC.
    pub context_interrupts: Arc<TokioRwLock<HashMap<ContextId, Arc<ContextInterruptState>>>>,
    /// Monotonically increasing generation counter for interrupt state.
    /// Prevents race where stream A's cleanup removes stream B's interrupt.
    pub interrupt_generation: AtomicU64,
    /// kj command dispatcher — shared across all connections.
    pub kj_dispatcher: Arc<kaijutsu_kernel::KjDispatcher>,
    /// Per-session current-context tracking for the `context` shell command.
    pub session_contexts: kaijutsu_kernel::runtime::context_engine::SessionContextMap,
    /// Per-(principal, client-instance) registry of live FlowBus subscriptions.
    ///
    /// When a client calls `subscribeBlocksFiltered` with the same
    /// `(principal, instance)` key as a prior live subscription, the old
    /// task is aborted before the new one starts. Without this, a client
    /// that reconnects after a silent socket death leaks one bridge task
    /// per reconnect cycle — the orphan holds the dead callback capability
    /// and (until the connection's `conn_cancel` finally fires) keeps the
    /// FlowBus subscriber alive on the kernel side.
    ///
    /// `parking_lot::Mutex` because all ops are insert/remove/replace and
    /// complete in microseconds.
    pub subscription_registry: Arc<parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>>>,
    /// Kernel-wide `PermissionEvents` subscriber registry (D-57, docs/acp.md
    /// gap #2): one dispatch channel per subscribed connection, drained by
    /// `PermissionAskBridge::ask` (wired onto `Broker::set_permission_asker`
    /// at bootstrap). The actual `permission_events::Client` capability
    /// stays on its owning connection's thread — see `PermissionAskBridge`'s
    /// doc comment for why (capnp `Client`s are `!Send`, this registry is
    /// not).
    ///
    /// Kernel-wide, not per-connection like `elicitation_subscribers`:
    /// `HookAction::Ask` can fire from any call path — an autonomous turn,
    /// a sibling context's tool call, a kaish script — so there's no "the
    /// connection that asked" to scope this to. A closed channel (its
    /// connection's bridge task ended) is pruned reactively by the bridge.
    pub permission_subscribers:
        Arc<TokioRwLock<Vec<tokio::sync::mpsc::Sender<PermissionAskChannelMsg>>>>,
}

pub type SharedKernel = Arc<SharedKernelState>;

impl Drop for SharedKernelState {
    fn drop(&mut self) {
        // Best-effort WAL checkpoint on clean teardown so the main `.db` file
        // doesn't linger behind committed history after exit. This fires only
        // when the LAST `Arc<SharedKernelState>` drops — a clean process exit
        // or test teardown. It does NOT run on SIGKILL/SIGTERM: the server's
        // run loop never returns and the process dies without unwinding. So
        // this is insurance, not the primary durability path — the proactive
        // checkpoint after each compaction is (see `BlockStore::compact_*`).
        // The remaining gap (a SIGTERM handler for systemd `stop`) is tracked
        // in docs/issues.md under "graceful-shutdown WAL checkpoint".
        let db = self.kernel_db.lock();
        match db.checkpoint() {
            Ok((busy, _, _)) if busy != 0 => {
                log::debug!("shutdown wal_checkpoint(TRUNCATE) busy; WAL left for next open");
            }
            Ok(_) => {}
            Err(e) => log::warn!("shutdown wal_checkpoint failed: {e}"),
        }
    }
}

impl SharedKernelState {
    /// Create a fresh `ContextInterruptState` for a new prompt, replacing any previous entry.
    ///
    /// `CancellationToken` cannot be reset — so each prompt gets a new one.
    /// Returns the interrupt state and its generation number. The generation
    /// must be passed to `remove_interrupt` to prevent the race where stream A's
    /// cleanup removes stream B's newer interrupt.
    pub async fn create_interrupt(
        &self,
        context_id: ContextId,
    ) -> (Arc<ContextInterruptState>, u64) {
        let generation = self.interrupt_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let state = ContextInterruptState::new(generation);
        let mut map = self.context_interrupts.write().await;
        map.insert(context_id, state.clone());
        (state, generation)
    }

    /// Look up an existing interrupt state for a context.
    ///
    /// Returns `None` if the context has no active interrupt (nothing running).
    pub async fn get_interrupt(&self, context_id: ContextId) -> Option<Arc<ContextInterruptState>> {
        let map = self.context_interrupts.read().await;
        map.get(&context_id).cloned()
    }

    /// Remove the interrupt state for a context (called when stream finishes).
    ///
    /// Only removes the entry if `generation` matches the current state's
    /// generation, preventing a stale stream from removing a newer stream's
    /// interrupt state.
    pub async fn remove_interrupt(&self, context_id: ContextId, generation: u64) {
        let mut map = self.context_interrupts.write().await;
        if let Some(state) = map.get(&context_id)
            && state.generation == generation
        {
            map.remove(&context_id);
        }
    }
}

/// Server-wide state. Shared via Arc across all SSH connections.
pub struct ServerRegistry {
    pub kernel: SharedKernel,
}

/// Spawn the server-lifetime turn driver.
///
/// This is the server half of the headless-drive keystone: it drains the
/// kernel's `turn.requested` FlowBus and runs an autonomous LLM turn for each
/// request. Producers are kernel-side code that can't reach the turn driver
/// directly — today `kj fork --prompt`, later drift-wake and the cruise
/// director.
///
/// It runs on its own dedicated thread with a current-thread runtime + LocalSet,
/// mirroring the per-connection RPC threads in `ssh.rs`. That's deliberate, not
/// incidental: `spawn_llm_for_prompt` uses `spawn_local`, so it needs a LocalSet
/// to run on, and the per-connection LocalSets aren't a fit — the turn bus is a
/// broadcast, so hosting a subscriber on every connection would drive each
/// request once per connection. One driver, one subscription, one turn.
pub fn spawn_turn_driver(registry: Arc<ServerRegistry>) {
    let builder = std::thread::Builder::new().name("turn-driver".to_string());
    if let Err(e) = builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("turn-driver: failed to build runtime: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let kernel = &registry.kernel;
            let mut sub = kernel.kernel.turn_flows().subscribe("turn.requested");
            log::info!("Turn driver online");
            while let Some(msg) = sub.recv().await {
                let TurnFlow::Requested {
                    context_id,
                    after_block_id,
                    // The user block is already in the log (anchored by
                    // `after_block_id`); hydration reads it from there.
                    content: _,
                    principal_id,
                    model,
                } = msg.payload
                else {
                    // The driver only subscribes to turn.requested. Completion
                    // and failure are observed on their own topics — in-process
                    // by the beat scheduler, and over the wire by
                    // `subscribeTurnEvents` subscribers (a future `kj wait`
                    // joins there, not here).
                    continue;
                };
                // Headless turn: no interactive session, so synthesize the
                // execution context — but read the context's durable cwd the
                // same way the interactive paths do. It used to hardcode `/`
                // on the theory that "the turn itself doesn't need it"; the
                // turn's file tools use cwd as the *walk root* for a pathless
                // `glob`/`grep`, so rooting it at `/` sent every autonomous
                // turn (kj drive, ACP prompts, the musician's OODA loop)
                // walking the whole filesystem — above any repo, so
                // .gitignore could not apply and the walk fell into
                // build-output trees until the 120s tool timeout fired. A
                // context with no durable cwd now stays `None` all the way
                // through: the file tools reject a missing cwd outright
                // instead of resolving relative paths against a fabricated
                // root (`servers/file.rs`'s up-front rejection).
                let tool_ctx = match context_cwd(kernel, context_id) {
                    Some(cwd) => kaijutsu_kernel::ExecContext::new(
                        principal_id,
                        context_id,
                        cwd,
                        kaijutsu_types::SessionId::new(),
                        kernel.kernel.id(),
                    ),
                    None => kaijutsu_kernel::ExecContext::new_without_cwd(
                        principal_id,
                        context_id,
                        kaijutsu_types::SessionId::new(),
                        kernel.kernel.id(),
                    ),
                };
                match spawn_llm_for_prompt(
                    kernel,
                    context_id,
                    model.as_deref(),
                    &after_block_id,
                    tool_ctx,
                    principal_id,
                    // The autonomous turn-driver path (the musician's OODA loop).
                    // `process_llm_stream` publishes the terminal `TurnFlow` at
                    // ACTUAL stream end with the real output block id (design §7);
                    // the old publish-at-spawn here is gone — it fired before the
                    // model wrote anything and raced the stream, so the beat
                    // scheduler read the seed prompt instead of the Act. The
                    // origin rides onto that event so the scheduler can tell an
                    // autonomous turn (which feeds the Act) from a human one.
                    TurnOrigin::Autonomous,
                )
                .await
                {
                    // Spawn succeeded — the stream owns the terminal Completed/Failed
                    // publish at its end. Nothing to publish here; doing so would
                    // double-announce (and at the wrong time, with no output id).
                    Ok(()) => {}
                    Err(e) => {
                        let err = e.to_string();
                        log::warn!(
                            "turn.requested: failed to drive turn for {context_id}: {err}"
                        );
                        // Surface the failure as a visible Error block in the
                        // child context — same `insert_error_block_as` error-block
                        // API the LLM stream uses (llm_stream.rs report_llm_error),
                        // anchored at the turn's `after_block_id` — so the dropped
                        // turn isn't silently invisible.
                        let payload = kaijutsu_types::ErrorPayload {
                            category: kaijutsu_types::ErrorCategory::Stream,
                            severity: kaijutsu_types::ErrorSeverity::Error,
                            code: None,
                            detail: Some(format!(
                                "autonomous turn failed to run for this context: {err}"
                            )),
                            span: None,
                            source_kind: None,
                        };
                        let summary = payload.summary_line();
                        if let Err(insert_err) = kernel.documents.insert_error_block_as(
                            context_id,
                            &after_block_id,
                            &payload,
                            summary,
                            Some(principal_id),
                        ) {
                            log::warn!(
                                "turn.requested: failed to insert error block for {context_id}: {insert_err}"
                            );
                        }
                        kernel.kernel.turn_flows().publish(TurnFlow::Failed {
                            context_id,
                            principal_id,
                            error: err,
                            origin: TurnOrigin::Autonomous,
                        });
                    }
                }
            }
            log::warn!("Turn driver: turn bus closed, driver exiting");
        });
    }) {
        log::error!("Failed to spawn turn-driver thread: {e}");
    }
}

/// Spawn the server-lifetime editor reconciler — the remote-merge half of the
/// editor push channel (docs/vi.md step 1b).
///
/// It drains the kernel's `block.text_ops` FlowBus and, for each edited block,
/// reconciles any open editor session bound to that block against the block's
/// merged text and pushes the new state. A session's own mirror write is a
/// no-op (its buffer already equals the block), so this only fires for *other*
/// writers — a sibling editor session, an MCP file edit, a streaming turn.
///
/// One reconciler for the whole server: the block flow is a broadcast, so a
/// per-connection subscriber would reconcile each edit once per connection. The
/// dedicated thread + LocalSet mirrors `spawn_turn_driver` (the reconcile path
/// touches the `!Send` editor sessions behind the kernel mutex).
pub fn spawn_editor_reconciler(registry: Arc<ServerRegistry>) {
    let builder = std::thread::Builder::new().name("editor-reconciler".to_string());
    if let Err(e) = builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("editor-reconciler: failed to build runtime: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let kernel = &registry.kernel;
            let mut sub = kernel.kernel.block_flows().subscribe("block.text_ops");
            log::info!("Editor reconciler online");
            while let Some(msg) = sub.recv().await {
                if let BlockFlow::TextOps {
                    context_id,
                    ref block_id,
                    ..
                } = msg.payload
                {
                    kernel
                        .kernel
                        .editor_reconcile_block(context_id, *block_id, &kernel.documents);
                }
            }
            log::warn!("Editor reconciler: block flow closed, exiting");
        });
    }) {
        log::error!("Failed to spawn editor-reconciler thread: {e}");
    }
}

/// A background execution tracked by exec_id.
struct RunningExecution {
    cancel: CancellationToken,
}

/// Per-connection state. Lives in each connection's LocalSet.
pub struct ConnectionState {
    pub principal: Principal,
    pub session_id: SessionId,
    /// Global session map for context tracking.
    pub session_contexts: kaijutsu_kernel::runtime::context_engine::SessionContextMap,
    pub command_history: Vec<CommandEntry>,
    next_exec_id: AtomicU64,
    /// Currently running executions, keyed by exec_id.
    running_executions: HashMap<u64, RunningExecution>,
    /// Output subscribers registered via subscribe_output().
    output_subscribers: Vec<kernel_output::Client>,
    /// Elicitation subscribers (M3-D6). The connection registers a
    /// callback via `subscribeMcpElicitations`; the kernel-side path
    /// calls `onRequest` on the first registered client when an MCP
    /// server emits `ServerNotification::Elicitation` (emitter wiring
    /// follows in the streaming effort).
    elicitation_subscribers: Vec<crate::kaijutsu_capnp::elicitation_events::Client>,
    /// Cancelled when the connection's RPC system tears down. Long-running
    /// spawn_local tasks (FlowBus bridge, etc.) tokio::select! on this so
    /// they shut down promptly instead of leaking onto the LocalSet.
    conn_cancel: CancellationToken,
    /// The other direction: fired BY a background task to take this whole
    /// connection down. `run_rpc` selects on it alongside the RpcSystem.
    ///
    /// The one caller today is the block bridge, when its per-subscriber queue
    /// overflows: the client is cut off rather than handed a stream with a hole
    /// in it (Amy, 2026-08-05: "I'd rather be disconnected"). Dropping the
    /// connection — not just the subscription — is what makes recovery work for
    /// EVERY client, including binaries that predate `onSubscriptionTerminated`:
    /// the actor's existing reconnect-with-full-resync path is already the
    /// tested route back to a consistent view.
    disconnect: CancellationToken,
    /// What this connection's push subscriptions can handle, as declared via
    /// `declareEventCapabilities`. Conservative until the client says otherwise.
    event_caps: EventCapabilities,
    /// The kernel's MIDI presence store, attached the first time this
    /// connection reports presence (`docs/midi-next.md` "Presence is
    /// sink-fed"). Held so `Drop` can reap what this connection claimed —
    /// a sink that crashes never sends its unplug, so presence must die with
    /// the connection rather than outlive it as a lie. `None` on a connection
    /// that never reported presence, which is most of them.
    midi_presence: Option<Arc<kaijutsu_kernel::midi_presence::MidiPresenceStore>>,
    /// The kernel's MIDI exchange registry, attached when this connection
    /// registered a sink bridge (`docs/midi-next.md` "SysEx: the exchange
    /// pattern"). Held for the same reason as `midi_presence`: a sink that
    /// crashes never gets to withdraw itself, and a registry entry nobody is
    /// draining would make every exchange wait for the kernel's deadline
    /// instead of answering "that sink is gone" at once.
    midi_exchange: Option<Arc<kaijutsu_kernel::midi_exchange::MidiExchangeRegistry>>,
}

impl ConnectionState {
    pub fn new(
        principal: Principal,
        session_contexts: kaijutsu_kernel::runtime::context_engine::SessionContextMap,
    ) -> Self {
        Self {
            principal,
            session_id: SessionId::new(),
            session_contexts,
            command_history: Vec::new(),
            next_exec_id: AtomicU64::new(1),
            running_executions: HashMap::new(),
            output_subscribers: Vec::new(),
            elicitation_subscribers: Vec::new(),
            conn_cancel: CancellationToken::new(),
            disconnect: CancellationToken::new(),
            event_caps: EventCapabilities::default(),
            midi_presence: None,
            midi_exchange: None,
        }
    }

    /// Remember the presence store this connection is writing into, so
    /// `Drop` can reap its records. Called from `report_midi_presence` on
    /// every report (idempotent) rather than at connect time: only a
    /// connection that actually reported presence has anything to reap, and
    /// the store handle then arrives from the same path that created the
    /// obligation.
    fn attach_midi_presence(
        &mut self,
        store: Arc<kaijutsu_kernel::midi_presence::MidiPresenceStore>,
    ) {
        if self.midi_presence.is_none() {
            self.midi_presence = Some(store);
        }
    }

    /// Remember the exchange registry this connection registered a sink in,
    /// so `Drop` can withdraw it. Idempotent, and called from the subscribe
    /// path for the same reason `attach_midi_presence` is called from the
    /// report path: only a connection that actually registered has anything
    /// to withdraw.
    fn attach_midi_exchange(
        &mut self,
        registry: Arc<kaijutsu_kernel::midi_exchange::MidiExchangeRegistry>,
    ) {
        if self.midi_exchange.is_none() {
            self.midi_exchange = Some(registry);
        }
    }

    /// Cancellation token that fires when the connection's RPC system
    /// tears down. Background tasks spawned on the connection's LocalSet
    /// should observe this in a `tokio::select!` so they can exit promptly.
    pub fn cancel_token(&self) -> CancellationToken {
        self.conn_cancel.clone()
    }

    /// Token a background task fires to bring this connection down (see
    /// [`ConnectionState::disconnect`]). `run_rpc` awaits it.
    pub fn disconnect_token(&self) -> CancellationToken {
        self.disconnect.clone()
    }

    /// What this connection's push subscriptions can handle.
    pub fn event_capabilities(&self) -> EventCapabilities {
        self.event_caps
    }

    /// Record the client's declared push capabilities. Read when a bridge
    /// starts, so a client must declare before it subscribes.
    pub fn set_event_capabilities(&mut self, caps: EventCapabilities) {
        self.event_caps = caps;
    }

    /// Get the connection's active context, or error if none joined.
    pub fn require_context(&self) -> Result<ContextId, capnp::Error> {
        self.session_contexts
            .get(&self.session_id)
            .map(|r| *r)
            .ok_or_else(|| {
                capnp::Error::failed("no context joined — call joinContext first".into())
            })
    }

    fn next_exec_id(&self) -> u64 {
        self.next_exec_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Register a new execution, returning the cancellation token.
    fn register_execution(&mut self, exec_id: u64) -> CancellationToken {
        let token = CancellationToken::new();
        self.running_executions.insert(
            exec_id,
            RunningExecution {
                cancel: token.clone(),
            },
        );
        token
    }

    /// Remove a completed execution from tracking.
    fn complete_execution(&mut self, exec_id: u64) {
        self.running_executions.remove(&exec_id);
    }

    /// Check if any execution is currently in-flight.
    fn has_running_execution(&self) -> bool {
        !self.running_executions.is_empty()
    }

    /// Cancel every in-flight execution on this connection. The `execute`
    /// path's `tokio::select!` arm observes the token and calls
    /// `kaish.cancel()`, aborting the materialized shell mid-command.
    fn cancel_running_executions(&self) {
        for running in self.running_executions.values() {
            running.cancel.cancel();
        }
    }

    /// Register an output subscriber callback.
    fn add_output_subscriber(&mut self, client: kernel_output::Client) {
        self.output_subscribers.push(client);
    }

    /// Register an MCP elicitation callback (M3-D6).
    pub fn add_elicitation_subscriber(
        &mut self,
        client: crate::kaijutsu_capnp::elicitation_events::Client,
    ) {
        self.elicitation_subscribers.push(client);
    }

    /// Snapshot the current set of elicitation subscribers — caller can
    /// invoke `onRequest` on each. Returns an empty Vec when nobody has
    /// subscribed; in that case the broker should fall back to a
    /// "decline" response so MCP servers don't block forever.
    pub fn elicitation_subscribers(
        &self,
    ) -> Vec<crate::kaijutsu_capnp::elicitation_events::Client> {
        self.elicitation_subscribers.clone()
    }
}

impl Drop for ConnectionState {
    fn drop(&mut self) {
        // Wake any background tasks (FlowBus bridge, peer-invoke bridge)
        // that are awaiting on this token so they unwind their loops and
        // release their references. Without this, a wedged capnp callback
        // can pin those tasks indefinitely on the LocalSet.
        self.conn_cancel.cancel();
        // Un-know whatever this connection told us about the rig
        // (`docs/midi-next.md` "Presence is sink-fed"). Presence is
        // connection-bound for the case the sink cannot cover: a crash, a
        // yanked network, a `kill -9` — no `present=false` report is coming,
        // so a record that outlives its reporter is a lie the kernel would
        // keep telling forever. Records go back to *unknown* (removed), never
        // to a fabricated `present=false`: nobody observed an absence. Same
        // lifecycle the subscriptions above die on — this Drop is the one
        // teardown that runs even when `run_rpc`'s tail never does.
        if let Some(store) = &self.midi_presence {
            let reaped = store.reap_connection(self.session_id);
            if !reaped.is_empty() {
                log::info!(
                    "midi presence: connection {} closed — {} device(s) back to unknown: {}",
                    self.session_id.short(),
                    reaped.len(),
                    reaped.join(", ")
                );
            }
        }
        // Withdraw this connection's MIDI exchange sink. The bridge task
        // does this too on its way out, but only Drop is guaranteed to run
        // when the RPC system is torn down mid-flight — and a registered sink
        // nobody drains turns every exchange into a full-deadline wait
        // instead of an immediate, honest "that sink is gone".
        if let Some(registry) = &self.midi_exchange
            && registry.unregister(self.session_id)
        {
            log::info!(
                "midi exchange: connection {} closed — its sink no longer answers exchanges",
                self.session_id.short()
            );
        }
        // Clean up per-session context tracking. Mirrors the explicit
        // remove that used to live at the tail of `run_rpc`; the Drop
        // guard runs even when the RPC system future is dropped mid-flight
        // (e.g., from a wedge + thread teardown), so the map can't leak.
        self.session_contexts.remove(&self.session_id);
    }
}

/// Get the stable data directory for kernel persistent storage.
/// Creates the directory if it doesn't exist.
/// Returns: ~/.local/share/kaijutsu/kernel/
fn kernel_data_dir() -> std::path::PathBuf {
    let dir = kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernel");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Failed to create kernel data dir {:?}: {}", dir, e);
    }

    // Log a migration hint if old per-kernel directories exist
    let old_dir = kaish_kernel::xdg_data_home()
        .join("kaijutsu")
        .join("kernels");
    if old_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&old_dir)
    {
        let count = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .count();
        if count > 0 {
            log::warn!(
                "Found {} old kernel data dir(s) in {:?}. \
                     To recover, copy the most recent data.db to {:?}",
                count,
                old_dir,
                dir
            );
        }
    }

    dir
}

/// Create a BlockStore backed by the shared KernelDb.
fn create_block_store_with_kernel_db(
    db: Arc<parking_lot::Mutex<KernelDb>>,
    default_workspace_id: kaijutsu_types::WorkspaceId,
    principal_id: PrincipalId,
    block_flows: SharedBlockFlowBus,
    input_flows: SharedInputDocFlowBus,
) -> Result<SharedBlockStore, String> {
    let inner = BlockStore::with_db_and_flows(
        db,
        default_workspace_id,
        principal_id,
        block_flows,
        input_flows,
    );
    let store = Arc::new(inner);
    store.load_from_db().map_err(|e| {
        format!("Failed to load documents from DB (refusing to start with empty store): {e}")
    })?;
    log::info!("Loaded {} documents from database", store.len());
    if let Err(e) = store.load_input_docs_from_db() {
        log::warn!("Failed to load input docs from DB: {}", e);
    }
    Ok(store)
}

/// Get the config directory path.
/// Returns: ~/.config/kaijutsu/
/// Map a config RPC `path` (a bare name like `theme.toml`, or an already-full
/// `/etc/config/…` path) to its canonical path under the config mount. Config is
/// a flat namespace, so this is just a root prepend when the caller passed a
/// bare name.
fn config_canonical(path: &str) -> String {
    let trimmed = path.trim();
    // Already an absolute config-namespace path — the flat kernel-global
    // `/etc/config`, or the per-client `/etc/client` (hierarchical:
    // `<root>/<client-id>/<file>`, docs/config-crdt-ownership.md). Pass it
    // through so per-client config is reachable through the same config RPCs.
    // The boundary check (component-correct, not a bare prefix match) comes
    // from the shared `kaijutsu_types::paths` predicates.
    if paths::is_config_path(trimmed) || paths::is_client_path(trimmed) {
        trimmed.to_string()
    } else {
        // A bare name defaults to the kernel-global config mount.
        paths::config_path(trimmed.trim_start_matches('/'))
    }
}

/// One-time seed override for a `/etc/config/<name>` path: the body of a host
/// file under `config_dir`, if both the dir and file exist. Returns `None`
/// (→ caller uses the embedded default) when no dir is given or no host file is
/// present. This is a bootstrap source only — never read again after seeding.
fn config_seed_override(config_dir: Option<&Path>, canonical: &str) -> Option<String> {
    let dir = config_dir?;
    let name = canonical.strip_prefix(&format!("{}/", paths::CONFIG_ROOT))?;
    std::fs::read_to_string(dir.join(name)).ok()
}

/// Restore a config file to its embedded default by writing the seed body into
/// the CRDT through the VFS. Returns `(success, error_message)`. Errors loudly
/// (no silent no-op) when the path ships no embedded default.
async fn reset_config_to_embedded(kernel: &Arc<Kernel>, path: &str) -> (bool, String) {
    use kaijutsu_kernel::vfs::VfsOps;
    let canonical = config_canonical(path);
    let Some(body) = kaijutsu_kernel::config_seed::config_seed_body(&canonical) else {
        return (
            false,
            format!("no embedded default for config '{canonical}' (nothing to reset to)"),
        );
    };
    match kernel
        .vfs()
        .write_all(std::path::Path::new(&canonical), body.as_bytes())
        .await
    {
        Ok(()) => (true, String::new()),
        Err(e) => (false, format!("{e}")),
    }
}

/// Initialize a kernel's LLM registry from the kernel database.
///
/// Model configuration is SQL-native (`backends` / `backend_models` /
/// `llm_defaults` / `casts` / `cast_slots` / `model_aliases` /
/// `embedding_config`). There is no `models.toml`, no CRDT config doc, and no
/// host file in this path — the embedded floor lives in
/// `kaijutsu_kernel::seed_backends` and is applied at kernel construction.
///
/// Returns the embedding config if one is set and enabled.
///
/// A build failure here is loud and leaves the registry empty rather than
/// falling back to some other backend: quietly running a different model than
/// the operator configured is the exact silent-wrong-answer this project
/// refuses. Repair with `kj backend reseed`.
async fn initialize_kernel_models(
    kernel: &Arc<Kernel>,
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
) -> Option<kaijutsu_kernel::EmbeddingModelConfig> {
    let (registry, embedding) = {
        let db = kernel_db.lock();
        let registry = match kaijutsu_kernel::build_llm_registry(&db) {
            Ok(r) => r,
            Err(e) => {
                log::error!(
                    "Failed to build the LLM registry from the kernel DB: {e} \
                     — repair with `kj backend reseed`"
                );
                return None;
            }
        };
        let embedding = match kaijutsu_kernel::load_embedding_config(&db) {
            Ok(e) => e,
            Err(e) => {
                log::error!("Failed to read embedding config: {e}");
                None
            }
        };
        (registry, embedding)
    };
    *kernel.llm().write().await = registry;
    log::info!("Initialized kernel LLM registry from the kernel DB");
    embedding
}


#[derive(Clone)]
pub struct CommandEntry {
    pub id: u64,
    pub code: String,
    pub timestamp: u64,
}

// ============================================================================
// Execute Output Dispatch
// ============================================================================

/// Dispatch stdout/stderr/exitCode events to all registered output subscribers.
///
/// Sends up to 3 `KernelOutputEvent` messages per execution:
/// - stdout (if non-empty)
/// - stderr (if non-empty)
/// - exitCode (always — signals completion)
///
/// Removes subscribers whose RPC calls fail (capability revoked / client disconnected).
///
/// Each callback send is bounded by `OUTPUT_CALLBACK_TIMEOUT`; a stalled
/// peer can't pin the LocalSet — the subscriber is dropped and dispatch
/// continues. Same wedge defense as the FlowBus bridges.
async fn dispatch_output_events(
    exec_id: u64,
    result: &kaish_kernel::interpreter::ExecResult,
    connection: &Rc<RefCell<ConnectionState>>,
) {
    const OUTPUT_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // Clone subscribers out to avoid holding RefCell borrow across await points.
    let subscribers: Vec<kernel_output::Client> =
        { connection.borrow().output_subscribers.clone() };

    if subscribers.is_empty() {
        return;
    }

    let mut failed_indices = Vec::new();

    // Inline timeout: each callback gets the same bound. Abstracting the
    // capnp `Request<T, R>` generics into a helper hits Pipelined/Unpin/'static
    // bounds that aren't worth the noise here.
    macro_rules! send_callback {
        ($req:expr) => {
            match tokio::time::timeout(OUTPUT_CALLBACK_TIMEOUT, $req.send().promise).await {
                Ok(Ok(_)) => true,
                Ok(Err(_)) => false,
                Err(_) => {
                    log::warn!(
                        "output subscriber callback timed out after {:?} for \
                         exec_id={} — dropping",
                        OUTPUT_CALLBACK_TIMEOUT,
                        exec_id,
                    );
                    false
                }
            }
        };
    }

    for (i, subscriber) in subscribers.iter().enumerate() {
        let mut ok = true;

        // stdout
        let stdout = result.text_out();
        if ok && !stdout.is_empty() {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_stdout(&stdout);
            }
            ok = send_callback!(req);
        }

        // stderr
        if ok && !result.err.is_empty() {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_stderr(&result.err);
            }
            ok = send_callback!(req);
        }

        // exitCode (always — signals completion)
        if ok {
            let mut req = subscriber.on_output_request();
            {
                let mut event = req.get().init_event();
                event.set_exec_id(exec_id);
                event.init_event().set_exit_code(result.code as i32);
            }
            ok = send_callback!(req);
        }

        if !ok {
            failed_indices.push(i);
        }
    }

    // Remove failed subscribers (iterate in reverse to preserve indices).
    if !failed_indices.is_empty() {
        let mut conn = connection.borrow_mut();
        for &i in failed_indices.iter().rev() {
            if i < conn.output_subscribers.len() {
                conn.output_subscribers.swap_remove(i);
            }
        }
    }
}

// ============================================================================
// Semantic Index Integration
// ============================================================================

/// Adapter: SharedBlockStore → kaijutsu_index::BlockSource
pub(crate) struct BlockStoreSource(pub(crate) SharedBlockStore);

impl kaijutsu_index::BlockSource for BlockStoreSource {
    fn block_snapshots(
        &self,
        ctx: ContextId,
    ) -> Result<Vec<kaijutsu_types::BlockSnapshot>, String> {
        // Try in-memory first; if missing, hydrate from DB on demand.
        if !self.0.contains(ctx) {
            let _ = self.0.load_one_from_db(ctx);
        }
        BlockStore::block_snapshots(&self.0, ctx).map_err(|e| e.to_string())
    }
}

/// Adapter: FlowBus<BlockFlow> subscription → kaijutsu_index::StatusReceiver
struct FlowBusStatusReceiver {
    sub: kaijutsu_kernel::flows::Subscription<BlockFlow>,
}

impl kaijutsu_index::StatusReceiver for FlowBusStatusReceiver {
    fn recv(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<kaijutsu_index::StatusEvent>> + Send + '_>,
    > {
        Box::pin(async {
            loop {
                let msg = self.sub.recv().await?;
                if let BlockFlow::StatusChanged {
                    context_id, status, ..
                } = msg.payload
                    && status.is_terminal()
                {
                    return Some(kaijutsu_index::StatusEvent { context_id, status });
                }
            }
        })
    }
}

// ============================================================================
// Shared Kernel Creation
// ============================================================================

/// Start the external MCP servers configured in `mcp.toml` (kaibo, bevy_brp, …).
///
/// `ExternalMcpServer` (`mcp/servers/external.rs`) was a complete, tested
/// `McpServerLike` implementation with no caller from the moment the old
/// `mcp_pool`/`mcp_config` were deleted; this is that caller.
///
/// Deliberately separate from [`create_shared_kernel`] and called only from
/// the serving path (`SshServer::run_on_listener`). Reconciling here spawns
/// real host subprocesses, which must not be a side effect of merely
/// constructing a kernel — that is what every kernel-booting test does, and
/// folding it in made `cargo test` launch a real kaibo against its live
/// state DB.
///
/// Per-server failures are logged loudly and mirrored onto
/// `Broker::external_mcp_failures` (for `kj mcp list`) but never abort boot:
/// a misconfigured kaibo entry must not take the kernel down, while staying
/// silent would make an absent tool indistinguishable from one that was
/// never configured.
pub async fn start_external_mcp_servers(kernel: &Arc<kaijutsu_kernel::Kernel>) {
    let report = kaijutsu_kernel::mcp::reconcile_external_mcp_servers(kernel).await;
    if report.is_clean() {
        log::info!(
            "external MCP: {} server(s) registered from mcp.toml",
            report.added.len()
        );
    } else {
        log::error!(
            "external MCP: {} registered, {} failed, {} malformed entries — see \
             `kj mcp list` (failed: {:?}; warnings: {:?})",
            report.added.len(),
            report.failed.len(),
            report.warnings.len(),
            report.failed,
            report.warnings,
        );
    }
}

/// What happened when a BlockStore-discovered conversation was offered to
/// KernelDb at cold start (see [`bootstrap_discovered_context`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextBootstrapOutcome {
    /// KernelDb had no row for this conversation; a minimal placeholder row
    /// was created so the discovered blocks are reachable.
    Inserted,
    /// KernelDb already knew this context. The persisted row is authoritative
    /// and is left exactly as-is — a placeholder must never overwrite it.
    AlreadyKnown { archived: bool },
}

/// Offer one BlockStore-discovered Conversation document to KernelDb.
///
/// Cold start walks every `DocKind::Conversation` document in the BlockStore
/// and makes sure KernelDb has a context row for it, so blocks that outlived
/// their row are still reachable. "Already known" is decided against the WHOLE
/// `contexts` table, archived rows included: an archived context still owns its
/// conversation document, so it is *known*, not missing. Re-inserting it would
/// be a resurrection — a placeholder row (state Live, `created_at` = now,
/// `created_by` = system) overwriting a real archived one. Today the PK
/// constraint is what stops that; this function stops it a step earlier, and
/// the constraint stays as the backstop.
///
/// A UNIQUE violation from the INSERT is therefore never expected. If one
/// happens anyway (another writer won a race), it is re-classified by reading
/// the row back: the caller asked for something that is already true, which is
/// not a failed write. Any other error IS a failed write and bubbles.
fn bootstrap_discovered_context(
    db: &KernelDb,
    context_id: ContextId,
) -> Result<ContextBootstrapOutcome, KernelDbError> {
    if let Some(existing) = db.get_context(context_id)? {
        return Ok(ContextBootstrapOutcome::AlreadyKnown {
            archived: existing.archived_at.is_some(),
        });
    }

    let row = ContextRow {
        context_id,
        label: None,
        provider: None,
        model: None,
        system_prompt: None,
        consent_mode: kaijutsu_kernel::control::ConsentMode::Collaborative,
        context_state: kaijutsu_types::ContextState::Live,
        context_type: "default".to_string(),
        created_at: kaijutsu_types::now_millis() as i64,
        created_by: PrincipalId::system(),
        forked_from: None,
        fork_kind: None,
        archived_at: None,
        workspace_id: None,
        preset_id: None,
        concluded_at: None,
        last_activity_at: None,
        promoted_at: None,
        demoted_at: None,
        paused_at: None,
        cast_id: None,
    };
    // No `unwrap_or_else(WorkspaceId::new)` here: a fabricated id names no row
    // in `workspaces`, so the fallback only converts a legible workspace error
    // into an illegible FK violation on the next statement.
    let default_ws = db.get_or_create_default_workspace(row.created_by)?;
    match db.insert_context_with_document(&row, default_ws) {
        Ok(()) => Ok(ContextBootstrapOutcome::Inserted),
        Err(e) => match db.get_context(context_id)? {
            // Someone inserted between our read and our write. The row we
            // wanted exists, so the request is satisfied — not a failed write.
            // Unexpected at cold start (the KernelDb lock is held across the
            // whole walk), so say so rather than hiding it.
            Some(existing) => {
                log::warn!(
                    "context {} appeared in KernelDb during cold-start bootstrap \
                     (concurrent writer); keeping the persisted row",
                    context_id.short(),
                );
                Ok(ContextBootstrapOutcome::AlreadyKnown {
                    archived: existing.archived_at.is_some(),
                })
            }
            // Nothing to show for the write: a real persistence failure.
            None => Err(e),
        },
    }
}

#[cfg(test)]
mod context_bootstrap_tests {
    use super::*;

    fn db() -> KernelDb {
        KernelDb::in_memory().expect("in-memory KernelDb")
    }

    /// A context the way a real one looks on disk: a human label, a real
    /// principal, a non-default context_type, a birth timestamp in the past.
    /// Nothing a cold-start placeholder would ever reproduce — which is the
    /// point: if bootstrap ever overwrites this row, the difference shows.
    fn persist_real_context(db: &KernelDb, label: &str) -> (ContextId, ContextRow) {
        let context_id = ContextId::new();
        let row = ContextRow {
            context_id,
            label: Some(label.to_string()),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-8".into()),
            system_prompt: None,
            consent_mode: kaijutsu_kernel::control::ConsentMode::Collaborative,
            context_state: kaijutsu_types::ContextState::Live,
            context_type: "coder".to_string(),
            created_at: 1_700_000_000_000,
            created_by: PrincipalId::new(),
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            cast_id: None,
        };
        let ws = db
            .get_or_create_default_workspace(row.created_by)
            .expect("default workspace");
        db.insert_context_with_document(&row, ws)
            .expect("persist context");
        (context_id, row)
    }

    #[test]
    fn unknown_conversation_is_bootstrapped() {
        let db = db();
        let discovered = ContextId::new();

        assert_eq!(
            bootstrap_discovered_context(&db, discovered).expect("bootstrap must not fail"),
            ContextBootstrapOutcome::Inserted,
            "a conversation document with no KernelDb row is exactly what \
             bootstrap exists for",
        );

        let row = db
            .get_context(discovered)
            .expect("read back")
            .expect("row must exist after Inserted");
        assert_eq!(row.context_state, kaijutsu_types::ContextState::Live);
        assert_eq!(row.created_by, PrincipalId::system());

        // The workspace rides on the `documents` row (contexts.workspace_id
        // stays NULL for every path that passes `workspace_id: None`). It must
        // be the real default workspace, never a fabricated id that names no
        // row in `workspaces`.
        let doc = db
            .get_document(discovered)
            .expect("read back")
            .expect("bootstrap creates the documents row too");
        assert_eq!(
            doc.workspace_id,
            db.get_or_create_default_workspace(PrincipalId::system())
                .expect("default workspace"),
        );
    }

    #[test]
    fn live_context_is_already_known() {
        let db = db();
        let (id, _) = persist_real_context(&db, "live-one");

        assert_eq!(
            bootstrap_discovered_context(&db, id).expect("bootstrap must not fail"),
            ContextBootstrapOutcome::AlreadyKnown { archived: false },
        );
    }

    /// The live-caught bug: every restart re-offered the same archived
    /// contexts and logged `UNIQUE constraint failed: contexts.context_id`.
    /// An archived context still owns its conversation document, so the
    /// document walk finds it — but it is *known*, not missing.
    #[test]
    fn archived_context_is_known_not_missing() {
        let db = db();
        let (id, _) = persist_real_context(&db, "archived-one");
        assert!(db.archive_context(id).expect("archive"), "row archived");

        assert_eq!(
            bootstrap_discovered_context(&db, id).expect(
                "re-offering an archived context is not a failed write — the \
                 row it asks for already exists",
            ),
            ContextBootstrapOutcome::AlreadyKnown { archived: true },
        );
    }

    /// Guards the fix that must NOT be made: idempotent insert-or-replace.
    /// The placeholder row is strictly poorer than the persisted one, so
    /// "make the insert idempotent" would silently resurrect an archived
    /// context as Live and erase its label, owner, type, and birth time.
    #[test]
    fn bootstrap_never_overwrites_the_persisted_row() {
        let db = db();
        let (id, original) = persist_real_context(&db, "precious");
        db.archive_context(id).expect("archive");

        let _ = bootstrap_discovered_context(&db, id);

        let after = db.get_context(id).expect("read back").expect("row");
        assert!(
            after.archived_at.is_some(),
            "bootstrap must never resurrect an archived context",
        );
        assert_eq!(after.label, original.label, "label must survive bootstrap");
        assert_eq!(after.created_by, original.created_by);
        assert_eq!(after.created_at, original.created_at);
        assert_eq!(after.context_type, original.context_type);
        assert_eq!(after.model, original.model);
    }

    /// Cold start runs on every restart; the second restart must be as quiet
    /// as the first was loud.
    #[test]
    fn second_bootstrap_of_the_same_conversation_is_benign() {
        let db = db();
        let discovered = ContextId::new();

        assert_eq!(
            bootstrap_discovered_context(&db, discovered).unwrap(),
            ContextBootstrapOutcome::Inserted,
        );
        assert_eq!(
            bootstrap_discovered_context(&db, discovered).expect(
                "a restart re-walking the same conversation document is the \
                 normal case, not an error",
            ),
            ContextBootstrapOutcome::AlreadyKnown { archived: false },
        );
    }
}

/// Create the shared kernel at server startup.
///
/// This performs all kernel initialization: VFS mounts, block store with DB
/// persistence, default context, config backend, block tools, LLM, and MCP.
/// The returned `SharedKernel` is shared across all connections via `Arc`.
pub async fn create_shared_kernel(
    // One-time CRDT config seed source on a fresh kernel (see the /etc/config
    // mount below). NOT ongoing ownership: production passes None (embedded
    // defaults; the kernel never reads the user's host config). Tests point it
    // at a tempdir to inject config seeds.
    config_dir: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<SharedKernel, capnp::Error> {
    // Create shared FlowBus instances - shared between Kernel and BlockStore
    // Per-subscription lossless queue depth (KAIJUTSU_FLOW_QUEUE_DEPTH). The
    // real server's buses must match the kernel's own sizing, not a smaller
    // literal that would kick clients sooner here than anywhere else.
    let flow_depth = kaijutsu_kernel::flows::configured_queue_depth();
    let block_flows = shared_block_flow_bus(flow_depth);
    let input_flows = shared_input_doc_flow_bus(flow_depth);

    // Resolve stable data directory (used for block store DB, kernel DB, semantic index)
    let resolved_data_dir = match data_dir {
        Some(dir) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("Failed to create data dir {:?}: {}", dir, e);
            }
            dir.to_path_buf()
        }
        None => kernel_data_dir(),
    };

    // Open KernelDb. Fail loudly if the file is unopenable — silently
    // falling back to in-memory hides data loss: persisted contexts on disk
    // become invisible while the server pretends to be a fresh kernel.
    let db_path = resolved_data_dir.join("kernel.db");
    let kernel_db = KernelDb::open(&db_path).map_err(|e| {
        capnp::Error::failed(format!(
            "Failed to open KernelDb at {}: {}",
            db_path.display(),
            e
        ))
    })?;
    log::info!("Opened KernelDb at {}", db_path.display());

    // KernelId is the kernel's birth certificate — written once on first
    // DB open, never changes thereafter. Clients use this to detect that
    // the DB they're talking to is the one they bound to.
    let id = kernel_db.kernel_id().map_err(|e| {
        capnp::Error::failed(format!("Failed to read kernel identity: {}", e))
    })?;
    log::info!("Kernel ID: {}", id.to_hex());
    let id_str = id.to_hex();

    // Create the kaijutsu kernel with shared FlowBus
    let kernel =
        Kernel::with_flows(id, &id_str, block_flows.clone(), &resolved_data_dir).await;

    // Read-only root — whole system visible (ls /usr/bin, cargo, etc.)
    kernel.mount("/", LocalBackend::read_only("/")).await;

    // /dev is opaque to ambient sweeps (longest-prefix wins over `/`): still
    // directly browsable, but the FSN backdrop/world walk stops at the node
    // rather than descending. `/dev/fd` is a live view of the kernel
    // process's own open file descriptors on macOS/BSD — readdir + per-entry
    // lstat races fd churn into a stray EBADF that used to fail the whole
    // root snapshot (live-caught 2026-07-22, fsn view rendering empty).
    kernel
        .mount("/dev", LocalBackend::read_only("/dev").opaque(true))
        .await;

    // Read-write ~/src (longest-prefix wins over /)
    let home = kaish_kernel::home_dir();
    let src_dir = home.join("src");
    kernel
        .mount(
            &format!("{}", src_dir.display()),
            LocalBackend::new(&src_dir),
        )
        .await;

    // Read-write /tmp for scratch/interop with external tools.
    // NOTE: tests/vfs_activity_wire.rs depends on this exact mount (it
    // creates/writes under a tempdir in /tmp over the wire) — if you unmount
    // or narrow it, update that test's fixture alongside.
    kernel.mount("/tmp", LocalBackend::new("/tmp")).await;

    // The rc tree at /etc/rc is CRDT-owned (docs/config-crdt-ownership.md): no
    // host mount, no host-disk seeding. It is mounted below, once the block
    // store exists (the CRDT-native backend maps onto it) — and the mount table
    // is frozen there, after every mount is in place.

    // Wrap KernelDb in Arc<Mutex> and create auto-workspaces
    let kernel_db_arc = Arc::new(parking_lot::Mutex::new(kernel_db));
    let default_ws_id = {
        let mut db = kernel_db_arc.lock();
        let ws = db
            .get_or_create_default_workspace(PrincipalId::system())
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        // Seed the reserved factory fork presets (full/window/spawn) — the
        // floor pattern (insert only if absent). Fail loud on error rather than
        // ship a kernel where `kj fork --preset` has nothing to recall.
        kaijutsu_kernel::seed_presets::ensure_factory_presets(&mut db, PrincipalId::system())
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        // Seed the LLM configuration floor — backends, their known context
        // windows, the `--model` aliases, the defaults, and the embedding
        // model. Same absent-only floor pattern as the presets above, and the
        // successor to seeding `models.toml` into the CRDT. Fail loud: a
        // kernel with no backends hangs its first turn on "no provider
        // configured", which is a far worse diagnostic than this error.
        kaijutsu_kernel::seed_backends::ensure_factory_backends(&mut db, PrincipalId::system())
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        ws
    };

    // Create block store backed by unified KernelDb
    let block_flows_for_index = block_flows.clone();
    let documents = create_block_store_with_kernel_db(
        kernel_db_arc.clone(),
        default_ws_id,
        PrincipalId::system(),
        block_flows,
        input_flows,
    )
    .map_err(capnp::Error::failed)?;

    // Mount the CRDT-native rc backend at /etc/rc (longest-prefix wins over the
    // read-only `/`; the host's real /etc is never touched). The block store's
    // load_from_db has already replayed any persisted rc Config docs; seed from
    // the embedded defaults only when the rc namespace is still empty (a
    // genuinely fresh kernel). After that the CRDT owns the content: a script
    // you `rm`'d stays gone, a repo-dropped seed does not resurrect. Per-file
    // recovery is `kj rc reset <path>`. Seeding failure is fatal — a kernel
    // without its stance scripts must not come up pretending all is well.
    let rc_fs = kaijutsu_kernel::runtime::config_crdt_fs::ConfigCrdtFs::new(
        documents.clone(),
        paths::RC_ROOT,
    );
    if rc_fs.is_empty() {
        let n = rc_fs.seed_from_embedded().map_err(|e| {
            capnp::Error::failed(format!("rc seed into CRDT failed: {e}"))
        })?;
        log::info!("seeded {n} rc script(s) into the CRDT (fresh kernel)");
    }
    kernel.mount(paths::RC_ROOT, rc_fs).await;

    // Config files (theme/models/mcp.toml + system.md) at /etc/config are
    // CRDT-owned too (slice 2, docs/config-crdt-ownership.md): the SAME backend
    // type as rc, one rule, no host file. Seed the embedded defaults only when
    // the config namespace is still empty (a genuinely fresh kernel); after that
    // the CRDT owns the content. Per-file recovery is `kj config reset`. Seeding
    // failure is fatal — a kernel without a system prompt is useless.
    let config_fs = kaijutsu_kernel::runtime::config_crdt_fs::ConfigCrdtFs::new(
        documents.clone(),
        paths::CONFIG_ROOT,
    );
    if config_fs.is_empty() {
        // One-time bootstrap seed of a fresh config namespace. `config_dir`
        // (when provided) is a seed *source*, NOT ongoing ownership: for each
        // config file, a host file under that dir supplies the body if present,
        // else the embedded default. Production passes None → embedded only (the
        // hard-reset cutover: the kernel never reads the user's host config).
        // Model config is NOT here — it is SQL-native (`kj backend`).
        // After this, the CRDT is the sole owner — no host read/flush/reload.
        let seed: Vec<(String, String)> = kaijutsu_kernel::config_seed::config_seed_files()
            .into_iter()
            .map(|(canonical, embedded)| {
                let body = config_seed_override(config_dir, &canonical)
                    .unwrap_or_else(|| embedded.to_string());
                (canonical, body)
            })
            .collect();
        let n = config_fs
            .seed_entries(seed)
            .map_err(|e| capnp::Error::failed(format!("config seed into CRDT failed: {e}")))?;
        log::info!("seeded {n} config file(s) into the CRDT (fresh kernel)");
    }
    kernel.mount(paths::CONFIG_ROOT, config_fs).await;

    // Per-client config at /etc/client (docs/config-crdt-ownership.md
    // "Per-client config"): the SAME CRDT-native backend, one more mount. Seeded
    // with the shared *client* defaults (the metronome click today) at the mount
    // root; per-client overrides at /etc/client/<client-id>/… are written lazily
    // and never seeded (there is no client id at build time). Seed only on a
    // fresh (empty) namespace — new even on an already-seeded kernel, so a
    // restart brings the shared defaults up once, then the CRDT owns them.
    let client_fs = kaijutsu_kernel::runtime::config_crdt_fs::ConfigCrdtFs::new(
        documents.clone(),
        paths::CLIENT_ROOT,
    );
    if client_fs.is_empty() {
        let seed: Vec<(String, String)> = kaijutsu_kernel::config_seed::client_seed_files()
            .into_iter()
            .map(|(canonical, embedded)| (canonical, embedded.to_string()))
            .collect();
        let n = client_fs.seed_entries(seed).map_err(|e| {
            capnp::Error::failed(format!("client config seed into CRDT failed: {e}"))
        })?;
        log::info!("seeded {n} client config default(s) into the CRDT (fresh kernel)");
    }
    kernel.mount(paths::CLIENT_ROOT, client_fs).await;

    // MIDI device profiles at /etc/midi/devices/<name> are CRDT-owned too
    // (docs/midi-next.md "Storage and identity"): the SAME backend type as
    // rc/config/client, one more mount. Seeded from the embedded profiles
    // (`assets/defaults/midi/devices/*.md`, `kaijutsu_kernel::midi_seed`) only
    // when the namespace is still empty (a genuinely fresh kernel); after that
    // the CRDT owns the content. `kj midi list`/`show` read it.
    let midi_fs = kaijutsu_kernel::runtime::config_crdt_fs::ConfigCrdtFs::new(
        documents.clone(),
        paths::MIDI_ROOT,
    );
    if midi_fs.is_empty() {
        let n = midi_fs
            .seed_entries(kaijutsu_kernel::midi_seed::seed_files())
            .map_err(|e| capnp::Error::failed(format!("midi seed into CRDT failed: {e}")))?;
        log::info!("seeded {n} midi device profile(s) into the CRDT (fresh kernel)");
    }
    kernel.mount(paths::MIDI_ROOT, midi_fs).await;

    // The ephemeral other half of a device: sink-fed presence at
    // /run/midi/<device> (docs/midi-next.md "Presence is sink-fed"). Read-only
    // by construction over the kernel's in-memory presence store — the ONLY
    // writer is a `reportMidiPresence` call from a sink, so nothing that
    // reaches the VFS can invent a presence fact. Deliberately NOT seeded and
    // deliberately not CRDT-owned: a restarted kernel with no sinks connected
    // knows nothing, and saying so is the honest answer.
    let presence_fs =
        kaijutsu_kernel::midi_presence::MidiPresenceFs::new(kernel.midi_presence().clone());
    kernel.mount(paths::MIDI_RUN_ROOT, presence_fs).await;

    // Mount the CAS object pool read-only at /v/cas (docs/slash-v.md track B).
    // A CasFs over the kernel's FileStore renders every stored object as an
    // immutable file, sharded on the hash's leading two hex chars to match the
    // on-disk objects/ layout. This is the substrate for client CAS sync: once
    // mounted on the kernel MountTable, kaish, the file tools, and SFTP all
    // reach it for free (no new RPC). Read-only by construction — every write
    // returns EROFS from the backend, so it sidesteps the SFTP capability gate.
    let cas_fs = kaijutsu_kernel::vfs::CasFs::new(kernel.cas().clone());
    kernel.mount(paths::CAS_ROOT, cas_fs).await;

    // Mount the client-shares registry at /r (docs/slash-r.md, slice 1).
    // ShareFs routes internally to whichever clients happen to be live —
    // per-share mounts are impossible once the table freezes below, so this
    // is the ONE backend that has to exist for the whole `/r` namespace, rows
    // appearing/vanishing in its registry as `kaijutsu-share` sessions
    // connect/disconnect (`ssh.rs`'s `SSH_SHARE_SUBSYSTEM` arm). Opaque to
    // ambient sweeps by construction (`ShareFs::opaque_to_sweeps`) — the FSN
    // backdrop and any indexer skip it, never crawling a client's disk.
    let share_fs = kaijutsu_kernel::vfs::ShareFs::new(kernel.share_registry().clone());
    kernel.mount(paths::R_ROOT, share_fs).await;

    // Freeze the mount table — security perimeter is now fixed.
    // No more mount/unmount via RPC after this point.
    kernel.freeze_mounts();

    let kernel_arc = Arc::new(kernel);
    let workspace_guard = Some(kaijutsu_kernel::file_tools::WorkspaceGuard::new(
        kernel_db_arc.clone(),
    ));
    let session_contexts = kaijutsu_kernel::runtime::context_engine::session_context_map();

    // Phase 1 M4: register virtual MCP servers on the broker so
    // dispatch_tool_via_broker has something to route to. This runs
    // alongside the legacy registry until M5 deletes it.
    let file_cache_for_broker = Arc::new(kaijutsu_kernel::file_tools::FileDocumentCache::new(
        documents.clone(),
        kernel_arc.vfs().clone(),
    ));
    // Share this exact instance with the kaish MountBackend so both surfaces
    // map a real file to the same CRDT document.
    kernel_arc.set_file_cache(file_cache_for_broker.clone());

    if let Err(e) = kernel_arc
        .register_builtin_mcp_servers(
            documents.clone(),
            file_cache_for_broker,
            workspace_guard,
            kernel_db_arc.clone(),
        )
        .await
    {
        return Err(capnp::Error::failed(format!(
            "Failed to register builtin MCP servers on broker: {}",
            e
        )));
    }

    // Wire the kernel into the broker so HookBody::Kaish can construct
    // an EmbeddedKaish at fire time. Stored as Weak; setter must be called
    // after the kernel is wrapped in Arc (here) but before any kaish hook
    // can fire.
    kernel_arc.broker().set_kernel(&kernel_arc).await;

    // Recover contexts: KernelDb is the primary source, with BlockStore discovery as fallback.
    let all_contexts: Vec<ContextRow> = {
        let db = kernel_db_arc.lock();

        // Step 1: Discover Conversation documents and make sure KernelDb has a
        // row for each. Only conversations are contexts — code/config/text docs
        // are internal. `bootstrap_discovered_context` decides "already known"
        // against the WHOLE contexts table, archived rows included: an archived
        // context still owns its conversation document, and re-offering it is
        // not a missing row (that check used to run against the ACTIVE set,
        // which re-offered every archived context on every single restart).
        let block_store_ids: Vec<ContextId> =
            documents.list_ids_by_kind(kaijutsu_types::DocKind::Conversation);
        for &bs_ctx_id in &block_store_ids {
            match bootstrap_discovered_context(&db, bs_ctx_id) {
                Ok(ContextBootstrapOutcome::Inserted) => log::info!(
                    "Bootstrapped context {} into KernelDb from BlockStore",
                    bs_ctx_id.short()
                ),
                Ok(ContextBootstrapOutcome::AlreadyKnown { archived }) => log::debug!(
                    "Context {} already in KernelDb ({}) — nothing to bootstrap",
                    bs_ctx_id.short(),
                    if archived { "archived" } else { "live" },
                ),
                // A discovered conversation we could not persist would come up
                // invisible to every context surface while its blocks stay live
                // in the BlockStore — two stores disagreeing about what exists.
                // Same stance as the context read below: refuse to start rather
                // than warn and carry on with a store we know is divergent.
                Err(e) => {
                    return Err(capnp::Error::failed(format!(
                        "Failed to bootstrap context {} into KernelDb: {}",
                        bs_ctx_id.short(),
                        e
                    )));
                }
            }
        }

        // Step 2: Read the active contexts to recover (lock dropped after this
        // block). A failure here means we cannot know which contexts should be
        // recovered — refuse to start rather than silently coming up with zero.
        db.list_active_contexts().map_err(|e| {
            capnp::Error::failed(format!("Failed to read contexts from KernelDb: {}", e))
        })?
    }; // db lock dropped here — safe to await below

    // Register recovered contexts into DriftRouter
    if !all_contexts.is_empty() {
        let mut drift = kernel_arc.drift().write();
        for row in &all_contexts {
            if let Err(e) = drift.register(
                row.context_id,
                row.label.as_deref(),
                row.forked_from,
                row.created_by,
            ) {
                log::warn!(
                    "Skipping context {} recovery: {}",
                    row.context_id.short(),
                    e
                );
                continue;
            }
            if let (Some(provider), Some(model)) = (&row.provider, &row.model) {
                let _ = drift.configure_llm(row.context_id, provider, model);
            }
            // Re-claim the persisted lost+found sink so a later dead letter
            // reuses it instead of minting a duplicate (which would also
            // conflict on the reserved label).
            if row.label.as_deref() == Some("lost+found") {
                drift.adopt_lost_found(row.context_id);
            }
            log::info!(
                "Recovered context {} (label={:?}, provider={:?}) from KernelDb",
                row.context_id.short(),
                row.label,
                row.provider,
            );
        }
    }

    // Initialize LLM registry + embedding config from the kernel DB.
    let embedding_config = initialize_kernel_models(&kernel_arc, &kernel_db_arc).await;

    // External MCP servers (mcp.toml — kaibo, bevy_brp, …) are deliberately
    // NOT started here. Spawning real subprocesses is a property of
    // *serving*, not of *constructing a kernel*, and this function is what
    // every kernel-booting test calls. Starting them here made `cargo test`
    // spawn a real kaibo, which opens the shared state DB at
    // `~/.local/state/kaibo/state.db` — a test suite reaching into a sibling
    // project's live state, and a hard dependency on that project's build
    // artifact existing. `SshServer::run_on_listener` calls
    // [`start_external_mcp_servers`] immediately after this returns; a test
    // that wants the reconcile calls it explicitly
    // (`kaijutsu-server/tests/mcp_boot.rs`).

    // Initialize semantic index if embedding model is configured
    let semantic_index = if let Some(emb_config) = embedding_config {
        let index_config = kaijutsu_index::IndexConfig::new(
            emb_config.model_dir.clone(),
            emb_config.dimensions,
            emb_config.max_tokens,
            &resolved_data_dir,
        );
        match kaijutsu_index::RtenEmbedder::new(
            &emb_config.model_dir,
            emb_config.dimensions,
            emb_config.max_tokens,
        ) {
            Ok(embedder) => {
                match kaijutsu_index::SemanticIndex::new(index_config, Box::new(embedder)) {
                    Ok(idx) => {
                        let idx = Arc::new(idx);
                        // Spawn background watcher for re-indexing on block completion
                        let block_source = Arc::new(BlockStoreSource(documents.clone()));
                        let status_receiver = FlowBusStatusReceiver {
                            sub: block_flows_for_index.subscribe("block.status"),
                        };

                        // Build synthesis callback — runs Rhai after each indexing.
                        // Must spawn_blocking: Rhai eval + ONNX embed are CPU-bound.
                        let synth_idx = idx.clone();
                        let synth_blocks: Arc<dyn kaijutsu_index::BlockSource> =
                            Arc::new(BlockStoreSource(documents.clone()));
                        let on_indexed: kaijutsu_index::watcher::OnIndexed =
                            Arc::new(move |ctx_id| {
                                let idx_clone = synth_idx.clone();
                                let blocks_clone = synth_blocks.clone();
                                let handle = tokio::task::spawn_blocking(move || {
                                    kaijutsu_kernel::runtime::synthesis::run_synthesis_and_cache(
                                        ctx_id,
                                        idx_clone.embedder_arc(),
                                        blocks_clone,
                                        &idx_clone,
                                    );
                                });
                                // run_synthesis_and_cache logs its own errors,
                                // but a panic on the blocking thread (poisoned
                                // lock, OOM mid-embed) would otherwise vanish
                                // with the dropped JoinHandle.
                                tokio::spawn(async move {
                                    if let Err(e) = handle.await {
                                        log::error!(
                                            "synthesis task panicked for context {}: {e}",
                                            ctx_id.short()
                                        );
                                    }
                                });
                            });

                        kaijutsu_index::watcher::spawn_index_watcher(
                            idx.clone(),
                            block_source,
                            Box::new(status_receiver),
                            Some(on_indexed),
                        );
                        log::info!(
                            "Semantic index initialized with {}",
                            emb_config.model_dir.display()
                        );
                        Some(idx)
                    }
                    Err(e) => {
                        log::warn!("Semantic index unavailable: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("Embedding model unavailable: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Create kj dispatcher — shared across all connections
    let kj_dispatcher = Arc::new(kaijutsu_kernel::KjDispatcher::new(
        kernel_arc.drift().clone(),
        documents.clone(),
        kernel_db_arc.clone(),
        kernel_arc.clone(),
    ));
    // Stash a Weak<Self> on the dispatcher so internal paths (rc
    // lifecycle, kaish hook bodies) can construct KjBuiltin without
    // threading an Arc through every method.
    kj_dispatcher.set_self_arc();
    // Install the semantic index so in-kernel shell materialization
    // (the model's `shell` / `read_only_shell`) can pair it with a
    // block-backed source — the index is built here (it needs the ONNX
    // embedder) but consumed kernel-side. `None` when embeddings are off.
    kj_dispatcher.set_semantic_index(semantic_index.clone());
    // Wire the dispatcher into the broker so HookBody::Kaish can
    // register `kj` as a tool inside hook kaish sessions.
    kernel_arc
        .broker()
        .set_kj_dispatcher(&kj_dispatcher)
        .await;

    let shared = SharedKernelState {
        id,
        name: id_str,
        kernel: kernel_arc,
        documents,
        conversation_cache: Arc::new(ConversationCache::new(64)),
        kernel_db: kernel_db_arc,
        semantic_index,
        context_interrupts: Arc::new(TokioRwLock::new(HashMap::new())),
        interrupt_generation: AtomicU64::new(0),
        kj_dispatcher,
        session_contexts,
        subscription_registry: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        permission_subscribers: Arc::new(TokioRwLock::new(Vec::new())),
    };

    // Wire the permission-ask bridge (D-57) onto the broker so
    // `HookAction::Ask` has somewhere to send its round trip. Same
    // `permission_subscribers` Arc a `subscribePermissionEvents` RPC call
    // pushes into — see `PermissionAskBridge`.
    shared
        .kernel
        .broker()
        .set_permission_asker(Arc::new(PermissionAskBridge {
            subscribers: shared.permission_subscribers.clone(),
        }))
        .await;

    // ROOT bootstrap: a brand-new kernel (nothing recovered above) has no
    // contexts. Seed a single `director` context, `ROOT` — the binding-admin
    // root of the tree (admin + rc-write). ROOT deliberately *can't* drive LLM
    // turns (a director loadout has no drive/fork authority); the operator
    // creates a coder (or any other type) from it when a conversational context
    // is needed. Trigger is strictly *zero contexts at cold start*; once any
    // context exists this never fires. Fail-loud: a kernel that can't seed its
    // root context is broken, same stance as the recovery read above.
    if all_contexts.is_empty() {
        let root_id = ContextId::new();
        log::info!(
            "No contexts at cold start — seeding ROOT director context {}",
            root_id.short()
        );
        create_context_inner(
            &shared,
            root_id,
            "director",
            Some("ROOT"),
            PrincipalId::system(),
            None,
            SessionId::new(),
        )
        .await
        .map_err(|e| {
            capnp::Error::failed(format!("failed to seed ROOT context: {e}"))
        })?;
    }

    Ok(Arc::new(shared))
}

// ============================================================================
// World Implementation
// ============================================================================

/// World capability implementation
pub struct WorldImpl {
    registry: Arc<ServerRegistry>,
    connection: Rc<RefCell<ConnectionState>>,
}

impl WorldImpl {
    pub fn new(registry: Arc<ServerRegistry>, connection: Rc<RefCell<ConnectionState>>) -> Self {
        Self {
            registry,
            connection,
        }
    }
}

impl world::Server for WorldImpl {
    fn whoami(
        self: Rc<Self>,
        _params: world::WhoamiParams,
        mut results: world::WhoamiResults,
    ) -> Promise<(), capnp::Error> {
        let conn = self.connection.borrow();
        let mut identity = results.get().init_identity();
        identity.set_username(&conn.principal.username);
        identity.set_display_name(&conn.principal.display_name);
        // principalId @2 exists on the wire but was never populated — the
        // canonical principal-population gap. The server is authoritative for
        // the connection's identity, so stamp it from conn.principal here.
        identity.set_principal_id(conn.principal.id.as_bytes());
        Promise::ok(())
    }

    fn list_kernels(
        self: Rc<Self>,
        _params: world::ListKernelsParams,
        mut results: world::ListKernelsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel = &self.registry.kernel;
        let mut kernels = results.get().init_kernels(1);
        let mut k = kernels.reborrow().get(0);
        k.set_id(kernel.id.as_bytes());
        k.set_name(&kernel.name);
        k.set_user_count(1);
        k.set_agent_count(0);
        Promise::ok(())
    }

    fn bind_kernel(
        self: Rc<Self>,
        params: world::BindKernelParams,
        mut results: world::BindKernelResults,
    ) -> Promise<(), capnp::Error> {
        let _params_reader = pry!(params.get());
        let _span = tracing::info_span!("rpc", method = "bind_kernel").entered();

        // No kernel creation — hand out the shared kernel capability.
        let kernel = self.registry.kernel.clone();
        let kernel_impl = KernelImpl::new(
            kernel.clone(),
            self.connection.clone(),
        );
        results.get().set_kernel(capnp_rpc::new_client(kernel_impl));
        results.get().set_kernel_id(kernel.id.as_bytes());
        Promise::ok(())
    }
}

// ============================================================================
// Kernel Implementation
// ============================================================================

struct KernelImpl {
    kernel: SharedKernel,
    connection: Rc<RefCell<ConnectionState>>,
}

impl KernelImpl {
    fn new(
        kernel: SharedKernel,
        connection: Rc<RefCell<ConnectionState>>,
    ) -> Self {
        Self { kernel, connection }
    }
}

/// Fill a `ContextHandleInfo` builder directly from a durable `ContextRow` —
/// the DB-driven counterpart to `list_contexts`' per-row wire-writing above,
/// which instead starts from a live `ContextHandle` (registry) and only
/// *supplements* with `ContextRow` fields. Used by `resolve_context_label`,
/// where there is no registry entry to start from (that's the whole point —
/// see the capnp doc comment on `resolveContextLabel`). Deliberately leaves
/// registry/runtime-only fields (`traceId`, `liveStatus`, `trackId`,
/// synthesis keywords/preview, usage/background-process summaries) at their
/// wire defaults: honest absence, not fabricated from data this path never
/// had.
fn write_context_row_to_handle_info(
    mut info: context_handle_info::Builder<'_>,
    row: &ContextRow,
) {
    info.set_id(row.context_id.as_bytes());
    info.set_label(row.label.as_deref().unwrap_or(""));
    info.set_parent_id(
        row.forked_from
            .map(|p| *p.as_bytes())
            .unwrap_or([0u8; 16])
            .as_slice(),
    );
    info.set_provider(row.provider.as_deref().unwrap_or(""));
    info.set_model(row.model.as_deref().unwrap_or(""));
    info.set_created_at(row.created_at as u64);
    info.set_fork_kind(row.fork_kind.as_ref().map(|fk| fk.as_str()).unwrap_or(""));
    info.set_archived_at(row.archived_at.map(|ts| ts as u64).unwrap_or(0));
    info.set_context_state(row.context_state.as_str());
    info.set_context_type(&row.context_type);
    info.set_concluded_at(row.concluded_at.map(|ts| ts as u64).unwrap_or(0));
    info.set_last_activity_at(row.last_activity_at.map(|ts| ts as u64).unwrap_or(0));
    info.set_promoted_at(row.promoted_at.map(|ts| ts as u64).unwrap_or(0));
    info.set_demoted_at(row.demoted_at.map(|ts| ts as u64).unwrap_or(0));
    info.set_paused_at(row.paused_at.map(|ts| ts as u64).unwrap_or(0));
}

/// Verify a context can be joined, healing the DriftRouter registration from
/// the durable KernelDb row if the in-memory registry has lost it. Returns
/// the context's trace id (zeroed if truly unknown) on success.
///
/// The registry is populated by `createContext`/`registerFork`, AND
/// `create_shared_kernel`'s boot-time recovery step already re-registers
/// every context `KernelDb::list_active_contexts` returns (`WHERE
/// archived_at IS NULL`) on every kernel start — so a live or concluded
/// context surviving a restart is normally already registered by the time
/// anyone calls `joinContext` (verified directly:
/// `list_contexts_recovers_live_context_after_restart`,
/// `crates/kaijutsu-server/tests/context_label_resolve.rs`). The gap
/// recovery does NOT cover is an ARCHIVED context: excluded from
/// `list_active_contexts`, its registry entry does not survive a restart,
/// even though its KernelDb row and BlockStore document both do. Before this
/// fix, joining it after a restart was a hard failure here ("use
/// createContext first") despite the context plainly existing — this is the
/// narrow, real remainder of the divergence docs/issues.md originally
/// described more broadly ("MCP-created context invisible to `kj context
/// list` after kernel restart" — the live/concluded case in that report does
/// not reproduce against current code; see the docs update alongside this
/// change). Healing here — the one place every attach/reconnect funnels
/// through (the `joinContext` RPC handler, and via that RPC,
/// `register_session`'s attach path) — fixes it generally: the join succeeds
/// AND the now-re-registered context reappears in `listContexts` for
/// everyone else.
///
/// Pulled out of the `joinContext` RPC handler so it's exercisable directly
/// against a `SharedKernelState` in tests, without a capnp/SSH round trip —
/// see `join_context_heals_registry_for_an_archived_context_after_restart`,
/// same test file.
async fn ensure_context_joinable(
    kernel: &SharedKernelState,
    context_id: ContextId,
) -> Result<[u8; 16], capnp::Error> {
    // Context must already exist — no auto-creation.
    if !kernel.documents.contains(context_id) {
        return Err(capnp::Error::failed(format!(
            "context {} does not exist — use createContext first",
            context_id
        )));
    }

    log::debug!("Re-joining existing context {}", context_id);

    // Ensure input doc exists (idempotent)
    if let Err(e) = kernel.documents.create_input_doc(context_id) {
        log::warn!(
            "Failed to create input doc for context {}: {}",
            context_id,
            e
        );
    }

    let needs_heal = { kernel.kernel.drift().read().get(context_id).is_none() };
    if needs_heal {
        let row = {
            let db = kernel.kernel_db.lock();
            db.get_context(context_id).map_err(|e| {
                capnp::Error::failed(format!(
                    "join_context: KernelDb lookup for {context_id} failed: {e}"
                ))
            })?
        };
        let row = row.ok_or_else(|| {
            capnp::Error::failed(format!(
                "context {context_id} not registered in drift router and has \
                 no KernelDb row — use createContext first",
            ))
        })?;
        log::warn!(
            "join_context: {} missing from the in-memory registry (likely a \
             kernel restart) — healing from its KernelDb row: label={:?} \
             created_at={} last_activity_at={:?}",
            context_id,
            row.label,
            row.created_at,
            row.last_activity_at,
        );
        let mut drift = kernel.kernel.drift().write();
        drift
            .register(context_id, row.label.as_deref(), row.forked_from, row.created_by)
            .map_err(|e| {
                capnp::Error::failed(format!(
                    "join_context: failed to heal registry for {context_id}: {e}"
                ))
            })?;
        if let (Some(p), Some(m)) = (&row.provider, &row.model) {
            let _ = drift.configure_llm(context_id, p, m);
        }
    }

    let trace_id = {
        let drift = kernel.kernel.drift().read();
        drift.get(context_id).map(|h| h.trace_id).unwrap_or([0u8; 16])
    };
    Ok(trace_id)
}

/// The shared context-creation recipe, called by both the `createContext` RPC
/// and the cold-start genesis bootstrap (`create_shared_kernel`).
///
/// Does, in order: create the Conversation document + input doc, read LLM
/// defaults, write the KernelDb row (rolling back the document on failure),
/// register in the DriftRouter (rolling back the row + document on failure),
/// run the `create` rc lifecycle for `context_type` (failure is logged, not
/// fatal — it surfaces as Error blocks in the new context), and arm the beat
/// for musician contexts. Hard failures (document / DB / drift) return `Err`;
/// everything downstream is best-effort. Wire-result writing is the caller's
/// job — this never touches capnp results.
#[allow(clippy::too_many_arguments)]
async fn create_context_inner(
    state: &SharedKernelState,
    context_id: ContextId,
    context_type: &str,
    label: Option<&str>,
    created_by: PrincipalId,
    parent_ctx: Option<ContextId>,
    session_id: SessionId,
) -> Result<(), capnp::Error> {
    // Create the conversation document for this context.
    if let Err(e) =
        state
            .documents
            .create_document(context_id, kaijutsu_types::DocKind::Conversation, None)
    {
        return Err(capnp::Error::failed(format!(
            "Failed to create document for context {}: {}",
            context_id, e
        )));
    }

    // Create the input document for this context (non-fatal).
    if let Err(e) = state.documents.create_input_doc(context_id) {
        log::warn!(
            "Failed to create input doc for context {}: {}",
            context_id,
            e
        );
    }

    // Read LLM defaults so new contexts start with a model set. If no provider
    // is configured, leave both None so the user gets a clear error on use
    // rather than a silently-injected hardcoded model.
    let (default_provider, default_model) = {
        let registry = state.kernel.llm().read().await;
        let provider = registry.default_provider_name().map(|s| s.to_string());
        let model = registry.default_model().map(|s| s.to_string());
        if provider.is_none() && model.is_none() {
            log::warn!("No LLM provider configured — new context will have no model set");
        }
        (provider, model)
    };

    // Write-through: KernelDb first, then DriftRouter. Both must succeed or we
    // roll in-memory state back — never a ghost live-in-memory-but-missing-from-DB
    // context (lost on restart), nor a DB row without a drift entry.
    {
        let db = state.kernel_db.lock();
        let row = ContextRow {
            context_id,
            label: label.map(|s| s.to_string()),
            provider: default_provider.clone(),
            model: default_model.clone(),
            system_prompt: None,
            consent_mode: kaijutsu_kernel::control::ConsentMode::Collaborative,
            context_state: kaijutsu_types::ContextState::Live,
            context_type: context_type.to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by,
            forked_from: parent_ctx,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            cast_id: None,
        };
        // No `unwrap_or_else(WorkspaceId::new)` fallback: a fabricated id names
        // no row in `workspaces`, so it only turns a legible workspace error
        // into an FK violation reported as an insert failure one line later.
        let default_ws = match db.get_or_create_default_workspace(row.created_by) {
            Ok(ws) => ws,
            Err(e) => {
                drop(db);
                let _ = state.documents.delete_document(context_id);
                return Err(capnp::Error::failed(format!(
                    "Failed to resolve default workspace for context {}: {}",
                    context_id.short(),
                    e
                )));
            }
        };
        if let Err(e) = db.insert_context_with_document(&row, default_ws) {
            drop(db);
            let _ = state.documents.delete_document(context_id);
            return Err(capnp::Error::failed(format!(
                "KernelDb insert_context failed for {}: {}",
                context_id.short(),
                e
            )));
        }
    }

    {
        let mut drift = state.kernel.drift().write();
        if let Err(e) = drift.register(context_id, label, parent_ctx, created_by) {
            drop(drift);
            let _ = state.kernel_db.lock().delete_context(context_id);
            let _ = state.documents.delete_document(context_id);
            return Err(capnp::Error::failed(format!("label conflict: {e}")));
        }
        if let (Some(p), Some(m)) = (&default_provider, &default_model) {
            let _ = drift.configure_llm(context_id, p, m);
        }
        log::info!(
            "Created context {} (label={:?}) in kernel DriftRouter",
            context_id,
            label
        );
    }

    // Run rc create-lifecycle scripts for this context_type — the same hook
    // `kj context create` fires. Failures surface as Error blocks in the new
    // context; they don't abort creation.
    let rc_caller = kaijutsu_kernel::KjCaller {
        principal_id: created_by,
        context_id: Some(context_id),
        session_id,
        confirmed: false,
        rc_depth: 0,
        // The privileged binding-write path is the rc kaish
        // (materialize_context_kaish_rc), not this caller, so it stays unprivileged.
        privileged: false,
    };
    if let Err(e) = state
        .kj_dispatcher
        .run_rc_lifecycle("create", context_id, parent_ctx, None, None, &rc_caller)
        .await
    {
        log::warn!("rc create lifecycle for {}: {e}", context_id.short());
    }

    // The beat arm now lives in the musician's `create/` rc (run above via
    // run_rc_lifecycle), not a Rust `context_type == "musician"` branch here —
    // this used to duplicate the same arm logic the `kj context create` builtin
    // carried. A context_type is a beat participant exactly when its `create/` rc
    // calls `kj transport arm`, so new beat-bearing roles (funkMusician, …) need
    // no kernel edit. See `docs/chameleon.md`, "context_type is an rc bundle".

    Ok(())
}

impl kernel::Server for KernelImpl {
    fn get_info(
        self: Rc<Self>,
        params: kernel::GetInfoParams,
        mut results: kernel::GetInfoResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "get_info").entered();
        let mut info = results.get().init_info();
        info.set_id(self.kernel.id.as_bytes());
        info.set_name(&self.kernel.name);
        info.set_user_count(1);
        info.set_agent_count(0);
        Promise::ok(())
    }

    // kaish execution methods

    fn execute(
        self: Rc<Self>,
        params: kernel::ExecuteParams,
        mut results: kernel::ExecuteResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "execute");
        let code = pry!(pry!(p.get_code()).to_str()).to_owned();
        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        // Non-blocking execute: return exec_id immediately, spawn execution in background.

        Promise::from_future(
            async move {
                // Materialize a single-use context shell seeded from L1. One
                // instance per execute call — durable env + cwd persist in the
                // DB, transient scope dies with the instance.
                let started_ctx = connection.borrow().require_context()?;
                let kaish = materialize_context_shell(&kernel, &connection).await?;

                // Reject concurrent executions — kaish kernel is serial.
                {
                    let conn = connection.borrow();
                    if conn.has_running_execution() {
                        return Err(capnp::Error::failed("execution already in progress".into()));
                    }
                }

                // Allocate exec_id and register the execution before spawning.
                let (exec_id, cancel_token) = {
                    let mut conn = connection.borrow_mut();
                    let id = conn.next_exec_id();
                    let token = conn.register_execution(id);

                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("system clock before UNIX epoch")
                        .as_secs();

                    conn.command_history.push(CommandEntry {
                        id,
                        code: code.clone(),
                        timestamp,
                    });

                    (id, token)
                };

                // Return exec_id to the caller immediately.
                results.get().set_exec_id(exec_id);

                // Spawn background execution — Rc<EmbeddedKaish> is fine on LocalSet.
                let connection_bg = connection.clone();
                let kernel_db_for_persist = kernel.kernel_db.clone();
                tokio::task::spawn_local(async move {
                    // Yield so the RPC response is sent before we start executing.
                    tokio::task::yield_now().await;

                    // Snapshot the shell's durable surface (cwd + exported env)
                    // so we can persist what this command changes back to L1.
                    let state_before = snapshot_shell_state(&kaish).await;

                    let exec_result = tokio::select! {
                        result = kaish.execute_with_options(&code, kaish_kernel::ExecuteOptions::default()) => {
                            match result {
                                Ok(r) => r,
                                Err(e) => {
                                    log::error!("kaish execute error: {}", e);
                                    kaish_kernel::interpreter::ExecResult::failure(1, e.to_string())
                                }
                            }
                        }
                        _ = cancel_token.cancelled() => {
                            kaish.cancel();
                            kaish_kernel::interpreter::ExecResult::failure(130, "interrupted")
                        }
                    };

                    // Propagate any in-shell context switch (`kj context switch`
                    // / `kj fork`) back to the connection's shared map — the
                    // materialized shell's map is isolated, so without this the
                    // switch would be invisible to subsequent RPCs. Done *before*
                    // dispatching the exit-code event so the active context is
                    // settled by the time the client learns the command finished
                    // (otherwise a client firing its next RPC immediately could
                    // observe a stale active context).
                    // No switch: persist this command's cwd/export changes to
                    // the context it ran in. On a switch the snapshots straddle
                    // two contexts and the outgoing cwd is already saved inside
                    // kaish, so we skip the write-back.
                    if propagate_context_switch(&kaish, started_ctx, &connection_bg).is_none() {
                        let state_after = snapshot_shell_state(&kaish).await;
                        persist_shell_state(
                            &kernel_db_for_persist,
                            started_ctx,
                            &state_before,
                            &state_after,
                        );
                    }

                    // Dispatch output events to all subscribers.
                    dispatch_output_events(exec_id, &exec_result, &connection_bg).await;

                    // Clean up execution tracking.
                    connection_bg.borrow_mut().complete_execution(exec_id);
                });

                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn interrupt(
        self: Rc<Self>,
        params: kernel::InterruptParams,
        _results: kernel::InterruptResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "interrupt").entered();
        let exec_id = p.get_exec_id();

        let conn = self.connection.borrow();
        if let Some(running) = conn.running_executions.get(&exec_id) {
            log::info!("Interrupting execution {}", exec_id);
            running.cancel.cancel();
        }
        // Silently ignore unknown exec_ids (may have already completed).
        Promise::ok(())
    }

    fn complete(
        self: Rc<Self>,
        params: kernel::CompleteParams,
        mut results: kernel::CompleteResults,
    ) -> Promise<(), capnp::Error> {
        // Wire-compatible stub: the schema keeps `complete` reserved for
        // kaish completions but the kernel doesn't surface a completion API
        // yet. Returns an empty list so clients render nothing rather than
        // wiring up dead code paths. Trace is still attached so the round
        // trip shows up in OTel.
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "complete").entered();

        let mut builder = results.get().init_completions(0);
        let _ = builder.reborrow();
        Promise::ok(())
    }

    fn subscribe_output(
        self: Rc<Self>,
        params: kernel::SubscribeOutputParams,
        _results: kernel::SubscribeOutputResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_output").entered();
        let callback = pry!(pry!(params.get()).get_callback());
        self.connection.borrow_mut().add_output_subscriber(callback);
        log::debug!("Output subscriber registered");
        Promise::ok(())
    }

    fn get_command_history(
        self: Rc<Self>,
        params: kernel::GetCommandHistoryParams,
        mut results: kernel::GetCommandHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "get_command_history").entered();
        let limit = p.get_limit() as usize;

        let conn = self.connection.borrow();
        let entries: Vec<_> = conn.command_history.iter().rev().take(limit).collect();

        let mut result_entries = results.get().init_entries(entries.len() as u32);
        for (i, entry) in entries.iter().enumerate() {
            let mut e = result_entries.reborrow().get(i as u32);
            e.set_id(entry.id);
            e.set_code(&entry.code);
            e.set_timestamp(entry.timestamp);
        }
        Promise::ok(())
    }

    // VFS methods

    fn vfs(
        self: Rc<Self>,
        _params: kernel::VfsParams,
        mut results: kernel::VfsResults,
    ) -> Promise<(), capnp::Error> {
        let vfs_impl = VfsImpl::new(self.kernel.kernel.clone());
        results.get().set_vfs(capnp_rpc::new_client(vfs_impl));
        Promise::ok(())
    }

    fn list_mounts(
        self: Rc<Self>,
        _params: kernel::ListMountsParams,
        mut results: kernel::ListMountsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "list_mounts");
        Promise::from_future(
            async move {
                let mounts = kernel_arc.list_mounts().await;
                let mut builder = results.get().init_mounts(mounts.len() as u32);
                for (i, mount) in mounts.iter().enumerate() {
                    let mut m = builder.reborrow().get(i as u32);
                    m.set_path(mount.path.to_string_lossy());
                    m.set_read_only(mount.read_only);
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn mount(
        self: Rc<Self>,
        params: kernel::MountParams,
        _results: kernel::MountResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };
        let source = match params.get_source().and_then(|p| get_path_str(p)) {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };
        let writable = params.get_writable();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "mount");
        Promise::from_future(
            async move {
                // Expand source path (e.g., ~ to home dir)
                let expanded = shellexpand::tilde(&source);
                let source_path = std::path::PathBuf::from(expanded.as_ref());

                if !source_path.exists() {
                    return Err(capnp::Error::failed(format!(
                        "source path does not exist: {}",
                        source_path.display()
                    )));
                }

                let backend = if writable {
                    LocalBackend::new(source_path)
                } else {
                    LocalBackend::read_only(source_path)
                };

                if !kernel_arc.mount(&path, backend).await {
                    return Err(capnp::Error::failed(
                        "mount table is frozen — mounts cannot be changed at runtime".to_string(),
                    ));
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn unmount(
        self: Rc<Self>,
        params: kernel::UnmountParams,
        mut results: kernel::UnmountResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params
            .get()
            .and_then(|p| p.get_path())
            .and_then(|p| get_path_str(p))
        {
            Ok(s) => s,
            Err(e) => return Promise::err(e),
        };

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "unmount");
        Promise::from_future(
            async move {
                let success = kernel_arc.unmount(&path).await;
                results.get().set_success(success);
                Ok(())
            }
            .instrument(span),
        )
    }

    // Tool execution

    fn execute_tool(
        self: Rc<Self>,
        params: kernel::ExecuteToolParams,
        mut results: kernel::ExecuteToolResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "execute_tool");
        let call = pry!(p.get_call());
        let tool_name = pry!(pry!(call.get_tool()).to_str()).to_owned();
        let tool_params = pry!(pry!(call.get_params()).to_str()).to_owned();
        let request_id = pry!(pry!(call.get_request_id()).to_str()).to_owned();

        let kernel_arc = self.kernel.kernel.clone();
        let _kernel_id = self.kernel.id;

        // Extract identity and resolve cwd from the context's durable L1 state.
        let (principal_id, context_id, session_id) = {
            let conn = self.connection.borrow();
            (
                conn.principal.id,
                pry!(conn.require_context()),
                conn.session_id,
            )
        };
        let cwd = context_cwd(&self.kernel, context_id);

        Promise::from_future(
            async move {
                let mut result = results.get().init_result();
                result.set_request_id(&request_id);

                let tool_ctx = match cwd {
                    Some(cwd) => kaijutsu_kernel::ExecContext::new(
                        principal_id,
                        context_id,
                        cwd,
                        session_id,
                        kernel_arc.id(),
                    ),
                    None => kaijutsu_kernel::ExecContext::new_without_cwd(
                        principal_id,
                        context_id,
                        session_id,
                        kernel_arc.id(),
                    ),
                };

                // Phase 5 D-54: tool filter retired. Visibility is now
                // enforced by the broker's `ContextToolBinding` +
                // `McpHookPhase::ListTools` inside `list_visible_tools` and
                // `dispatch_tool_via_broker`.

                // Dispatch through the Phase 1 broker (M4).
                match kernel_arc
                    .dispatch_tool_via_broker(&tool_name, &tool_params, &tool_ctx)
                    .await
                {
                    Ok(exec_result) => {
                        result.set_success(exec_result.success);
                        result.set_output(&exec_result.stdout);
                        if !exec_result.stderr.is_empty() {
                            result.set_error(&exec_result.stderr);
                        }
                    }
                    Err(e) => {
                        result.set_success(false);
                        result.set_error(e.to_string());
                    }
                }
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn get_tool_schemas(
        self: Rc<Self>,
        params: kernel::GetToolSchemasParams,
        mut results: kernel::GetToolSchemasResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();
        let (principal_id, context_id) = {
            let conn = self.connection.borrow();
            // Fall back to a throwaway ContextId when the connection has no
            // active context — the broker will auto-populate a binding for
            // that ephemeral id.
            let ctx = conn.require_context().unwrap_or_else(|_| ContextId::new());
            (conn.principal.id, ctx)
        };

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_tool_schemas");
        Promise::from_future(
            async move {
                let visible = kernel_arc
                    .list_tool_defs_via_broker(context_id, principal_id)
                    .await
                    .map_err(|e| {
                        log::error!(
                            "get_tool_schemas: list_tool_defs_via_broker failed for context {context_id}: {e}"
                        );
                        capnp::Error::failed(format!("list_tool_defs_via_broker: {e}"))
                    })?;
                let mut builder = results.get().init_schemas(visible.len() as u32);
                for (i, (name, schema, description)) in visible.iter().enumerate() {
                    let mut s = builder.reborrow().get(i as u32);
                    s.set_name(name);
                    s.set_description(description.as_deref().unwrap_or(""));
                    s.set_category("mcp");
                    s.set_input_schema(schema.to_string());
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    // =========================================================================
    // Block-based CRDT operations
    // =========================================================================

    fn subscribe_blocks(
        self: Rc<Self>,
        params: kernel::SubscribeBlocksParams,
        _results: kernel::SubscribeBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_blocks").entered();
        let callback = pry!(pry!(params.get()).get_callback());

        // The same callback the FlowBus bridge pushes events at is the one the
        // kernel calls BACK for a MIDI exchange (`docs/midi-next.md` "SysEx:
        // the exchange pattern"). Registered here — where a client first
        // proves it is holding a sink callback — and keyed by this
        // connection's SessionId, the same key presence attribution uses.
        {
            let registry = self.kernel.kernel.midi_exchange().clone();
            let connection = {
                let mut conn = self.connection.borrow_mut();
                conn.attach_midi_exchange(registry.clone());
                conn.session_id
            };
            let conn_cancel = self.connection.borrow().cancel_token();
            spawn_midi_exchange_bridge(registry, connection, callback.clone(), conn_cancel);
        }

        {
            let block_flows = self.kernel.kernel.block_flows().clone();
            let input_flows = self.kernel.documents.input_flows().cloned();
            let kernel_id = self.kernel.id;
            // Connection-lifetime cancellation. Cleared on ConnectionState
            // Drop, so the bridge unwinds when the RPC system tears down (even
            // mid-callback). `disconnect` is the other direction: the bridge
            // fires it to take the whole connection down when this subscriber
            // falls behind, which is what makes the client reconnect and
            // resync.
            let (conn_cancel, disconnect, caps) = {
                let conn = self.connection.borrow();
                (
                    conn.cancel_token(),
                    conn.disconnect_token(),
                    conn.event_capabilities(),
                )
            };

            // Cap'n Proto callbacks are not Send, so spawn_local.
            tokio::task::spawn_local(async move {
                let block_sub = block_flows.subscribe("block.*");
                // Input flows are optional at this subscription site.
                let input_sub = input_flows.map(|f| f.subscribe("input.*"));
                run_block_bridge(
                    callback,
                    block_sub,
                    input_sub,
                    None,
                    caps,
                    kernel_id,
                    conn_cancel,
                    disconnect,
                    "FlowBus",
                )
                .await;
            });
        }
        Promise::ok(())
    }

    // ── In-app editor sessions (the vi/edit builtin; see docs/vi.md) ────────
    // Thin wire wrappers over the kernel's `editor_*` primitives. Session ids
    // are global; the path resolves the owning CRDT block. Edits mirror onto
    // that block — rc/config permission errors surface here loudly.

    fn editor_open(
        self: Rc<Self>,
        params: kernel::EditorOpenParams,
        mut results: kernel::EditorOpenResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "editor_open");
        let path = pry!(pry!(p.get_path()).to_str()).to_owned();
        let kernel = self.kernel.clone();
        Promise::from_future(
            async move {
                match kernel.kernel.editor_open(&path, &kernel.documents).await {
                    Ok((id, state)) => {
                        set_editor_state(results.get().init_state(), id.as_u64(), &state);
                        Ok(())
                    }
                    Err(e) => Err(capnp::Error::failed(format!("editor_open failed: {e}"))),
                }
            }
            .instrument(span),
        )
    }

    fn editor_keys(
        self: Rc<Self>,
        params: kernel::EditorKeysParams,
        mut results: kernel::EditorKeysResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "editor_keys");
        let session_id = p.get_session_id();
        let keys = pry!(pry!(p.get_keys()).to_str()).to_owned();
        let id = kaijutsu_kernel::editor::EditorSessionId::from_u64(session_id);
        let kernel = self.kernel.clone();
        // `editor_keys` is async now (a `:r` read awaits a VFS/kaish fetch).
        Promise::from_future(
            async move {
                match kernel.kernel.editor_keys(id, &keys, &kernel.documents).await {
                    Ok(state) => {
                        set_editor_state(results.get().init_state(), session_id, &state);
                        Ok(())
                    }
                    Err(e) => Err(capnp::Error::failed(format!("editor_keys failed: {e}"))),
                }
            }
            .instrument(span),
        )
    }

    fn editor_state(
        self: Rc<Self>,
        params: kernel::EditorStateParams,
        mut results: kernel::EditorStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _guard = extract_rpc_trace(p.get_trace(), "editor_state").entered();
        let session_id = p.get_session_id();
        let id = kaijutsu_kernel::editor::EditorSessionId::from_u64(session_id);
        match self.kernel.kernel.editor_state(id) {
            Ok(state) => {
                set_editor_state(results.get().init_state(), session_id, &state);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!("editor_state failed: {e}"))),
        }
    }

    fn editor_save(
        self: Rc<Self>,
        params: kernel::EditorSaveParams,
        mut results: kernel::EditorSaveResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _guard = extract_rpc_trace(p.get_trace(), "editor_save").entered();
        let session_id = p.get_session_id();
        let id = kaijutsu_kernel::editor::EditorSessionId::from_u64(session_id);
        match self.kernel.kernel.editor_save(id) {
            Ok(state) => {
                set_editor_state(results.get().init_state(), session_id, &state);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!("editor_save failed: {e}"))),
        }
    }

    fn editor_quit(
        self: Rc<Self>,
        params: kernel::EditorQuitParams,
        _results: kernel::EditorQuitResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _guard = extract_rpc_trace(p.get_trace(), "editor_quit").entered();
        let session_id = p.get_session_id();
        let id = kaijutsu_kernel::editor::EditorSessionId::from_u64(session_id);
        match self.kernel.kernel.editor_quit(id, &self.kernel.documents) {
            Ok(()) => Promise::ok(()),
            Err(e) => Promise::err(capnp::Error::failed(format!("editor_quit failed: {e}"))),
        }
    }

    /// Push channel: forward `EditorFlow` events to the subscriber's callback.
    /// Mirrors `subscribe_blocks` — a per-connection bridge task with the same
    /// cancellation + callback-timeout + health-reaping discipline.
    fn subscribe_editor(
        self: Rc<Self>,
        params: kernel::SubscribeEditorParams,
        _results: kernel::SubscribeEditorResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_editor").entered();
        let callback = pry!(pry!(params.get()).get_callback());
        {
            let editor_flows = self.kernel.kernel.editor_flows().clone();
            let kernel_id = self.kernel.id;
            let (conn_cancel, disconnect) = {
                let conn = self.connection.borrow();
                (conn.cancel_token(), conn.disconnect_token())
            };

            tokio::task::spawn_local(async move {
                let mut sub = editor_flows.subscribe("editor.*");
                let mut health = SubscriberHealth::new(SUBSCRIBER_FAILURE_STREAK_TIMEOUT);
                const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                log::debug!("Started editor subscription for kernel {}", kernel_id);

                loop {
                    let success = tokio::select! {
                        _ = conn_cancel.cancelled() => {
                            log::debug!("editor bridge cancelled with connection");
                            break;
                        }
                        Some(ev) = sub.recv_event() => {
                            let msg = match ev {
                                kaijutsu_kernel::flows::FlowRecv::Message(m) => m,
                                // Same doctrine as the block bridge: a
                                // subscriber that cannot keep up is cut off, not
                                // handed a stream with a hole in it. Editor state
                                // is a full snapshot per event, so a gap here
                                // means a renderer silently showing a stale
                                // buffer — exactly what we refuse to ship.
                                kaijutsu_kernel::flows::FlowRecv::Terminated(info) => {
                                    tracing::error!(
                                        kernel = %kernel_id,
                                        topic = info.topic,
                                        delivered = info.delivered,
                                        "editor subscriber fell behind — dropping the \
                                         connection so it resyncs"
                                    );
                                    disconnect.cancel();
                                    break;
                                }
                            };
                            match msg.payload {
                                EditorFlow::StateChanged { session_id, ref state } => {
                                    let mut req = callback.on_editor_state_request();
                                    set_editor_state(req.get().init_state(), session_id, state);
                                    await_editor_callback(req.send().promise, CALLBACK_TIMEOUT, kernel_id).await
                                }
                                EditorFlow::Closed { session_id } => {
                                    let mut req = callback.on_editor_closed_request();
                                    req.get().set_session_id(session_id);
                                    await_editor_callback(req.send().promise, CALLBACK_TIMEOUT, kernel_id).await
                                }
                            }
                        }
                        else => break,
                    };

                    if !health.record(success) {
                        log::warn!(
                            "editor bridge for kernel {} stopping: callback \
                             failures continuous for over {:?} — reaping subscriber",
                            kernel_id,
                            SUBSCRIBER_FAILURE_STREAK_TIMEOUT,
                        );
                        break;
                    }
                }
                log::debug!("editor bridge task for kernel {} ended", kernel_id);
            });
        }
        Promise::ok(())
    }

    /// Push channel: bridges the kernel's `TurnFlow` outcome bus onto the
    /// client's `TurnEvents` callback — the wire half of turn completion.
    ///
    /// Same shape as `subscribe_editor` (FlowBus subscribe → spawn_local bridge
    /// → per-callback timeout → health reaping → dies with the connection), and
    /// kernel-wide for the same reason: the event names its own context, so one
    /// subscription serves a client watching many.
    ///
    /// It subscribes to `turn.completed`/`turn.failed` and NOT `turn.*` on
    /// purpose. `turn.requested` is a request *to* this server's turn driver;
    /// forwarding it would tell a client about work it cannot do and cannot
    /// observe the result of any other way.
    fn subscribe_turn_events(
        self: Rc<Self>,
        params: kernel::SubscribeTurnEventsParams,
        _results: kernel::SubscribeTurnEventsResults,
    ) -> Promise<(), capnp::Error> {
        // Turn/LLM-adjacent spans are 100%-sampled (docs/telemetry.md), unlike
        // the 10% default for `rpc`: a turn is rare, expensive, and the thing we
        // most need a complete trace of.
        let _span = tracing::info_span!("rpc", method = "subscribe_turn_events").entered();
        let callback = pry!(pry!(params.get()).get_callback());
        {
            let turn_flows = self.kernel.kernel.turn_flows().clone();
            let kernel_id = self.kernel.id;
            let (conn_cancel, disconnect) = {
                let conn = self.connection.borrow();
                (conn.cancel_token(), conn.disconnect_token())
            };

            tokio::task::spawn_local(async move {
                // Two subscriptions, two independent queues. Turn outcomes are
                // rare and CRITICAL — a dropped `Completed` is what left an ACP
                // prompt promise unresolved forever on 2026-08-05 — and the
                // per-subscription queues mean bulk block/text pressure cannot
                // evict, delay, or terminate them: those ride a different bus
                // entirely.
                let mut completed = turn_flows.subscribe("turn.completed");
                let mut failed = turn_flows.subscribe("turn.failed");
                let mut health = SubscriberHealth::new(SUBSCRIBER_FAILURE_STREAK_TIMEOUT);
                const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                log::debug!("Started turn-events subscription for kernel {}", kernel_id);

                loop {
                    let success = tokio::select! {
                        _ = conn_cancel.cancelled() => {
                            log::debug!("turn-events bridge cancelled with connection");
                            break;
                        }
                        Some(ev) = completed.recv_event() => {
                            let msg = match ev {
                                kaijutsu_kernel::flows::FlowRecv::Message(m) => m,
                                kaijutsu_kernel::flows::FlowRecv::Terminated(info) => {
                                    tracing::error!(
                                        kernel = %kernel_id,
                                        topic = info.topic,
                                        delivered = info.delivered,
                                        "turn-events subscriber fell behind — dropping \
                                         the connection so it resyncs rather than \
                                         missing a turn outcome"
                                    );
                                    disconnect.cancel();
                                    break;
                                }
                            };
                            match msg.payload {
                                TurnFlow::Completed {
                                    context_id,
                                    principal_id,
                                    output_block_id,
                                    reason,
                                    origin,
                                } => {
                                    let span = tracing::info_span!(
                                        "turn.events_push",
                                        turn.stop_reason = reason.as_str(),
                                        turn.origin = origin.as_str(),
                                    );
                                    let _enter = span.enter();
                                    let mut req = callback.on_turn_completed_request();
                                    {
                                        let mut p = req.get();
                                        p.set_context_id(context_id.as_bytes());
                                        p.set_principal_id(principal_id.as_bytes());
                                        p.set_stop_reason(stop_reason_to_capnp(reason));
                                        p.set_origin(turn_origin_to_capnp(origin));
                                        // `hasOutputBlock` is the honest signal
                                        // for "the turn wrote no text" — the id
                                        // slot stays zeroed and the client is
                                        // told not to read it, rather than
                                        // shipping a plausible-looking nil id.
                                        p.set_has_output_block(output_block_id.is_some());
                                        if let Some(id) = output_block_id {
                                            let mut b = p.init_output_block_id();
                                            set_block_id_builder(&mut b, &id);
                                        }
                                    }
                                    drop(_enter);
                                    await_editor_callback(req.send().promise, CALLBACK_TIMEOUT, kernel_id).await
                                }
                                // A wildcard-free bus subscription still delivers
                                // only its own topic; anything else here is a bug
                                // in the bus, not something to paper over.
                                other => {
                                    log::error!(
                                        "turn.completed subscription delivered {}: bus routing bug",
                                        other.subject()
                                    );
                                    true
                                }
                            }
                        }
                        Some(ev) = failed.recv_event() => {
                            let msg = match ev {
                                kaijutsu_kernel::flows::FlowRecv::Message(m) => m,
                                kaijutsu_kernel::flows::FlowRecv::Terminated(info) => {
                                    tracing::error!(
                                        kernel = %kernel_id,
                                        topic = info.topic,
                                        delivered = info.delivered,
                                        "turn-events subscriber fell behind — dropping \
                                         the connection so it resyncs rather than \
                                         missing a turn outcome"
                                    );
                                    disconnect.cancel();
                                    break;
                                }
                            };
                            match msg.payload {
                                TurnFlow::Failed {
                                    context_id,
                                    principal_id,
                                    ref error,
                                    origin,
                                } => {
                                    let span = tracing::info_span!(
                                        "turn.events_push",
                                        turn.stop_reason = "failed",
                                        turn.origin = origin.as_str(),
                                    );
                                    let _enter = span.enter();
                                    let mut req = callback.on_turn_failed_request();
                                    {
                                        let mut p = req.get();
                                        p.set_context_id(context_id.as_bytes());
                                        p.set_principal_id(principal_id.as_bytes());
                                        p.set_error(error.as_str());
                                        p.set_origin(turn_origin_to_capnp(origin));
                                    }
                                    drop(_enter);
                                    await_editor_callback(req.send().promise, CALLBACK_TIMEOUT, kernel_id).await
                                }
                                ref other => {
                                    log::error!(
                                        "turn.failed subscription delivered {}: bus routing bug",
                                        other.subject()
                                    );
                                    true
                                }
                            }
                        }
                        else => break,
                    };

                    if !health.record(success) {
                        log::warn!(
                            "turn-events bridge for kernel {} stopping: callback \
                             failures continuous for over {:?} — reaping subscriber",
                            kernel_id,
                            SUBSCRIBER_FAILURE_STREAK_TIMEOUT,
                        );
                        break;
                    }
                }
                log::debug!("turn-events bridge task for kernel {} ended", kernel_id);
            });
        }
        Promise::ok(())
    }

    /// Push channel: server-ticked timer streams `VfsActivityEvents` digests
    /// to the subscriber (Lane K, FSN slice-1, `docs/scenes/vfs.md`). Unlike
    /// `subscribe_editor`/`subscribe_blocks` this is NOT a FlowBus bridge —
    /// there is no event to react to, only a per-connection interval timer
    /// calling `MountTable::activity_digest` (pure) and, on send success,
    /// `ActivityCursor::commit`. Same cancellation + callback-timeout +
    /// health-reaping discipline as the FlowBus bridges above, reusing
    /// `await_editor_callback`/`SubscriberHealth` (both generic over the
    /// callback's payload, despite the "editor" name on the former). No
    /// fat-stack thread: this never re-enters kaish/rc, just a timer tick and
    /// a capnp call.
    fn subscribe_vfs_activity(
        self: Rc<Self>,
        params: kernel::SubscribeVfsActivityParams,
        _results: kernel::SubscribeVfsActivityResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_vfs_activity").entered();
        let p = pry!(params.get());
        let callback = pry!(p.get_callback());
        // 0 requests the server default; anything below the floor is clamped
        // up rather than rejected — a client asking for a tighter interval
        // than we're willing to serve still gets a working subscription.
        let requested_ms = p.get_interval_ms();
        let interval_ms = if requested_ms == 0 {
            VFS_ACTIVITY_DEFAULT_INTERVAL_MS
        } else {
            requested_ms.max(VFS_ACTIVITY_MIN_INTERVAL_MS)
        };

        let vfs = self.kernel.kernel.vfs().clone();
        let kernel_id = self.kernel.id;
        let conn_cancel = self.connection.borrow().cancel_token();

        tokio::task::spawn_local(async move {
            let mut cursor = ActivityCursor::default();
            let mut health = SubscriberHealth::new(SUBSCRIBER_FAILURE_STREAK_TIMEOUT);
            const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms as u64));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            log::debug!(
                "Started vfs activity subscription for kernel {} (interval {}ms)",
                kernel_id,
                interval_ms
            );

            loop {
                tokio::select! {
                    _ = conn_cancel.cancelled() => {
                        log::debug!("vfs activity bridge cancelled with connection");
                        break;
                    }
                    _ = ticker.tick() => {
                        // Quiet tick: this subscriber's cursor is fully caught
                        // up — nothing new anywhere AND no stragglers pending
                        // from an earlier capped digest (a truncated commit
                        // holds the cursor's epoch back so the drain continues
                        // across quiet ticks; see ActivityCursor::commit). No
                        // callback, no health accounting — nothing to fail at.
                        let Some(digest) = vfs.activity_digest(&cursor, VFS_ACTIVITY_DIGEST_MAX_ENTRIES) else {
                            continue;
                        };

                        let mut req = callback.on_activity_digest_request();
                        {
                            let mut entries = req.get().init_entries(digest.entries.len() as u32);
                            for (i, (path, total)) in digest.entries.iter().enumerate() {
                                let mut entry = entries.reborrow().get(i as u32);
                                entry.set_path(path.to_string_lossy());
                                entry.set_total(*total);
                                entry.set_generation(vfs.generation_of(path));
                            }
                        }
                        req.get().set_global_total(digest.global_total);

                        let success = await_editor_callback(req.send().promise, CALLBACK_TIMEOUT, kernel_id).await;
                        // Commit ONLY on a successful send — a failed send
                        // must be recomputed identically next tick, never
                        // silently marked delivered (see activity.rs's
                        // two-phase diff/commit doc).
                        if success {
                            cursor.commit(&digest);
                        }

                        if !health.record(success) {
                            log::warn!(
                                "vfs activity bridge for kernel {} stopping: callback \
                                 failures continuous for over {:?} — reaping subscriber",
                                kernel_id,
                                SUBSCRIBER_FAILURE_STREAK_TIMEOUT,
                            );
                            break;
                        }
                    }
                }
            }
            log::debug!("vfs activity bridge task for kernel {} ended", kernel_id);
        });
        Promise::ok(())
    }

    fn get_context_state(
        self: Rc<Self>,
        _params: kernel::GetContextStateParams,
        _results: kernel::GetContextStateResults,
    ) -> Promise<(), capnp::Error> {
        // Tombstoned: use getBlocks @82 + getContextSync @83 instead.
        // Schema ordinal @14 is preserved for wire compatibility.
        Promise::err(capnp::Error::failed(
            "getContextState removed: use getBlocks @82 + getContextSync @83".into(),
        ))
    }

    // =========================================================================
    // LLM operations
    // =========================================================================

    fn prompt(
        self: Rc<Self>,
        params: kernel::PromptParams,
        mut results: kernel::PromptResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!("prompt() called for kernel {}", self.kernel.id);
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "prompt");
        let request = pry!(params.get_request());
        let content = pry!(pry!(request.get_content()).to_str()).to_owned();
        let context_id_bytes = pry!(request.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        log::info!(
            "Received prompt request: context_id={}, content_len={}",
            context_id,
            content.len()
        );
        // Note: Cap'n Proto defaults unset Text fields to "", so we filter empty strings
        let model = request
            .get_model()
            .ok()
            .and_then(|m| m.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());

        let kernel = self.kernel.clone();
        let (user_principal_id, session_id) = {
            let conn = self.connection.borrow();
            (conn.principal.id, conn.session_id)
        };

        Promise::from_future(
            async move {
                log::debug!("prompt future started for context_id={}", context_id);

                // Resolve cwd from the context's durable L1 state.
                let tool_ctx = match context_cwd(&kernel, context_id) {
                    Some(cwd) => kaijutsu_kernel::ExecContext::new(
                        user_principal_id,
                        context_id,
                        cwd,
                        session_id,
                        kernel.id,
                    ),
                    None => kaijutsu_kernel::ExecContext::new_without_cwd(
                        user_principal_id,
                        context_id,
                        session_id,
                        kernel.id,
                    ),
                };

                let documents = kernel.documents.clone();

                // Document must exist — join_context is the sole creator
                if documents.get(context_id).is_none() {
                    return Err(capnp::Error::failed(format!(
                        "context {} not found — call join_context first",
                        context_id
                    )));
                }

                // Create user message block at the end of the document
                let last_block = documents.last_block_id(context_id);
                log::info!(
                    "Inserting user block into context {}, after={:?}",
                    context_id,
                    last_block
                );
                let user_block_id = documents
                    .insert_block_as(
                        context_id,
                        None,
                        last_block.as_ref(),
                        Role::User,
                        BlockKind::Text,
                        &content,
                        Status::Done,
                        ContentType::Plain,
                        Some(user_principal_id),
                    )
                    .map_err(|e| {
                        log::error!("Failed to insert user block: {}", e);
                        capnp::Error::failed(format!("failed to insert user block: {}", e))
                    })?;
                log::debug!("Inserted user block: {:?}", user_block_id);

                // Generate prompt ID
                let prompt_id = uuid::Uuid::new_v4().to_string();
                log::debug!("Generated prompt_id={}", prompt_id);

                log::info!("User message block inserted, spawning LLM stream task");
                log::info!(
                    "Using model: {} (requested: {:?})",
                    model.as_deref().unwrap_or("default"),
                    model
                );

                // Spawn LLM streaming in background. An interactive human
                // prompt: it announces like every other turn, and the
                // `Interactive` origin is what keeps the musician's OODA Act
                // from crystallizing it (design §7) — the filter lives on the
                // consumer now, so a `subscribeTurnEvents` client still hears
                // this turn finish.
                spawn_llm_for_prompt(
                    &kernel,
                    context_id,
                    model.as_deref(),
                    &user_block_id,
                    tool_ctx,
                    user_principal_id,
                    TurnOrigin::Interactive,
                )
                .await?;

                // Return immediately with prompt_id - streaming happens in background
                results.get().set_prompt_id(&prompt_id);
                log::debug!(
                    "prompt() returning immediately with prompt_id={}",
                    prompt_id
                );
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    // =========================================================================
    // Context operations
    // =========================================================================

    fn list_contexts(
        self: Rc<Self>,
        params: kernel::ListContextsParams,
        mut results: kernel::ListContextsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();
        let kernel_db_arc = self.kernel.kernel_db.clone();
        let _kernel_id = self.kernel.id;
        let semantic_index = self.kernel.semantic_index.clone();
        let documents = self.kernel.documents.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "list_contexts");
        Promise::from_future(
            async move {
                // Build KernelDb lookup for fork_kind + archived_at (fields not on DriftRouter)
                let db_map: HashMap<ContextId, ContextRow> = {
                    let db = kernel_db_arc.lock();
                    match db.list_all_contexts() {
                        Ok(rows) => rows.into_iter().map(|r| (r.context_id, r)).collect(),
                        Err(_) => HashMap::new(),
                    }
                };
                // Context → track binding (one query for the whole kernel). The
                // attachments table is transactional on attach/detach, so —
                // unlike the playhead — the persisted rows ARE current. A
                // context attaches to at most one track (docs/tracks.md).
                let track_of: HashMap<ContextId, String> = {
                    let db = kernel_db_arc.lock();
                    match db.list_all_attachments() {
                        Ok(rows) => rows
                            .into_iter()
                            .map(|a| (a.context_id, a.track_id))
                            .collect(),
                        Err(_) => HashMap::new(),
                    }
                };

                // Cast id → label (Track D, 2026-08-03). Resolved ONCE for the
                // whole listing — never per-row — same convention as
                // `track_of`/`db_map` above. The wire ships the label, never a
                // bare CastId, so the app never needs its own cast lookup.
                let cast_labels: HashMap<kaijutsu_types::CastId, String> = {
                    let db = kernel_db_arc.lock();
                    match db.list_casts() {
                        Ok(casts) => casts.into_iter().map(|c| (c.cast_id, c.label)).collect(),
                        Err(_) => HashMap::new(),
                    }
                };

                // Context → last-call usage snapshot (one query for the whole
                // kernel, same shape as db_map/track_of above). Absent for a
                // context that has never completed an LLM call — honest
                // absence, not a fabricated zero (see `ContextUsageRow`).
                let usage_map: HashMap<ContextId, ContextUsageRow> = {
                    let db = kernel_db_arc.lock();
                    match db.list_all_context_usage() {
                        Ok(rows) => rows.into_iter().map(|r| (r.context_id, r)).collect(),
                        Err(_) => HashMap::new(),
                    }
                };
                // Context-window denominator, via the same
                // `LlmRegistry::context_window_for_live` `kj context info
                // --json` uses (config wins as an override; a live, cached
                // Anthropic lookup fills the honest gaps config never had) —
                // no second, possibly-diverging lookup. Resolved ONCE per
                // distinct (provider, model) pair, up front — not per row
                // inside the loop below — because that loop holds a
                // synchronous `RwLockReadGuard` (`drift.read()`, not the
                // async kind) across its body; awaiting a live network
                // lookup while holding that guard would block any writer
                // for the duration of the call (clippy `await_holding_lock`
                // caught this). A cache miss here awaits the network once
                // per unique model; every other row (and every later poll,
                // once the client's own cache is warm) is a synchronous hit.
                let window_by_model: HashMap<(String, String), Option<u64>> = {
                    let llm_registry = kernel_arc.llm().read().await;
                    let mut resolved = HashMap::new();
                    for u in usage_map.values() {
                        let key = (u.provider.clone(), u.model.clone());
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            resolved.entry(key)
                        {
                            let window = llm_registry
                                .context_window_for_live(&u.provider, &u.model)
                                .await;
                            e.insert(window);
                        }
                    }
                    resolved
                };

                // Background-process ambient state — one query for the whole
                // kernel (same batching convention as usage_map/track_of
                // above). A context absent from the map has no background
                // history at all; `resolve_background_wire_fields` treats
                // that identically to `BackgroundSummary::default()`.
                let bg_by_ctx = kernel_arc.background_processes().summary_by_context();

                // Read from the kernel's drift router — runtime authority for provider/model
                let drift = kernel_arc.drift().read();
                let contexts = drift.list_contexts();
                let mut ctx_list = results.get().init_contexts(contexts.len() as u32);

                for (i, ctx) in contexts.iter().enumerate() {
                    let mut c = ctx_list.reborrow().get(i as u32);
                    c.set_id(ctx.id.as_bytes());
                    c.set_label(ctx.label.as_deref().unwrap_or(""));
                    // Wire field is still named `parentId` — Rust side renamed to `forked_from`
                    c.set_parent_id(
                        ctx.forked_from
                            .map(|p| *p.as_bytes())
                            .unwrap_or([0u8; 16])
                            .as_slice(),
                    );
                    c.set_provider(ctx.provider.as_deref().unwrap_or(""));
                    c.set_model(ctx.model.as_deref().unwrap_or(""));
                    c.set_created_at(ctx.created_at);
                    c.set_trace_id(&ctx.trace_id);

                    c.set_context_state(ctx.state.as_str());

                    // Supplement with KernelDb metadata
                    if let Some(row) = db_map.get(&ctx.id) {
                        c.set_fork_kind(row.fork_kind.as_ref().map(|fk| fk.as_str()).unwrap_or(""));
                        c.set_archived_at(row.archived_at.map(|ts| ts as u64).unwrap_or(0));
                        c.set_concluded_at(row.concluded_at.map(|ts| ts as u64).unwrap_or(0));
                        c.set_context_type(&row.context_type);
                        // Stage 1 (time-well) kernel truth: last block-op
                        // recency, stamped by BlockStore::journal_op via
                        // KernelDb::touch_context_activity. 0 = never/unknown
                        // (client normalizes 0 -> None, same sentinel as
                        // concluded_at above).
                        c.set_last_activity_at(
                            row.last_activity_at.map(|ts| ts as u64).unwrap_or(0),
                        );
                        // Time-well ring placement (docs/timewell.md). 0 = unset,
                        // same sentinel convention as archived_at/concluded_at.
                        c.set_promoted_at(row.promoted_at.map(|ts| ts as u64).unwrap_or(0));
                        c.set_demoted_at(row.demoted_at.map(|ts| ts as u64).unwrap_or(0));
                        c.set_paused_at(row.paused_at.map(|ts| ts as u64).unwrap_or(0));
                        // Empty when uncast, or the cast_id is dangling (removed
                        // between the two reads above) — never an error.
                        c.set_cast_label(
                            row.cast_id
                                .and_then(|id| cast_labels.get(&id))
                                .map(String::as_str)
                                .unwrap_or(""),
                        );
                    }

                    // Context-window usage — the bottom-dock gauge's wire spine
                    // (kaijutsu.capnp ContextHandleInfo doc comment). No usage
                    // row = never completed an LLM call: honest sentinels,
                    // never fabricated. `resolve_usage_wire_fields` is unit
                    // tested directly (`context_usage_wire_tests` below) —
                    // sentinel selection lives there, not re-derived here.
                    let usage = usage_map.get(&ctx.id);
                    let window = usage.and_then(|u| {
                        window_by_model
                            .get(&(u.provider.clone(), u.model.clone()))
                            .copied()
                            .flatten()
                    });
                    let (wire_window, wire_used_tokens, wire_used_pct) =
                        resolve_usage_wire_fields(usage, window);
                    c.set_context_window(wire_window);
                    c.set_context_used_tokens(wire_used_tokens);
                    c.set_context_used_pct(wire_used_pct);

                    // Background-process ambient state — app visibility into
                    // `background_exec.rs` (kaijutsu.capnp ContextHandleInfo
                    // doc comment). `resolve_background_wire_fields` is unit
                    // tested directly (`background_wire_tests` below).
                    let bg_summary = bg_by_ctx.get(&ctx.id).cloned().unwrap_or_default();
                    let (bg_running, bg_oldest, bg_last_at, bg_last_status, bg_last_code) =
                        resolve_background_wire_fields(&bg_summary);
                    c.set_background_running_count(bg_running);
                    c.set_background_oldest_running_started_at(bg_oldest);
                    c.set_background_last_finished_at(bg_last_at);
                    c.set_background_last_finished_status(bg_last_status);
                    c.set_background_last_exit_code(bg_last_code);

                    // Supplement with synthesis data (keywords + preview)
                    if let Some(ref idx) = semantic_index
                        && let Some(synth) = idx.synthesis_cache().get_any(ctx.id)
                    {
                        let kw_strs: Vec<&str> =
                            synth.keywords.iter().map(|(k, _)| k.as_str()).collect();
                        let mut kw_list = c.reborrow().init_keywords(kw_strs.len() as u32);
                        for (j, kw) in kw_strs.iter().enumerate() {
                            kw_list.set(j as u32, kw);
                        }
                        // Prefer the sentence-level gist (extractive, scored
                        // across the top-K blocks) over the old 80-char
                        // block-head preview; same wire field either way — no
                        // schema change, just richer content for the well
                        // card face's gist line.
                        if let Some(gist) = synth.gist.as_deref().filter(|g| !g.is_empty()) {
                            c.set_top_block_preview(gist);
                        } else if let Some((_, _, preview)) = synth.top_blocks.first() {
                            c.set_top_block_preview(preview);
                        }
                    }

                    // Live activity for the time-well pulse (Running = working,
                    // Error = last turn failed). Stage 1 (time-well): now an
                    // O(1) cache read (`BlockStore::live_status`), bumped
                    // incrementally by `journal_op` on every mutating block
                    // op, instead of a per-poll full block scan + reduce.
                    let live = documents.live_status(ctx.id);
                    c.set_live_status(status_to_capnp(live));

                    // Track binding (empty = unattached; TrackIds are never empty).
                    if let Some(track_id) = track_of.get(&ctx.id) {
                        c.set_track_id(track_id);
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    /// Resolve a label straight against `KernelDb` — deliberately bypassing
    /// the DriftRouter that `listContexts` reads. See the capnp doc comment
    /// on `resolveContextLabel` for the full rationale (an indexed lookup
    /// vs. a registry scan, and the one real registry gap — archived
    /// contexts — this also happens to sidestep). `registerSession`'s
    /// upsert/attach decision (docs/issues.md, "register_session hard-fails
    /// on label conflict") needs this durable truth before it decides
    /// whether to attach, suffix, or create.
    fn resolve_context_label(
        self: Rc<Self>,
        params: kernel::ResolveContextLabelParams,
        mut results: kernel::ResolveContextLabelResults,
    ) -> Promise<(), capnp::Error> {
        let label = pry!(pry!(pry!(params.get()).get_label()).to_str()).to_owned();
        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "resolve_context_label");
        let kernel_db_arc = self.kernel.kernel_db.clone();
        Promise::from_future(
            async move {
                let row = {
                    let db = kernel_db_arc.lock();
                    db.find_context_by_label(&label).map_err(|e| {
                        capnp::Error::failed(format!(
                            "resolve_context_label: KernelDb lookup for '{label}' failed: {e}"
                        ))
                    })?
                };
                let mut res = results.get();
                match row {
                    Some(row) => {
                        res.set_found(true);
                        let info = res.init_info();
                        write_context_row_to_handle_info(info, &row);
                    }
                    None => res.set_found(false),
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    /// Create a new context with the given label.
    ///
    /// Generates a fresh ContextId (UUIDv7), creates the document in the
    /// block store, and registers it in the kernel's drift router.
    fn create_context(
        self: Rc<Self>,
        params: kernel::CreateContextParams,
        mut results: kernel::CreateContextResults,
    ) -> Promise<(), capnp::Error> {
        let label = pry!(pry!(pry!(params.get()).get_label()).to_str()).to_owned();
        // Empty context_type on the wire (old clients, plain `create_context`)
        // means "default" — the mode bundle whose rc create-scripts run below.
        let context_type_raw = pry!(pry!(pry!(params.get()).get_context_type()).to_str()).to_owned();
        let context_type = if context_type_raw.is_empty() {
            "default".to_string()
        } else {
            context_type_raw
        };

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        let session_id = connection.borrow().session_id;
        let parent_ctx = connection
            .borrow()
            .session_contexts
            .get(&session_id)
            .map(|r| *r);

        log::info!(
            "create_context: label='{}' kernel='{}'",
            label,
            kernel.id.to_hex()
        );

        Promise::from_future(async move {
            let context_id = ContextId::new();
            let created_by = connection.borrow().principal.id;
            let label_ref = if label.is_empty() {
                None
            } else {
                Some(label.as_str())
            };
            create_context_inner(
                &kernel,
                context_id,
                &context_type,
                label_ref,
                created_by,
                parent_ctx,
                session_id,
            )
            .await?;
            results.get().set_id(context_id.as_bytes());
            Ok(())
        })
    }

    /// Join an existing context, returning its context_id.
    ///
    /// The context must already exist (created via `createContext`). Returns an
    /// error if the context doesn't exist — no auto-creation.
    fn join_context(
        self: Rc<Self>,
        params: kernel::JoinContextParams,
        mut results: kernel::JoinContextResults,
    ) -> Promise<(), capnp::Error> {
        let params = pry!(params.get());
        // Schema: joinContext(contextId :Data, instance :Text) -> (contextId :Data)
        let context_id_bytes = pry!(params.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes).ok_or_else(|| capnp::Error::failed(
                "invalid context ID (expected 16 bytes)".into()
            ))
        );
        let instance = pry!(pry!(params.get_instance()).to_str()).to_owned();

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();

        log::info!(
            "join_context: context_id={} instance='{}' kernel='{}'",
            context_id,
            instance,
            kernel.id.to_hex()
        );

        let span = extract_rpc_trace(params.get_trace(), "join_context");
        Promise::from_future(
            async move {
                let trace_id = ensure_context_joinable(&kernel, context_id).await?;
                let _ctx_span =
                    kaijutsu_telemetry::context_root_span(&trace_id, "join_context").entered();

                // Update connection's active context in the global map
                let session_id = connection.borrow().session_id;
                connection
                    .borrow()
                    .session_contexts
                    .insert(session_id, context_id);

                // Return the context_id
                results.get().set_context_id(context_id.as_bytes());

                Ok(())
            }
            .instrument(span),
        )
    }

    // =========================================================================
    // MCP (legacy — removed in Phase 1 M5)
    // =========================================================================

    fn list_mcp_servers(
        self: Rc<Self>,
        _params: kernel::ListMcpServersParams,
        mut results: kernel::ListMcpServersResults,
    ) -> Promise<(), capnp::Error> {
        // Empty list — no servers registered until Phase 2.
        results.get().init_servers(0);
        Promise::ok(())
    }

    fn call_mcp_tool(
        self: Rc<Self>,
        params: kernel::CallMcpToolParams,
        mut results: kernel::CallMcpToolResults,
    ) -> Promise<(), capnp::Error> {
        // M6-G3: route MCP tool calls through the Phase 1 broker so the
        // SSH wire can exercise the same dispatch surface in-process and
        // remote tests use. Tool names are resolved via the calling
        // context's binding; a fresh binding seed is auto-populated by
        // dispatch_tool_via_broker on first touch.
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "call_mcp_tool").entered();
        let call = pry!(p.get_call());
        let tool_name = pry!(pry!(call.get_tool()).to_str()).to_owned();
        let arguments = pry!(pry!(call.get_arguments()).to_str()).to_owned();

        let connection = self.connection.clone();
        let kernel = self.kernel.clone();
        Promise::from_future(async move {
            let session_id = connection.borrow().session_id;
            let principal_id = connection.borrow().principal.id;
            let context_id = connection
                .borrow()
                .session_contexts
                .get(&session_id)
                .map(|r| *r)
                .ok_or_else(|| {
                    capnp::Error::failed(
                        "no context joined — call joinContext first".into(),
                    )
                })?;
            // Sibling call sites (`execute_tool`, `prompt`) resolve this from
            // the context's durable L1 state; this one used to hardcode `/`
            // unconditionally instead — worse than the others, since it
            // never even looked. Bring it in line: a real cwd if the context
            // has one, `None` (not a fabricated root) if it doesn't.
            let exec_ctx = match context_cwd(&kernel, context_id) {
                Some(cwd) => kaijutsu_kernel::ExecContext::new(
                    principal_id,
                    context_id,
                    cwd,
                    session_id,
                    kernel.kernel.id(),
                ),
                None => kaijutsu_kernel::ExecContext::new_without_cwd(
                    principal_id,
                    context_id,
                    session_id,
                    kernel.kernel.id(),
                ),
            };
            let exec = kernel
                .kernel
                .dispatch_tool_via_broker(&tool_name, &arguments, &exec_ctx)
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            let mut out = results.get().init_result();
            out.set_content(&exec.stdout);
            out.set_is_error(!exec.success);
            Ok(())
        })
    }

    // =========================================================================
    // Shell execution (kaish REPL)
    // =========================================================================

    fn shell_execute(
        self: Rc<Self>,
        params: kernel::ShellExecuteParams,
        mut results: kernel::ShellExecuteResults,
    ) -> Promise<(), capnp::Error> {
        log::debug!(
            "shell_execute() called for kernel {}",
            self.kernel.id.to_hex()
        );
        let params = pry!(params.get());
        let trace_span = extract_rpc_trace(params.get_trace(), "shell_execute");
        let code = pry!(pry!(params.get_code()).to_str()).to_owned();
        let context_id_bytes = pry!(params.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let user_initiated = params.get_user_initiated();
        log::info!(
            "Shell execute: context_id={}, code={}, user_initiated={}",
            context_id,
            code,
            user_initiated
        );

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();
        let user_principal_id = self.connection.borrow().principal.id;

        Promise::from_future(
            async move {
                // Shared facade gate (deny-by-default): humans (app) and agents
                // (MCP) both reach shell execution through this RPC, so the
                // allow-set is enforced here for everyone, keyed on the context
                // binding — not on which client called.
                kernel
                    .kernel
                    .broker()
                    .check_facade(&context_id, "shell")
                    .await
                    .map_err(|e| capnp::Error::failed(format!("shell denied: {e}")))?;

                let command_block_id = execute_shell_command(
                    &code,
                    context_id,
                    user_principal_id,
                    user_initiated,
                    &kernel,
                    &connection,
                )
                .await?;

                let mut block_id_builder = results.get().init_command_block_id();
                set_block_id_builder(&mut block_id_builder, &command_block_id);
                Ok(())
            }
            .instrument(trace_span),
        )
    }

    // =========================================================================
    // Shell state (cwd, last result)
    // =========================================================================

    fn get_cwd(
        self: Rc<Self>,
        _params: kernel::GetCwdParams,
        mut results: kernel::GetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let connection = self.connection.clone();
        let kernel = self.kernel.clone();

        let span = tracing::info_span!("rpc", method = "get_cwd");
        Promise::from_future(
            async move {
                // cwd is durable context state (L1), shared across the context's
                // lifetime; default to the VFS landing dir when unset.
                let context_id = connection.borrow().require_context()?;
                let cwd = context_cwd(&kernel, context_id)
                    .unwrap_or_else(|| std::path::PathBuf::from("/docs"));

                results.get().set_path(cwd.to_string_lossy());
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_cwd(
        self: Rc<Self>,
        params: kernel::SetCwdParams,
        mut results: kernel::SetCwdResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Promise::err(capnp::Error::failed(format!("invalid path: {}", e)));
                }
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing path: {}", e))),
        };

        let connection = self.connection.clone();
        let kernel = self.kernel.clone();

        let span = tracing::info_span!("rpc", method = "set_cwd");
        Promise::from_future(
            async move {
                // cwd is durable, context-scoped state: write it straight to L1
                // (`context_shell.cwd`). Every materialized shell for this
                // context — model, interactive, headless — seeds from here.
                let context_id = connection.borrow().require_context()?;

                // Validate the path against the context's shell backend (the
                // namespace `cd` uses) before persisting — fail fast rather than
                // store a cwd that every later materialized shell would reject
                // on restore. A throwaway shell is enough to reach the backend.
                let kaish = materialize_context_shell(&kernel, &connection).await?;
                if !kaish.try_set_cwd(std::path::PathBuf::from(&path)).await {
                    results.get().set_success(false);
                    results
                        .get()
                        .set_error(format!("not a directory: {}", path));
                    return Ok(());
                }

                let updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before UNIX epoch")
                    .as_millis() as i64;
                kernel
                    .kernel_db
                    .lock()
                    .upsert_context_shell(&ContextShellRow {
                        context_id,
                        cwd: Some(path.clone()),
                        updated_at,
                    })
                    .map_err(|e| {
                        capnp::Error::failed(format!("failed to persist cwd: {}", e))
                    })?;
                results.get().set_success(true);
                results.get().set_error("");
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_last_result(
        self: Rc<Self>,
        _params: kernel::GetLastResultParams,
        mut results: kernel::GetLastResultResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "get_last_result").entered();

        // `$?` was read off the per-connection persistent kaish. Under the
        // per-use materialization model there is no persistent shell to hold a
        // "last exit code" — each command runs in a throwaway instance — so this
        // always returns the empty/zero result. The wire method stays for
        // compatibility; shell exit codes now live on the ToolResult block
        // (`set_exit_code` in `execute_shell_command`), which is the durable,
        // multi-writer-safe home for them.
        let mut result_builder = results.get().init_result();
        result_builder.set_code(0);
        result_builder.set_ok(true);
        result_builder.set_stdout(&[]);
        result_builder.set_stderr("");
        Promise::ok(())
    }

    fn push_ops(
        self: Rc<Self>,
        params: kernel::PushOpsParams,
        mut results: kernel::PushOpsResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let _trace_guard = extract_rpc_trace(params_reader.get_trace(), "push_ops").entered();
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let ops_data = pry!(params_reader.get_ops()).to_vec();

        log::debug!(
            "push_ops called for context {} with {} bytes",
            context_id,
            ops_data.len()
        );

        let _ctx_span = if let Some(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "push_ops").entered())
        } else {
            None
        };

        let documents = &self.kernel.documents;

        // Deserialize the sync payload
        let payload: kaijutsu_crdt::block_store::SyncPayload =
            match kaijutsu_types::codec::decode(&ops_data)
        {
            Ok(p) => p,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!(
                    "failed to deserialize sync payload: {}",
                    e
                )));
            }
        };

        // Merge the sync payload into the document
        let ack_version = match documents.merge_ops(context_id, payload) {
            Ok(version) => version,
            Err(e) => {
                return Promise::err(capnp::Error::failed(format!("failed to merge ops: {}", e)));
            }
        };

        log::debug!("push_ops merged successfully, new version: {}", ack_version);
        results.get().set_ack_version(ack_version);
        Promise::ok(())
    }

    // =========================================================================
    // MCP Resource Operations (legacy — removed in Phase 1 M5)
    // =========================================================================

    fn list_mcp_resources(
        self: Rc<Self>,
        _params: kernel::ListMcpResourcesParams,
        mut results: kernel::ListMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        results.get().init_resources(0);
        Promise::ok(())
    }

    fn subscribe_mcp_resources(
        self: Rc<Self>,
        params: kernel::SubscribeMcpResourcesParams,
        _results: kernel::SubscribeMcpResourcesResults,
    ) -> Promise<(), capnp::Error> {
        // Accept the `instance` param for wire compatibility with the new
        // client. No-op handler today (resource events not wired); when this
        // gains a real bridge task it should plug into
        // `subscription_registry` like `subscribe_blocks_filtered` does.
        let _p = pry!(params.get());
        Promise::ok(())
    }

    fn subscribe_mcp_elicitations(
        self: Rc<Self>,
        params: kernel::SubscribeMcpElicitationsParams,
        _results: kernel::SubscribeMcpElicitationsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let callback = pry!(p.get_callback());
        // `instance` is read for wire compatibility. Elicitation subscribers
        // are stored in per-connection state, which is dropped when the
        // connection tears down — no cross-connection dedupe needed.
        let _instance = pry!(pry!(p.get_instance()).to_str());
        self.connection
            .borrow_mut()
            .add_elicitation_subscriber(callback);
        Promise::ok(())
    }

    /// Register a `PermissionEvents` callback (D-57, docs/acp.md gap #2).
    ///
    /// Kernel-wide, unlike `subscribe_mcp_elicitations` above — but the
    /// `callback` capability itself is `!Send` (an `Rc`-based capnp hook),
    /// while `SharedKernelState::permission_subscribers` must be `Send +
    /// Sync` (a connection can run on its own dedicated thread). So this
    /// spawns a `spawn_local` bridge task, exactly like `subscribe_editor`/
    /// `subscribe_turn_events`, that owns `callback` on THIS connection's
    /// `LocalSet` and drains an `mpsc` channel of asks; only the channel's
    /// `Sender` — which is `Send`-safe — goes into the shared registry.
    /// `PermissionAskBridge::ask` (any thread) sends into that channel and
    /// awaits the paired oneshot reply.
    ///
    /// Dies with the connection (`conn_cancel`); no explicit unsubscribe
    /// RPC. A dead entry (channel closed) is pruned reactively by the
    /// bridge on its next failed send — see `PermissionAskBridge`'s doc
    /// comment.
    /// Record what this connection's push subscriptions can handle.
    ///
    /// Explicit opt-in rather than probing: a client that never calls this
    /// keeps receiving exactly the event shapes it already understands, so the
    /// app binary on moltar works unchanged against a kernel that gained
    /// coalescing. Call it before subscribing — a bridge reads the flags once,
    /// when it starts.
    fn declare_event_capabilities(
        self: Rc<Self>,
        params: kernel::DeclareEventCapabilitiesParams,
        _results: kernel::DeclareEventCapabilitiesResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let caps = EventCapabilities {
            text_ops_batch: p.get_text_ops_batch(),
            subscription_terminated: p.get_subscription_terminated(),
        };
        log::debug!(
            "client declared event capabilities: batch={} kick={}",
            caps.text_ops_batch,
            caps.subscription_terminated,
        );
        self.connection.borrow_mut().set_event_capabilities(caps);
        Promise::ok(())
    }

    fn subscribe_permission_events(
        self: Rc<Self>,
        params: kernel::SubscribePermissionEventsParams,
        _results: kernel::SubscribePermissionEventsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let callback = pry!(p.get_callback());
        let subscribers = self.kernel.permission_subscribers.clone();
        let conn_cancel = self.connection.borrow().cancel_token();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<PermissionAskChannelMsg>(16);
        tokio::task::spawn_local(async move {
            loop {
                let msg = tokio::select! {
                    _ = conn_cancel.cancelled() => {
                        log::debug!("permission-ask bridge cancelled with connection");
                        break;
                    }
                    m = rx.recv() => match m {
                        Some(m) => m,
                        None => break,
                    },
                };
                let mut req = callback.on_ask_request();
                {
                    let mut p = req.get().init_request();
                    p.set_request_id(&msg.request.request_id);
                    p.set_context_id(msg.request.context_id.as_bytes());
                    p.set_description(&msg.request.description);
                    p.set_instance(&msg.request.instance);
                    p.set_tool(&msg.request.tool);
                    p.set_hook_id(&msg.request.hook_id);
                    let mut options = p
                        .reborrow()
                        .init_options(msg.request.options.len() as u32);
                    for (i, opt) in msg.request.options.iter().enumerate() {
                        let mut o = options.reborrow().get(i as u32);
                        o.set_id(&opt.id);
                        o.set_label(&opt.label);
                        o.set_kind(&opt.kind);
                    }
                }
                let outcome = match req.send().promise.await {
                    Ok(reply) => match reply.get().and_then(|r| r.get_response()) {
                        Ok(response) => {
                            let allow = response.get_allow();
                            let selected_option_id = if response.get_has_selected_option_id() {
                                match response.get_selected_option_id() {
                                    Ok(t) => match t.to_str() {
                                        Ok(s) => Some(s.to_string()),
                                        Err(e) => {
                                            log::warn!(
                                                "permission ask selectedOptionId not valid UTF-8: {e}; dropping it"
                                            );
                                            None
                                        }
                                    },
                                    Err(e) => {
                                        log::warn!("permission ask selectedOptionId unreadable: {e}; dropping it");
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            let remember = if response.get_has_remember() {
                                match response.get_remember() {
                                    Ok(t) => match t.to_str() {
                                        Ok(s) => Some(s.to_string()),
                                        Err(e) => {
                                            log::warn!(
                                                "permission ask remember not valid UTF-8: {e}; dropping it"
                                            );
                                            None
                                        }
                                    },
                                    Err(e) => {
                                        log::warn!("permission ask remember unreadable: {e}; dropping it");
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            PermissionOutcome::Answered(PermissionAnswer {
                                allow,
                                selected_option_id,
                                remember,
                            })
                        }
                        Err(e) => {
                            log::warn!("permission ask response malformed: {e}");
                            PermissionOutcome::NoSubscriber
                        }
                    },
                    Err(e) => {
                        log::warn!(
                            "permission ask subscriber callback failed (request_id={}): {e}",
                            msg.request.request_id,
                        );
                        PermissionOutcome::NoSubscriber
                    }
                };
                // Best-effort: the asker may have already timed out and
                // dropped its receiver, which is not a bridge-task error.
                let _ = msg.reply.send(outcome);
            }
            log::debug!("permission-ask bridge task ended");
        });

        Promise::from_future(async move {
            subscribers.write().await.push(tx);
            Ok(())
        })
    }

    // ========================================================================
    // Peer Registry (drift navigation transport)
    // ========================================================================

    fn attach_peer(
        self: Rc<Self>,
        params: kernel::AttachPeerParams,
        mut results: kernel::AttachPeerResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let config_reader = pry!(params_reader.get_config());

        let nick = pry!(config_reader.get_nick())
            .to_str()
            .unwrap_or("unknown")
            .to_owned();

        // Stamp the peer's principal from the authoritative connection identity
        // — never trusted from the client. The client-supplied `instance` is
        // just a uniqueness token (no trust claim); empty keys the peer by nick.
        let principal = self.connection.borrow().principal.id;
        let instance = config_reader
            .get_instance()
            .ok()
            .and_then(|t| t.to_str().ok())
            .unwrap_or("")
            .to_owned();
        // The key this peer is stored under — derived the same way the registry
        // does, so the bridge task can self-detach exactly this entry.
        let detach_key = peer_key(&nick, &instance);
        let config = PeerConfig {
            nick: nick.clone(),
            instance,
            principal: Some(principal),
        };

        // Extract optional PeerCommands callback for reverse invocation.
        // get_commands() returns Err for null/missing capability pointers —
        // this is the standard capnp pattern for optional capabilities.
        let commands_callback = params_reader.get_commands().ok();

        let kernel_arc = self.kernel.kernel.clone();
        // Wake the bridge task when this connection drops (Drop fires
        // conn_cancel), so it can self-detach instead of lingering on rx.recv()
        // with a dead callback — the same idiom the FlowBus bridges use.
        let conn_cancel = self.connection.borrow().cancel_token();

        let span = tracing::info_span!("rpc", method = "attach_peer");
        Promise::from_future(
            async move {
                // Create invoke channel if callback provided
                let invoke_sender = if let Some(callback) = commands_callback {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<InvokeRequest>(32);
                    let nick_for_task = nick.clone();
                    let bridge_kernel = kernel_arc.clone();
                    let detach_key_for_task = detach_key.clone();
                    // A clone of our own sender, used purely as an identity token
                    // for self-detach: we remove our registry entry only if it's
                    // still ours (a re-attach may have replaced it). Holding it
                    // also keeps `rx` open, so the task exits on conn_cancel, not
                    // on a transient sender drop.
                    let tx_self = tx.clone();

                    // Bridge task: recv InvokeRequest from channel, call capnp callback.
                    // Per-invoke timeout matches the client-side bound (15s) so a
                    // peer that stops reading can't pin this task on the LocalSet.
                    const PEER_INVOKE_TIMEOUT: std::time::Duration =
                        std::time::Duration::from_secs(20);
                    tokio::task::spawn_local(async move {
                        loop {
                            let request = tokio::select! {
                                // Connection dropped: stop waiting and self-detach.
                                _ = conn_cancel.cancelled() => break,
                                maybe = rx.recv() => match maybe {
                                    Some(request) => request,
                                    None => break,
                                },
                            };
                            let mut req = callback.invoke_request();
                            {
                                let mut p = req.get();
                                p.set_action(&request.action);
                                p.set_params(&request.params);
                            }
                            let result = match tokio::time::timeout(
                                PEER_INVOKE_TIMEOUT, req.send().promise,
                            ).await {
                                Ok(Ok(response)) => match response.get().and_then(|r| r.get_result()) {
                                    Ok(data) => Ok(data.to_vec()),
                                    Err(e) => Err(format!("capnp read error: {e}")),
                                },
                                Ok(Err(e)) => Err(format!("RPC error: {e}")),
                                Err(_) => Err(format!(
                                    "peer invoke timed out after {:?}", PEER_INVOKE_TIMEOUT,
                                )),
                            };
                            if request.reply.send(InvokeResponse { result }).is_err() {
                                tracing::debug!(
                                    nick = %nick_for_task,
                                    "Peer invoke reply dropped (caller likely timed out)",
                                );
                            }
                        }
                        // Self-detach this exact peer so a dropped connection's
                        // window can't linger in the registry (and out of any
                        // principal/nick fan-out) — but only if we're still the
                        // registered owner, so an old task can't clobber a peer
                        // that re-attached under the same key. reap_closed is the
                        // backstop for anything this misses.
                        let removed = bridge_kernel
                            .detach_peer_if_sender(&detach_key_for_task, &tx_self)
                            .await;
                        log::debug!(
                            "Peer invoke bridge for '{}' ended; self-detach {} = {}",
                            nick_for_task, detach_key_for_task, removed,
                        );
                    });

                    Some(tx)
                } else {
                    None
                };

                let peer_info = kernel_arc
                    .attach_peer(config, invoke_sender)
                    .await
                    .map_err(|e| capnp::Error::failed(format!("failed to attach peer: {}", e)))?;

                let mut info = results.get().init_info();
                set_peer_info(&mut info, &peer_info);

                Ok(())
            }
            .instrument(span),
        )
    }

    fn list_peers(
        self: Rc<Self>,
        _params: kernel::ListPeersParams,
        mut results: kernel::ListPeersResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "list_peers");
        Promise::from_future(
            async move {
                let peers = kernel_arc.list_peers().await;

                let mut list = results.get().init_peers(peers.len() as u32);
                for (i, peer) in peers.iter().enumerate() {
                    let mut p = list.reborrow().get(i as u32);
                    set_peer_info(&mut p, peer);
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn detach_peer(
        self: Rc<Self>,
        params: kernel::DetachPeerParams,
        _results: kernel::DetachPeerResults,
    ) -> Promise<(), capnp::Error> {
        let nick = pry!(pry!(pry!(params.get()).get_nick()).to_str()).to_owned();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "detach_peer");
        Promise::from_future(
            async move {
                kernel_arc.detach_peer(&nick).await;
                Ok(())
            }
            .instrument(span),
        )
    }

    fn invoke_peer(
        self: Rc<Self>,
        params: kernel::InvokePeerParams,
        mut results: kernel::InvokePeerResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let nick = pry!(pry!(params_reader.get_nick()).to_str()).to_owned();
        let action = pry!(pry!(params_reader.get_action()).to_str()).to_owned();
        let invoke_params = pry!(params_reader.get_params()).to_vec();

        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "invoke_peer");
        Promise::from_future(
            async move {
                let result = kernel_arc.invoke_peer(&nick, &action, invoke_params).await;
                match result {
                    Ok(data) => {
                        results.get().set_result(&data);
                        Ok(())
                    }
                    Err(e) => Err(capnp::Error::failed(format!("invoke_peer: {e}"))),
                }
            }
            .instrument(span),
        )
    }

    // ========================================================================
    // Timeline Navigation
    // ========================================================================

    // forkFromVersion removed — consolidated to kj fork

    fn cherry_pick_block(
        self: Rc<Self>,
        params: kernel::CherryPickBlockParams,
        mut results: kernel::CherryPickBlockResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let source_block_id_reader = pry!(params_reader.get_source_block_id());
        let source_block_id = pry!(parse_block_id_from_reader(&source_block_id_reader));
        // Schema: cherryPickBlock(sourceBlockId, targetContextId :Data)
        let target_ctx_bytes = pry!(params_reader.get_target_context_id());
        let target_ctx_id =
            pry!(
                ContextId::try_from_slice(target_ctx_bytes).ok_or_else(|| capnp::Error::failed(
                    "invalid target context ID (expected 16 bytes)".into()
                ))
            );

        log::info!(
            "Cherry-pick request: block={} to context={}",
            source_block_id.to_key(),
            target_ctx_id
        );

        let documents = self.kernel.documents.clone();
        let kernel_arc = self.kernel.kernel.clone();
        let user_principal_id = self.connection.borrow().principal.id;

        // Get the source document and extract block snapshot (DashMap access is sync)
        let doc_entry = match documents.get(source_block_id.context_id) {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Source document not found".into())),
        };
        let block_snapshot = match doc_entry.doc.get_block_snapshot(&source_block_id) {
            Some(snapshot) => snapshot,
            None => return Promise::err(capnp::Error::failed("Block not found".into())),
        };
        drop(doc_entry);

        let span = extract_rpc_trace(params_reader.get_trace(), "cherry_pick_block");
        Promise::from_future(
            async move {
                // Look up target context in drift router for trace linkage
                let drift = kernel_arc.drift().read();
                let target_handle = drift.get(target_ctx_id).ok_or_else(|| {
                    capnp::Error::failed(format!("target context {} not found", target_ctx_id))
                })?;
                let trace_id = target_handle.trace_id;
                drop(drift);
                let _ctx_span =
                    kaijutsu_telemetry::context_root_span(&trace_id, "cherry_pick_block").entered();

                // Target document must exist
                if !documents.contains(target_ctx_id) {
                    return Err(capnp::Error::failed(format!(
                        "target context {} not found — join target context first",
                        target_ctx_id
                    )));
                }

                // Insert the block into target document (authored by calling user)
                let after_id = documents.last_block_id(target_ctx_id);
                let new_block_id = documents
                    .insert_block_as(
                        target_ctx_id,
                        None,
                        after_id.as_ref(),
                        block_snapshot.role,
                        block_snapshot.kind,
                        block_snapshot.content,
                        Status::Done,
                        block_snapshot.content_type,
                        Some(user_principal_id),
                    )
                    .map_err(|e| capnp::Error::failed(format!("Failed to insert block: {}", e)))?;

                // Build result
                let mut new_block_builder = results.get().init_new_block_id();
                set_block_id_builder(&mut new_block_builder, &new_block_id);

                log::info!(
                    "Cherry-pick complete: {} -> {}",
                    source_block_id.to_key(),
                    new_block_id.to_key()
                );
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_context_history(
        self: Rc<Self>,
        params: kernel::GetContextHistoryParams,
        mut results: kernel::GetContextHistoryResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let _span = extract_rpc_trace(params_reader.get_trace(), "get_context_history").entered();
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let limit = params_reader.get_limit() as usize;

        let _ctx_span = if let Some(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "get_document_history").entered())
        } else {
            None
        };

        // Get the document (DashMap access is sync)
        let doc_entry = self.kernel.documents.get(context_id);
        let doc_entry = match doc_entry {
            Some(entry) => entry,
            None => return Promise::err(capnp::Error::failed("Document not found".into())),
        };

        // Get blocks ordered by creation time to build version history
        let blocks = doc_entry.doc.blocks_ordered();
        let current_version = doc_entry.version();

        // For now, each block addition is a "version snapshot"
        // In the future, this could be more granular (edits, etc.)
        let snapshot_count = blocks.len().min(limit);
        let mut snapshots = results.get().init_snapshots(snapshot_count as u32);

        for (i, block) in blocks.iter().take(limit).enumerate() {
            let mut snapshot = snapshots.reborrow().get(i as u32);
            snapshot.set_version(i as u64 + 1);
            snapshot.set_timestamp(block.created_at);
            snapshot.set_block_count((i + 1) as u32);
            snapshot.set_change_kind(crate::kaijutsu_capnp::ChangeKind::BlockAdded);

            let mut block_id_builder = snapshot.init_changed_block_id();
            set_block_id_builder(&mut block_id_builder, &block.id);
        }

        log::debug!(
            "Document history: {} snapshots (current version: {})",
            snapshot_count,
            current_version
        );
        Promise::ok(())
    }

    // =========================================================================
    // Config Methods (Phase 2: Config as CRDT)
    // =========================================================================

    fn list_configs(
        self: Rc<Self>,
        _params: kernel::ListConfigsParams,
        mut results: kernel::ListConfigsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel = self.kernel.kernel.clone();
        let span = tracing::info_span!("rpc", method = "list_configs");
        Promise::from_future(
            async move {
                use kaijutsu_kernel::vfs::VfsOps;
                // Config is CRDT-owned; the live set is whatever the /etc/config
                // mount holds. Bare file names, matching the old surface.
                let names: Vec<String> = match kernel
                    .vfs()
                    .readdir(std::path::Path::new(paths::CONFIG_ROOT))
                    .await
                {
                    Ok(entries) => entries
                        .into_iter()
                        .filter(|e| e.kind.is_file())
                        .map(|e| e.name)
                        .collect(),
                    Err(e) => {
                        log::warn!("list_configs: readdir /etc/config failed: {e}");
                        Vec::new()
                    }
                };
                let mut builder = results.get().init_configs(names.len() as u32);
                for (i, name) in names.iter().enumerate() {
                    builder.set(i as u32, name);
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    /// Disk reload is meaningless now that the CRDT is the sole owner — there is
    /// no host file to reload from. `reloadConfig` is repurposed as "restore this
    /// config to its embedded default" (same as `resetConfig`); kept on the wire
    /// for client compatibility.
    fn reload_config(
        self: Rc<Self>,
        params: kernel::ReloadConfigParams,
        mut results: kernel::ReloadConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let kernel = self.kernel.kernel.clone();
        let span = tracing::info_span!("rpc", method = "reload_config");
        Promise::from_future(
            async move {
                let (ok, err) = reset_config_to_embedded(&kernel, &path).await;
                results.get().set_success(ok);
                results.get().set_error(&err);
                Ok(())
            }
            .instrument(span),
        )
    }

    fn reset_config(
        self: Rc<Self>,
        params: kernel::ResetConfigParams,
        mut results: kernel::ResetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let kernel = self.kernel.kernel.clone();
        let span = tracing::info_span!("rpc", method = "reset_config");
        Promise::from_future(
            async move {
                let (ok, err) = reset_config_to_embedded(&kernel, &path).await;
                results.get().set_success(ok);
                results.get().set_error(&err);
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_config(
        self: Rc<Self>,
        params: kernel::GetConfigParams,
        mut results: kernel::GetConfigResults,
    ) -> Promise<(), capnp::Error> {
        let path = pry!(pry!(pry!(params.get()).get_path()).to_str()).to_owned();
        let kernel = self.kernel.kernel.clone();
        let span = tracing::info_span!("rpc", method = "get_config");
        Promise::from_future(
            async move {
                use kaijutsu_kernel::vfs::VfsOps;
                let canonical = config_canonical(&path);
                match kernel
                    .vfs()
                    .read_all(std::path::Path::new(&canonical))
                    .await
                {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(content) => {
                            results.get().set_content(&content);
                            results.get().set_error("");
                        }
                        Err(e) => {
                            results.get().set_content("");
                            results
                                .get()
                                .set_error(format!("config {canonical} is not UTF-8: {e}"));
                        }
                    },
                    Err(e) => {
                        results.get().set_content("");
                        results.get().set_error(format!("{e}"));
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    // ========================================================================
    // Drift: Cross-Context Communication
    // ========================================================================

    fn get_context_id(
        self: Rc<Self>,
        params: kernel::GetContextIdParams,
        mut results: kernel::GetContextIdResults,
    ) -> Promise<(), capnp::Error> {
        let ctx_id = pry!(self.connection.borrow().require_context());
        let kernel_arc = self.kernel.kernel.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_context_id");
        Promise::from_future(
            async move {
                results.get().set_id(ctx_id.as_bytes());
                let drift = kernel_arc.drift().read();
                let label = drift
                    .get(ctx_id)
                    .and_then(|h| h.label.as_deref())
                    .unwrap_or("");
                results.get().set_label(label);
                Ok(())
            }
            .instrument(span),
        )
    }

    fn configure_llm(
        self: Rc<Self>,
        params: kernel::ConfigureLlmParams,
        mut results: kernel::ConfigureLlmResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();

        // Use explicit contextId if provided (16 bytes), otherwise connection's current
        let ctx_id_bytes = pry!(params_reader.get_context_id());
        let ctx_id = if ctx_id_bytes.len() == 16 {
            pry!(
                ContextId::try_from_slice(ctx_id_bytes)
                    .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
            )
        } else {
            pry!(self.connection.borrow().require_context())
        };

        let shared_kernel = self.kernel.clone();
        let span = extract_rpc_trace(params_reader.get_trace(), "configure_llm");
        Promise::from_future(
            async move {
                // Validate the backend before persisting — never write bad
                // data. A backend must already EXIST (`kj backend set`): the
                // name alone no longer implies a kind, so there is nothing
                // sensible to construct from a bare string, and inventing one
                // here would let a typo persist onto a context row.
                let known = kaijutsu_kernel::build_llm_registry(&shared_kernel.kernel_db.lock())
                    .map(|r| r.backend_config(&provider_name).is_some())
                    .unwrap_or(false);
                match known {
                    true => {
                        // Provider is valid — now persist
                        {
                            let db = shared_kernel.kernel_db.lock();
                            if let Err(e) =
                                db.update_model(ctx_id, Some(&provider_name), Some(&model))
                            {
                                log::warn!(
                                    "KernelDb update_model failed for {}: {}",
                                    ctx_id.short(),
                                    e
                                );
                            }
                        }
                        {
                            let mut drift = kernel_arc.drift().write();
                            let _ = drift.configure_llm(ctx_id, &provider_name, &model);
                        }
                        // The backend is already registered (that is what
                        // `known` established); kernel-wide defaults are NOT
                        // touched — the model is per-context.
                        results.get().set_success(true);
                        results.get().set_error("");
                        log::info!(
                            "Context {} model set: provider={}, model={}",
                            ctx_id.short(),
                            provider_name,
                            model
                        );
                    }
                    false => {
                        let msg = format!(
                            "unknown backend '{provider_name}' \
                             (configure it with `kj backend set {provider_name} --kind …`)"
                        );
                        results.get().set_success(false);
                        results.get().set_error(&msg);
                        log::warn!(
                            "Failed to configure LLM for context {}: {msg}",
                            ctx_id.short(),
                        );
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn drift_queue(
        self: Rc<Self>,
        _params: kernel::DriftQueueParams,
        mut results: kernel::DriftQueueResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "drift_queue");
        Promise::from_future(
            async move {
                let drift = kernel_arc.drift().read();
                let queue = drift.queue();

                let mut list = results.get().init_staged(queue.len() as u32);
                for (i, drift_item) in queue.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_id(drift_item.id);
                    entry.set_source_ctx(drift_item.source_ctx.as_bytes());
                    entry.set_target_ctx(drift_item.target_ctx.as_bytes());
                    entry.set_content(&drift_item.content);
                    entry.set_source_model(drift_item.source_model.as_deref().unwrap_or(""));
                    entry.set_drift_kind(drift_kind_to_capnp(drift_item.drift_kind));
                    entry.set_created_at(drift_item.created_at);
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn drift_cancel(
        self: Rc<Self>,
        params: kernel::DriftCancelParams,
        mut results: kernel::DriftCancelResults,
    ) -> Promise<(), capnp::Error> {
        let staged_id = pry!(params.get()).get_staged_id();
        let kernel_arc = self.kernel.kernel.clone();

        let span = tracing::info_span!("rpc", method = "drift_cancel");
        Promise::from_future(
            async move {
                let mut drift = kernel_arc.drift().write();
                let success = drift.cancel(staged_id);
                results.get().set_success(success);
                Ok(())
            }
            .instrument(span),
        )
    }

    // listAllContexts was removed — listContexts now reads from kernel's drift router

    // ========================================================================
    // LLM Configuration (Phase 5)
    // ========================================================================

    fn get_llm_config(
        self: Rc<Self>,
        params: kernel::GetLlmConfigParams,
        mut results: kernel::GetLlmConfigResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();

        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "get_llm_config");
        Promise::from_future(
            async move {
                let registry = kernel_arc.llm().read().await;

                let mut config = results.get().init_config();
                config.set_default_provider(registry.default_provider_name().unwrap_or(""));
                config.set_default_model(registry.default_model().unwrap_or(""));

                // Only include available providers (skip disabled/unconfigured)
                let provider_names: Vec<&str> = registry
                    .list()
                    .into_iter()
                    .filter(|name| registry.get(name).is_some())
                    .collect();
                let mut providers = config.init_providers(provider_names.len() as u32);
                for (i, name) in provider_names.iter().enumerate() {
                    let mut entry = providers.reborrow().get(i as u32);
                    entry.set_name(name);
                    entry.set_available(true);
                    if let Some(p) = registry.get(name) {
                        let avail = p.available_models();
                        entry.set_default_model(avail.first().copied().unwrap_or(""));
                    }
                    // Populate full models list from aliases + default
                    let model_ids = registry.models_for_provider(name);
                    let mut models_list = entry.init_models(model_ids.len() as u32);
                    for (j, model_id) in model_ids.iter().enumerate() {
                        models_list.set(j as u32, model_id);
                    }
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_default_provider(
        self: Rc<Self>,
        params: kernel::SetDefaultProviderParams,
        mut results: kernel::SetDefaultProviderResults,
    ) -> Promise<(), capnp::Error> {
        let provider_name = pry!(pry!(pry!(params.get()).get_provider()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();
        let kernel_db = self.kernel.kernel_db.clone();

        let span = tracing::info_span!("rpc", method = "set_default_provider");
        Promise::from_future(
            async move {
                // DB-first, then rebuild — the same write-then-reload shape
                // every `kj backend/cast/alias` mutation uses. The old body
                // mutated the in-memory registry only, so the change silently
                // reverted on the next config write's reload (or restart):
                // registry drifting from DB truth, the dual-ownership disease
                // this config system exists to prevent. `set_llm_defaults`
                // refuses a dangling backend name, so validation rides the
                // write.
                let rebuilt = {
                    let db = kernel_db.lock();
                    let row = match db.get_llm_defaults() {
                        Ok(Some(mut d)) => {
                            d.default_backend = provider_name.clone();
                            d
                        }
                        Ok(None) => {
                            results.get().set_success(false);
                            results.get().set_error(
                                "no llm_defaults row — run `kj backend reseed` \
                                 or `kj backend default set`",
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            results.get().set_success(false);
                            results.get().set_error(format!("reading llm_defaults: {e}"));
                            return Ok(());
                        }
                    };
                    if let Err(e) = db.set_llm_defaults(&row) {
                        results.get().set_success(false);
                        results.get().set_error(format!("{e}"));
                        return Ok(());
                    }
                    kaijutsu_kernel::build_llm_registry(&db)
                };
                match rebuilt {
                    Ok(reg) => {
                        *kernel_arc.llm().write().await = reg;
                        results.get().set_success(true);
                        results.get().set_error("");
                        log::info!("Default LLM backend set to: {}", provider_name);
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(format!(
                            "write landed but the LLM registry failed to reload: {e}"
                        ));
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_default_model(
        self: Rc<Self>,
        params: kernel::SetDefaultModelParams,
        mut results: kernel::SetDefaultModelResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let provider_name = pry!(pry!(params_reader.get_provider()).to_str()).to_owned();
        let model = pry!(pry!(params_reader.get_model()).to_str()).to_owned();
        let kernel_arc = self.kernel.kernel.clone();
        let kernel_db = self.kernel.kernel_db.clone();

        let span = tracing::info_span!("rpc", method = "set_default_model");
        Promise::from_future(
            async move {
                // DB-first + rebuild, same shape as set_default_provider
                // above (and the kj mutations). Takes both halves of the
                // identity, so it sets backend AND model together.
                let rebuilt = {
                    let db = kernel_db.lock();
                    let row = match db.get_llm_defaults() {
                        Ok(Some(mut d)) => {
                            d.default_backend = provider_name.clone();
                            d.default_model = model.clone();
                            d
                        }
                        Ok(None) => {
                            results.get().set_success(false);
                            results.get().set_error(
                                "no llm_defaults row — run `kj backend reseed` \
                                 or `kj backend default set`",
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            results.get().set_success(false);
                            results.get().set_error(format!("reading llm_defaults: {e}"));
                            return Ok(());
                        }
                    };
                    if let Err(e) = db.set_llm_defaults(&row) {
                        results.get().set_success(false);
                        results.get().set_error(format!("{e}"));
                        return Ok(());
                    }
                    kaijutsu_kernel::build_llm_registry(&db)
                };
                match rebuilt {
                    Ok(reg) => {
                        *kernel_arc.llm().write().await = reg;
                        results.get().set_success(true);
                        results.get().set_error("");
                        log::info!(
                            "Default model set to: {} (backend: {})",
                            model,
                            provider_name
                        );
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(format!(
                            "write landed but the LLM registry failed to reload: {e}"
                        ));
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    // Phase 5 D-54: get/setToolFilter and get/setContextToolFilter retired.
    // Capnp ordinals 69/70/85/86 renamed to `...Removed` stubs; the
    // generated trait's default `unimplemented` impls cover them.
    // See `builtin.bindings` MCP tools for the replacement.

    // forkFiltered removed — consolidated to kj fork

    // =========================================================================
    // Shell Variable Introspection
    // =========================================================================

    fn get_shell_var(
        self: Rc<Self>,
        params: kernel::GetShellVarParams,
        mut results: kernel::GetShellVarResults,
    ) -> Promise<(), capnp::Error> {
        let name = match params.get().and_then(|p| p.get_name()) {
            Ok(n) => match n.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => {
                    return Promise::err(capnp::Error::failed(format!("invalid name: {}", e)));
                }
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("missing name: {}", e))),
        };

        let connection = self.connection.clone();
        let kernel = self.kernel.clone();

        let span = tracing::info_span!("rpc", method = "get_shell_var");
        Promise::from_future(
            async move {
                // Shell vars are durable, context-scoped env (L1 `context_env`,
                // string-only). A var set here is equivalent to
                // `kj context set --env` and visible to every materialized shell.
                let context_id = connection.borrow().require_context()?;
                let value = kernel
                    .kernel_db
                    .lock()
                    .get_context_env(context_id)
                    .map_err(|e| {
                        capnp::Error::failed(format!("failed to read context env: {}", e))
                    })?
                    .into_iter()
                    .find(|row| row.key == name)
                    .map(|row| row.value);

                let mut builder = results.get();
                if let Some(val) = value {
                    builder.set_found(true);
                    value_to_shell_value(builder.init_value(), &kaish_kernel::ast::Value::String(val));
                } else {
                    builder.set_found(false);
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn set_shell_var(
        self: Rc<Self>,
        params: kernel::SetShellVarParams,
        mut results: kernel::SetShellVarResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let name = pry!(pry!(reader.get_name()).to_str()).to_owned();
        let value_reader = pry!(reader.get_value());
        let value = match shell_value_to_value(value_reader) {
            Ok(v) => v,
            Err(e) => {
                results.get().set_success(false);
                results.get().set_error(format!("invalid value: {}", e));
                return Promise::ok(());
            }
        };

        let connection = self.connection.clone();
        let kernel = self.kernel.clone();

        let span = tracing::info_span!("rpc", method = "set_shell_var");
        Promise::from_future(
            async move {
                // Durable, context-scoped write to L1. Structured values collapse
                // to string (env is string-only) — the lossy half of the
                // shell-var ⇄ env round-trip, accepted by design.
                let context_id = connection.borrow().require_context()?;
                kernel
                    .kernel_db
                    .lock()
                    .set_context_env(context_id, &name, &value_to_env_string(&value))
                    .map_err(|e| {
                        capnp::Error::failed(format!("failed to persist context env: {}", e))
                    })?;
                results.get().set_success(true);
                results.get().set_error("");
                Ok(())
            }
            .instrument(span),
        )
    }

    fn list_shell_vars(
        self: Rc<Self>,
        _params: kernel::ListShellVarsParams,
        mut results: kernel::ListShellVarsResults,
    ) -> Promise<(), capnp::Error> {
        let connection = self.connection.clone();
        let kernel = self.kernel.clone();

        let span = tracing::info_span!("rpc", method = "list_shell_vars");
        Promise::from_future(
            async move {
                // List the context's durable env (L1) as shell vars.
                let context_id = connection.borrow().require_context()?;
                let rows = kernel
                    .kernel_db
                    .lock()
                    .get_context_env(context_id)
                    .map_err(|e| {
                        capnp::Error::failed(format!("failed to list context env: {}", e))
                    })?;

                let mut list_builder = results.get().init_vars(rows.len() as u32);
                for (i, row) in rows.iter().enumerate() {
                    let mut entry = list_builder.reborrow().get(i as u32);
                    entry.set_name(&row.key);
                    value_to_shell_value(
                        entry.init_value(),
                        &kaish_kernel::ast::Value::String(row.value.clone()),
                    );
                }

                Ok(())
            }
            .instrument(span),
        )
    }

    // ========================================================================
    // Per-client durable view state (docs/shared-state.md "Retiring KV")
    // ========================================================================

    fn set_last_context(
        self: Rc<Self>,
        params: kernel::SetLastContextParams,
        _results: kernel::SetLastContextResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "set_last_context").entered();
        let p = pry!(params.get());
        let client_id = pry!(pry!(p.get_client_id()).to_str()).to_owned();
        let context_id_str = pry!(pry!(p.get_context_id()).to_str());
        let context_id = pry!(
            ContextId::parse(context_id_str)
                .map_err(|e| capnp::Error::failed(format!("invalid context id: {}", e)))
        );
        pry!(
            self.kernel
                .kernel_db
                .lock()
                .set_client_view(&client_id, context_id)
                .map_err(|e| capnp::Error::failed(format!("failed to persist client view: {}", e)))
        );

        // Auto-promote on visit: the app calls this on every context switch,
        // and a context with no explicit ring placement yet (never promoted,
        // demoted, concluded, or archived) rides into ring 0 automatically.
        // Explicit demotion/conclusion are sticky — only an explicit promote
        // brings those back. Recording the visit is the primary contract of
        // this RPC, so a failed auto-promote must never fail the call.
        // The gap between the persist-view lock above and this second lock is
        // TOCTOU-harmless: the row is re-read here (and again inside
        // promote_context) before anything acts on it.
        {
            let db = self.kernel.kernel_db.lock();
            match db.get_context(context_id) {
                Ok(Some(row))
                    if row.promoted_at.is_none()
                        && row.demoted_at.is_none()
                        && row.concluded_at.is_none()
                        && row.archived_at.is_none() =>
                {
                    match db.promote_context(context_id) {
                        Ok(_) => {}
                        Err(KernelDbError::Validation(_)) => {
                            log::info!(
                                "set_last_context: visit auto-promote skipped: active ring full (context={})",
                                context_id.short()
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "set_last_context: auto-promote failed for context={}: {e}",
                                context_id.short()
                            );
                        }
                    }
                }
                Ok(_) => {} // explicit placement already set, or context unknown
                Err(e) => {
                    log::warn!(
                        "set_last_context: failed to read context={} for auto-promote: {e}",
                        context_id.short()
                    );
                }
            }
        }

        Promise::ok(())
    }

    fn get_client_view(
        self: Rc<Self>,
        params: kernel::GetClientViewParams,
        mut results: kernel::GetClientViewResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "get_client_view").entered();
        let client_id = pry!(pry!(pry!(params.get()).get_client_id()).to_str()).to_owned();
        let view = pry!(
            self.kernel
                .kernel_db
                .lock()
                .get_client_view(&client_id)
                .map_err(|e| capnp::Error::failed(format!("failed to read client view: {}", e)))
        );
        let mut b = results.get();
        match view {
            Some(context_id) => {
                b.set_context_id(context_id.to_string());
                b.set_found(true);
            }
            None => {
                b.set_context_id("");
                b.set_found(false);
            }
        }
        Promise::ok(())
    }

    /// Enumerate every track's live state (docs/tracks.md; the time well's
    /// Stage 3 wire slice). Answered by the beat scheduler from its in-memory
    /// `TrackState` map — never the persisted row, whose playhead lags. An
    /// unwired scheduler (embedded/test kernel) answers with an empty list
    /// rather than an error: "no scheduler" and "no tracks" look the same to a
    /// viewer, and the well should render either as simply trackless.
    fn list_tracks(
        self: Rc<Self>,
        params: kernel::ListTracksParams,
        mut results: kernel::ListTracksResults,
    ) -> Promise<(), capnp::Error> {
        let kernel_arc = self.kernel.kernel.clone();
        let span = extract_rpc_trace(pry!(params.get()).get_trace(), "list_tracks");
        Promise::from_future(
            async move {
                let tracks = match kernel_arc.request_track_snapshot() {
                    Some(rx) => rx.await.unwrap_or_default(),
                    None => Vec::new(),
                };
                let mut list = results.get().init_tracks(tracks.len() as u32);
                for (i, t) in tracks.iter().enumerate() {
                    let mut b = list.reborrow().get(i as u32);
                    b.set_id(t.id.as_str());
                    b.set_score_context_id(t.score_context.as_bytes());
                    b.set_playing(t.playing);
                    b.set_playhead_tick(t.playhead);
                    b.set_period_us(t.period.as_micros() as u64);
                    b.set_beats_per_phrase(t.beats_per_phrase);
                    b.set_beat_count(t.beat_count);
                    b.set_last_epoch_ns(t.last_epoch_ns);
                    b.set_clock_kind(&t.clock_kind);
                    let mut att = b.init_attached(t.attached.len() as u32);
                    for (j, ctx) in t.attached.iter().enumerate() {
                        att.set(j as u32, ctx.as_bytes());
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn compact_context(
        self: Rc<Self>,
        params: kernel::CompactContextParams,
        mut results: kernel::CompactContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "compact_context").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let _ctx_span = if let Some(drift) = self.kernel.kernel.drift().try_read() {
            let trace_id = drift.trace_id_for_context(context_id).unwrap_or([0u8; 16]);
            Some(kaijutsu_telemetry::context_root_span(&trace_id, "compact_document").entered())
        } else {
            None
        };

        // Per-block DTE stores don't need compaction — each block's DTE
        // is already minimal. This is intentionally a no-op; sync_generation
        // stays 0 and SyncReset is never emitted. If compaction is ever
        // reintroduced, bump DocumentEntry.sync_generation and emit
        // BlockFlow::SyncReset so clients can resync their frontier.
        let mut r = results.get();
        r.set_new_size(0);
        r.set_generation(0);
        Promise::ok(())
    }

    // =========================================================================
    // Input document operations (compose scratchpad)
    // =========================================================================

    fn edit_input(
        self: Rc<Self>,
        params: kernel::EditInputParams,
        mut results: kernel::EditInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "edit_input");
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let pos = p.get_pos() as usize;
        let insert = pry!(pry!(p.get_insert()).to_str()).to_owned();
        let delete = p.get_delete() as usize;

        log::debug!(
            "edit_input: context={}, pos={}, insert_len={}, delete={}",
            context_id,
            pos,
            insert.len(),
            delete
        );

        let kernel = self.kernel.clone();
        Promise::from_future(
            async move {
                // Shared facade gate — both live compose typing (app) and the
                // MCP write_input/edit_input handlers reach the input doc through
                // this RPC, so the allow-set is enforced here for everyone.
                kernel
                    .kernel
                    .broker()
                    .check_facade(&context_id, "edit_input")
                    .await
                    .map_err(|e| capnp::Error::failed(format!("edit_input denied: {e}")))?;

                match kernel.documents.edit_input(context_id, pos, &insert, delete) {
                    Ok(_ops) => {
                        // edit_input emits InputDocFlow::TextOps via FlowBus; the
                        // version is implicit from the DTE document, return 0 as ack.
                        results.get().set_ack_version(0);
                        Ok(())
                    }
                    Err(e) => Err(capnp::Error::failed(format!("edit_input failed: {}", e))),
                }
            }
            .instrument(span),
        )
    }

    fn get_input_state(
        self: Rc<Self>,
        params: kernel::GetInputStateParams,
        mut results: kernel::GetInputStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_input_state").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        log::debug!("get_input_state: context={}", context_id);

        let documents = &self.kernel.documents;

        match documents.get_input_state(context_id) {
            Ok((content, ops, version)) => {
                let mut r = results.get();
                r.set_content(&content);
                r.set_ops(&ops);
                r.set_version(version);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!(
                "get_input_state failed: {}",
                e
            ))),
        }
    }

    fn push_input_ops(
        self: Rc<Self>,
        params: kernel::PushInputOpsParams,
        mut results: kernel::PushInputOpsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "push_input_ops").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let ops_data = pry!(p.get_ops()).to_vec();

        log::debug!(
            "push_input_ops: context={}, ops_len={}",
            context_id,
            ops_data.len()
        );

        let documents = &self.kernel.documents;

        match documents.merge_input_ops(context_id, &ops_data) {
            Ok(version) => {
                results.get().set_ack_version(version);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(format!(
                "push_input_ops failed: {}",
                e
            ))),
        }
    }

    fn submit_input(
        self: Rc<Self>,
        params: kernel::SubmitInputParams,
        mut results: kernel::SubmitInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let trace_span = extract_rpc_trace(p.get_trace(), "submit_input");
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let is_shell = pry!(p.get_mode()) == InputMode::Shell;

        log::info!("submit_input: context={} shell={}", context_id, is_shell);

        let kernel = self.kernel.clone();
        let connection = self.connection.clone();
        let user_principal_id = self.connection.borrow().principal.id;

        Promise::from_future(
            async move {
                // Shared facade gate (deny-by-default): submit is reachable by
                // both the app (Enter in compose) and the MCP submit_input tool.
                kernel
                    .kernel
                    .broker()
                    .check_facade(&context_id, "submit_input")
                    .await
                    .map_err(|e| capnp::Error::failed(format!("submit_input denied: {e}")))?;

                let documents = kernel.documents.clone();

                // Read text first, validate, THEN clear — avoids clearing compose
                // on whitespace-only input (InputCleared would fire with no block created).
                let text = documents
                    .get_input_text(context_id)
                    .map_err(|e| capnp::Error::failed(format!("get_input_text: {}", e)))?;
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Err(capnp::Error::failed("input is empty".into()));
                }
                // Input has content — now clear it
                documents
                    .clear_input(context_id)
                    .map_err(|e| capnp::Error::failed(format!("clear_input failed: {}", e)))?;

                if is_shell {
                    let command_block_id = execute_shell_command(
                        &text,
                        context_id,
                        user_principal_id,
                        true,
                        &kernel,
                        &connection,
                    )
                    .await?;

                    let mut block_id_builder = results.get().init_command_block_id();
                    set_block_id_builder(&mut block_id_builder, &command_block_id);
                } else {
                    // Chat prompt — create user message block and invoke LLM

                    // Document must exist — join_context is the sole creator
                    if documents.get(context_id).is_none() {
                        return Err(capnp::Error::failed(format!(
                            "context {} not found — call join_context first",
                            context_id
                        )));
                    }

                    // Build ToolContext from connection state; cwd is durable
                    // context-scoped state (L1).
                    let session_id = connection.borrow().session_id;
                    let tool_ctx = match context_cwd(&kernel, context_id) {
                        Some(cwd) => kaijutsu_kernel::ExecContext::new(
                            user_principal_id,
                            context_id,
                            cwd,
                            session_id,
                            kernel.id,
                        ),
                        None => kaijutsu_kernel::ExecContext::new_without_cwd(
                            user_principal_id,
                            context_id,
                            session_id,
                            kernel.id,
                        ),
                    };

                    // Create user message block at the end of the document
                    let last_block = documents.last_block_id(context_id);
                    let user_block_id = documents
                        .insert_block_as(
                            context_id,
                            None,
                            last_block.as_ref(),
                            Role::User,
                            BlockKind::Text,
                            &text,
                            Status::Done,
                            ContentType::Plain,
                            Some(user_principal_id),
                        )
                        .map_err(|e| {
                            capnp::Error::failed(format!("failed to insert user block: {}", e))
                        })?;

                    // Spawn LLM streaming in background. An interactive chat
                    // prompt via submit_input: it announces like every other
                    // turn; the `Interactive` origin keeps it out of the
                    // musician's OODA Act (design §7) while still reaching every
                    // `subscribeTurnEvents` client — this is the turn an ACP
                    // frontend is waiting on.
                    spawn_llm_for_prompt(
                        &kernel,
                        context_id,
                        None,
                        &user_block_id,
                        tool_ctx,
                        user_principal_id,
                        TurnOrigin::Interactive,
                    )
                    .await?;

                    let mut block_id_builder = results.get().init_command_block_id();
                    set_block_id_builder(&mut block_id_builder, &user_block_id);
                }

                Ok(())
            }
            .instrument(trace_span),
        )
    }

    fn clear_input(
        self: Rc<Self>,
        params: kernel::ClearInputParams,
        _results: kernel::ClearInputResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "clear_input").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        log::info!("clear_input: context={}", context_id);

        match self.kernel.documents.clear_input(context_id) {
            Ok(_text) => Promise::ok(()),
            Err(e) => Promise::err(capnp::Error::failed(format!("clear_input failed: {}", e))),
        }
    }

    fn search_similar(
        self: Rc<Self>,
        params: kernel::SearchSimilarParams,
        mut results: kernel::SearchSimilarResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "search_similar");
        let query = pry!(pry!(p.get_query()).to_str()).to_string();
        let k = p.get_k() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let search_results = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        let q = query.clone();
                        tokio::task::spawn_blocking(move || idx.search(&q, k))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("search: {}", e)))?
                    }
                    None => vec![],
                };

                // Populate labels from drift router
                let drift = kernel.kernel.drift().read();

                let mut list = results.get().init_results(search_results.len() as u32);
                for (i, r) in search_results.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_context_id(r.context_id.as_bytes());
                    entry.set_score(r.score);
                    if let Some(handle) = drift.get(r.context_id)
                        && let Some(ref label) = handle.label
                    {
                        entry.set_label(label);
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_neighbors(
        self: Rc<Self>,
        params: kernel::GetNeighborsParams,
        mut results: kernel::GetNeighborsResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "get_neighbors");
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let k = p.get_k() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let search_results = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        tokio::task::spawn_blocking(move || idx.neighbors(context_id, k))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("neighbors: {}", e)))?
                    }
                    None => vec![],
                };

                let drift = kernel.kernel.drift().read();

                let mut list = results.get().init_results(search_results.len() as u32);
                for (i, r) in search_results.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_context_id(r.context_id.as_bytes());
                    entry.set_score(r.score);
                    if let Some(handle) = drift.get(r.context_id)
                        && let Some(ref label) = handle.label
                    {
                        entry.set_label(label);
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_clusters(
        self: Rc<Self>,
        params: kernel::GetClustersParams,
        mut results: kernel::GetClustersResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let span = extract_rpc_trace(p.get_trace(), "get_clusters");
        let min_cluster_size = p.get_min_cluster_size() as usize;
        let kernel = self.kernel.clone();

        Promise::from_future(
            async move {
                let clusters = match &kernel.semantic_index {
                    Some(idx) => {
                        let idx = idx.clone();
                        tokio::task::spawn_blocking(move || idx.clusters(min_cluster_size))
                            .await
                            .map_err(|e| capnp::Error::failed(format!("spawn_blocking: {}", e)))?
                            .map_err(|e| capnp::Error::failed(format!("clusters: {}", e)))?
                    }
                    None => vec![],
                };

                let mut list = results.get().init_clusters(clusters.len() as u32);
                for (i, c) in clusters.iter().enumerate() {
                    let mut entry = list.reborrow().get(i as u32);
                    entry.set_cluster_id(c.cluster_id as u32);
                    if let Some(ref label) = c.label {
                        entry.set_label(label);
                    }
                    let mut ids = entry.init_context_ids(c.context_ids.len() as u32);
                    for (j, ctx_id) in c.context_ids.iter().enumerate() {
                        ids.set(j as u32, ctx_id.as_bytes());
                    }
                }
                Ok(())
            }
            .instrument(span),
        )
    }

    fn get_blocks(
        self: Rc<Self>,
        params: kernel::GetBlocksParams,
        mut results: kernel::GetBlocksResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_blocks").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let query_reader = pry!(p.get_query());
        let query = pry!(parse_block_query(&query_reader));

        let documents = &self.kernel.documents;

        let blocks = match query {
            kaijutsu_types::BlockQuery::All => {
                pry!(
                    documents
                        .block_snapshots(context_id)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
            kaijutsu_types::BlockQuery::ByIds(ids) => {
                if ids.is_empty() {
                    return Promise::err(capnp::Error::failed(
                        "byIds requires at least one block ID".into(),
                    ));
                }
                pry!(
                    documents
                        .get_blocks_by_ids(context_id, &ids)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
            kaijutsu_types::BlockQuery::ByFilter(filter) => {
                pry!(
                    filter
                        .validate()
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                );
                pry!(
                    documents
                        .query_blocks(context_id, &filter)
                        .map_err(|e| capnp::Error::failed(e.to_string()))
                )
            }
        };

        let mut block_list = results.get().init_blocks(blocks.len() as u32);
        for (i, block) in blocks.iter().enumerate() {
            let mut block_builder = block_list.reborrow().get(i as u32);
            set_block_snapshot(&mut block_builder, block);
        }

        Promise::ok(())
    }

    fn get_context_sync(
        self: Rc<Self>,
        params: kernel::GetContextSyncParams,
        mut results: kernel::GetContextSyncResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "get_context_sync").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let documents = &self.kernel.documents;
        let (ops, version) = pry!(
            documents
                .context_sync_state(context_id)
                .map_err(|e| capnp::Error::failed(e.to_string()))
        );

        let mut r = results.get();
        r.set_context_id(context_id.as_bytes());
        r.set_ops(&ops);
        r.set_version(version);

        Promise::ok(())
    }

    fn subscribe_blocks_filtered(
        self: Rc<Self>,
        params: kernel::SubscribeBlocksFilteredParams,
        _results: kernel::SubscribeBlocksFilteredResults,
    ) -> Promise<(), capnp::Error> {
        let _span = tracing::info_span!("rpc", method = "subscribe_blocks_filtered").entered();
        let p = pry!(params.get());
        let callback = pry!(p.get_callback());

        // Client-supplied instance UUID. Together with the principal it forms
        // the dedupe key — a reconnect from the same client (same instance)
        // replaces the prior live subscription instead of stacking. Older
        // clients that don't set it pass an empty string; those still benefit
        // from dedupe per-principal but two distinct old clients of the same
        // principal will trample each other. That's an acceptable degradation
        // — the alternative (no dedupe for empty instance) leaks tasks again.
        let instance = pry!(pry!(p.get_instance()).to_str()).to_owned();

        // Parse the BlockEventFilter from the capnp struct
        let filter = if p.has_filter() {
            let f = pry!(p.get_filter());
            parse_block_event_filter(f)
        } else {
            kaijutsu_types::BlockEventFilter::default()
        };

        let has_filter = filter.has_active_constraint();

        // Which FlowBus topics to subscribe to, given the event_types filter.
        // Must stay a superset of what `matches_filter` can pass — directives
        // (RenderCue) always pass, so the narrow single-topic partition is only
        // safe for a lone directive filter (see `filtered_subscribe_pattern`).
        let subscribe_pattern = filtered_subscribe_pattern(&filter.event_types);

        // Reverse direction on the same callback: see `subscribe_blocks`.
        {
            let registry = self.kernel.kernel.midi_exchange().clone();
            let connection = {
                let mut conn = self.connection.borrow_mut();
                conn.attach_midi_exchange(registry.clone());
                conn.session_id
            };
            let conn_cancel = self.connection.borrow().cancel_token();
            spawn_midi_exchange_bridge(registry, connection, callback.clone(), conn_cancel);
        }

        {
            let block_flows = self.kernel.kernel.block_flows().clone();
            let input_flows = self.kernel.documents.input_flows().cloned();
            let kernel_id = self.kernel.id;
            let (conn_cancel, disconnect, caps, principal_id, session_id) = {
                let conn = self.connection.borrow();
                (
                    conn.cancel_token(),
                    conn.disconnect_token(),
                    conn.event_capabilities(),
                    conn.principal.id,
                    conn.session_id,
                )
            };
            let registry = self.kernel.subscription_registry.clone();
            let dedupe_key = (principal_id, instance.clone());
            let wire_filter = if has_filter { Some(filter) } else { None };

            let task = tokio::task::spawn_local(async move {
                let block_sub = block_flows.subscribe(subscribe_pattern);
                let input_sub = input_flows.map(|f| f.subscribe("input.*"));
                log::debug!(
                    "Started filtered FlowBus subscription for kernel {} (filter_active={}, \
                     pattern={}, batch={}, kick={})",
                    kernel_id.to_hex(),
                    has_filter,
                    subscribe_pattern,
                    caps.text_ops_batch,
                    caps.subscription_terminated,
                );
                run_block_bridge(
                    callback,
                    block_sub,
                    input_sub,
                    wire_filter,
                    caps,
                    kernel_id,
                    conn_cancel,
                    disconnect,
                    "Filtered FlowBus",
                )
                .await;
            });

            // Register the AbortHandle. If a prior subscription exists for
            // this (principal, instance) — typically a reconnect after the
            // previous TCP connection died silently — abort it before
            // dropping it from the map. The aborted task's `tokio::select!`
            // wakes via the cancellation path and unwinds without delivering
            // any more events; the dead callback held by that task is dropped
            // with the task frame.
            //
            // We do NOT remove our own entry on natural exit (when conn_cancel
            // fires from the connection's Drop). The cost of leaving a
            // finished AbortHandle in the map is O(small struct) per distinct
            // (principal, instance) ever seen; correctness wins over a tiny
            // bounded leak.
            let new_handle = task.abort_handle();
            match register_subscription(&registry, dedupe_key.clone(), session_id, new_handle) {
                SubscriptionRegistration::Fresh | SubscriptionRegistration::ReplacedFinished => {}
                // Same connection re-issuing the RPC (e.g. a changed filter)
                // — ordinary, not worth an operator's attention.
                SubscriptionRegistration::ReplacedSameConnection => {
                    log::debug!(
                        "Replacing this connection's own FlowBus subscription for \
                         kernel {} instance={} principal={:?} session={}",
                        kernel_id,
                        dedupe_key.1,
                        dedupe_key.0,
                        session_id.short(),
                    );
                }
                // A different connection claimed this (principal, instance)
                // slot. Could be a legitimate reconnect (same logical client,
                // new SSH session) or two live clients sharing one hardcoded
                // `instance` literal silently stealing the block-event bridge
                // from each other (docs/issues.md, "MCP shell delay") — we
                // can't fully tell those apart from here, so surface both the
                // replaced session and its age: a young age with both
                // connections still live reads as theft, a multi-second/
                // minute age reads as an ordinary reconnect.
                SubscriptionRegistration::ReplacedDifferentConnection {
                    prior_session_id,
                    age,
                } => {
                    tracing::warn!(
                        kernel = %kernel_id.to_hex(),
                        principal = ?dedupe_key.0,
                        instance = %dedupe_key.1,
                        prior_session = %prior_session_id.short(),
                        new_session = %session_id.short(),
                        prior_age_secs = age.as_secs_f64(),
                        "Replacing live FlowBus subscription registered by a \
                         different connection — reconnect if this instance id \
                         is genuinely one client, or two live clients \
                         trampling a shared instance id if not",
                    );
                }
            }
        }
        Promise::ok(())
    }

    // =========================================================================
    // Context Interrupt
    // =========================================================================

    fn interrupt_context(
        self: Rc<Self>,
        params: kernel::InterruptContextParams,
        mut results: kernel::InterruptContextResults,
    ) -> Promise<(), capnp::Error> {
        let params_reader = pry!(params.get());
        let context_id_bytes = pry!(params_reader.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let immediate = params_reader.get_immediate();

        // Hard interrupt: kill this connection's in-flight kaish command(s).
        // The per-use shell holds no persistent handle, so we cancel via the
        // running-execution registry — each `execute` spawns with a token whose
        // `select!` arm calls `kaish.cancel()`. Gated on the target context
        // matching the one this connection is driving; the old per-connection
        // cancel fired regardless of which context was targeted.
        if immediate {
            let conn = self.connection.borrow();
            if conn.require_context().ok() == Some(context_id) {
                conn.cancel_running_executions();
            }
        }

        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let success = if let Some(interrupt) = kernel.get_interrupt(context_id).await {
                if immediate {
                    interrupt.hard();
                } else {
                    interrupt.soft();
                }
                true
            } else {
                // No active stream for this context — no-op (idempotent).
                false
            };

            log::info!(
                "interruptContext: context={}, immediate={}, success={}",
                context_id,
                immediate,
                success
            );

            results.get().set_success(success);
            Ok(())
        })
    }

    fn list_presets(
        self: Rc<Self>,
        params: kernel::ListPresetsParams,
        mut results: kernel::ListPresetsResults,
    ) -> Promise<(), capnp::Error> {
        let _span = extract_rpc_trace(pry!(params.get()).get_trace(), "list_presets");
        let _kernel_id = self.kernel.id;

        let (presets, cast_labels) = {
            let db = self.kernel.kernel_db.lock();
            let presets = db.list_presets().unwrap_or_default();
            // Resolved once for the whole listing (Track D) — `provider`/
            // `model` (@3/@4) are demolished; `castLabel` (@5) replaces them.
            let cast_labels: HashMap<kaijutsu_types::CastId, String> = db
                .list_casts()
                .map(|casts| casts.into_iter().map(|c| (c.cast_id, c.label)).collect())
                .unwrap_or_default();
            (presets, cast_labels)
        };

        let mut list = results.get().init_presets(presets.len() as u32);
        for (i, preset) in presets.iter().enumerate() {
            let mut p = list.reborrow().get(i as u32);
            p.set_id(preset.preset_id.as_bytes());
            p.set_label(&preset.label);
            p.set_description(preset.description.as_deref().unwrap_or(""));
            // @3/@4 (provider/model) are UNUSED since Track D — left at their
            // wire default (empty), never populated.
            p.set_cast_label(
                preset
                    .cast_id
                    .and_then(|id| cast_labels.get(&id))
                    .map(String::as_str)
                    .unwrap_or(""),
            );
        }

        Promise::ok(())
    }

    fn set_context_state(
        self: Rc<Self>,
        params: kernel::SetContextStateParams,
        mut results: kernel::SetContextStateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "set_context_state").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let state_str = pry!(pry!(p.get_state()).to_str());
        let new_state = match kaijutsu_types::ContextState::from_str(state_str) {
            Ok(s) => s,
            Err(_) => {
                results.get().set_success(false);
                results
                    .get()
                    .set_error(format!("unknown state '{state_str}'"));
                return Promise::ok(());
            }
        };

        // Validate transition: only Staging → Live allowed in v1
        let drift_router = self.kernel.kernel.drift().clone();
        {
            let drift = match drift_router.try_read() {
                Some(d) => d,
                None => {
                    results.get().set_success(false);
                    results.get().set_error("drift router busy");
                    return Promise::ok(());
                }
            };
            if let Some(current) = drift.context_state(context_id) {
                use kaijutsu_types::ContextState::*;
                match (current, new_state) {
                    (Staging, Live) => {} // allowed
                    (same, target) if same == target => {
                        results.get().set_success(true);
                        return Promise::ok(());
                    }
                    (from, to) => {
                        results.get().set_success(false);
                        results
                            .get()
                            .set_error(format!("transition {from} → {to} not allowed"));
                        return Promise::ok(());
                    }
                }
            } else {
                results.get().set_success(false);
                results.get().set_error("context not found in drift router");
                return Promise::ok(());
            }
        }

        // Update DriftRouter
        {
            let mut drift = match drift_router.try_write() {
                Some(d) => d,
                None => {
                    results.get().set_success(false);
                    results.get().set_error("drift router busy (write)");
                    return Promise::ok(());
                }
            };
            if let Err(e) = drift.set_state(context_id, new_state) {
                results.get().set_success(false);
                results.get().set_error(e.to_string());
                return Promise::ok(());
            }
        }

        // Update KernelDb
        {
            let db = self.kernel.kernel_db.lock();
            if let Err(e) = db.update_context_state(context_id, new_state) {
                log::error!("set_context_state: KernelDb update failed: {e}");
                // DriftRouter was already updated — log but don't fail
            }
        }

        log::info!(
            "set_context_state: context={} state={}",
            context_id.short(),
            new_state
        );
        results.get().set_success(true);
        Promise::ok(())
    }

    fn conclude(
        self: Rc<Self>,
        params: kernel::ConcludeParams,
        mut results: kernel::ConcludeResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "conclude").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let drift_router = self.kernel.kernel.drift().clone();

        // Inspect current state for idempotency + guards.
        {
            let drift = match drift_router.try_read() {
                Some(d) => d,
                None => {
                    results.get().set_success(false);
                    results.get().set_error("drift router busy");
                    return Promise::ok(());
                }
            };
            use kaijutsu_types::ContextState::*;
            match drift.context_state(context_id) {
                None => {
                    results.get().set_success(false);
                    results.get().set_error("context not found");
                    return Promise::ok(());
                }
                // Idempotent: re-concluding a concluded context succeeds without
                // restamping `concluded_at`.
                Some(Concluded) => {
                    results.get().set_success(true);
                    return Promise::ok(());
                }
                Some(Archived) => {
                    results.get().set_success(false);
                    results
                        .get()
                        .set_error("cannot conclude an archived context");
                    return Promise::ok(());
                }
                Some(Live) | Some(Staging) => {}
            }
        }

        // Persist first — KernelDb holds the authoritative `concluded_at` stamp.
        {
            let db = self.kernel.kernel_db.lock();
            match db.conclude_context(context_id) {
                Ok(true) => {}
                Ok(false) => {
                    results.get().set_success(false);
                    results.get().set_error(
                        "context could not be concluded (already concluded, archived, or unknown)",
                    );
                    return Promise::ok(());
                }
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(e.to_string());
                    return Promise::ok(());
                }
            }
        }

        // Reflect in the DriftRouter so `listContexts` shows the new state
        // immediately. If the router is busy, the DB is already authoritative and
        // `concluded_at` drives the client's banding regardless — report success.
        match drift_router.try_write() {
            Some(mut drift) => {
                if let Err(e) = drift.set_state(context_id, kaijutsu_types::ContextState::Concluded)
                {
                    log::error!("conclude: drift set_state failed: {e}");
                }
            }
            None => {
                log::warn!("conclude: drift router busy (write); DB concluded, router state lags");
            }
        }

        log::info!("conclude: context={}", context_id.short());
        results.get().set_success(true);
        Promise::ok(())
    }

    fn rename_context(
        self: Rc<Self>,
        params: kernel::RenameContextParams,
        _results: kernel::RenameContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let label = pry!(pry!(p.get_label()).to_str()).to_owned();

        // DB is authoritative. A taken label surfaces as the UNIQUE-violation
        // error rather than silently stealing it — label theft is `kj context
        // retag`'s explicitly-confirmed job, not this RPC's.
        {
            let db = self.kernel.kernel_db.lock();
            if let Err(e) = db.update_label(context_id, Some(&label)) {
                return Promise::err(capnp::Error::failed(format!("rename_context: {e}")));
            }
        }

        // Keep the live label index in step. try_write like conclude/promote:
        // the DB is authoritative and a busy router only lags — never block
        // the single-threaded RPC loop on it.
        match self.kernel.kernel.drift().try_write() {
            Some(mut drift) => {
                if let Err(e) = drift.rename(context_id, Some(&label)) {
                    log::error!("rename_context: drift rename failed: {e}");
                }
            }
            None => {
                log::warn!(
                    "rename_context: drift router busy (write); DB renamed, router label lags"
                );
            }
        }

        log::info!("rename_context: context={} label={label}", context_id.short());
        Promise::ok(())
    }

    fn promote_context(
        self: Rc<Self>,
        params: kernel::PromoteContextParams,
        mut results: kernel::PromoteContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "promote_context").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        // No drift-router existence pre-check: the DB is authoritative (it
        // errors NotFound for unknown ids), and an archived context must be
        // promotable — resurrection — whether or not the router remembers it.
        let outcome = {
            let db = self.kernel.kernel_db.lock();
            match db.promote_context(context_id) {
                Ok(outcome) => outcome,
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(e.to_string());
                    return Promise::ok(());
                }
            }
        };

        // Resurrection reverses archive's DriftRouter sync: without this the
        // router still holds Archived and sessions can't touch the context.
        // Conclusion is orthogonal to placement — a resurrected concluded
        // context comes back Concluded, not Live. try_write like `conclude`:
        // the DB is authoritative and a busy router only lags, so never
        // block the single-threaded RPC loop on it.
        if let PromoteOutcome::Resurrected { concluded } = outcome {
            let state = if concluded {
                kaijutsu_types::ContextState::Concluded
            } else {
                kaijutsu_types::ContextState::Live
            };
            match self.kernel.kernel.drift().try_write() {
                Some(mut drift) => {
                    if let Err(e) = drift.set_state(context_id, state) {
                        log::error!("promote_context: drift set_state failed: {e}");
                    }
                }
                None => {
                    log::warn!(
                        "promote_context: drift router busy (write); DB resurrected, router state lags"
                    );
                }
            }
        }

        log::info!(
            "promote_context: context={} outcome={:?}",
            context_id.short(),
            outcome
        );
        results.get().set_success(true);
        Promise::ok(())
    }

    fn demote_context(
        self: Rc<Self>,
        params: kernel::DemoteContextParams,
        mut results: kernel::DemoteContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "demote_context").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let outcome = {
            let db = self.kernel.kernel_db.lock();
            match db.demote_context(context_id) {
                Ok(outcome) => outcome,
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(e.to_string());
                    return Promise::ok(());
                }
            }
        };

        // Only the ladder's terminal step (archive) touches lifecycle state
        // the drift router tracks; unpromote/demote are ring placement only.
        // try_write like `conclude`: the DB is authoritative and a busy
        // router only lags, so never block the single-threaded RPC loop.
        if outcome == DemoteOutcome::Archived {
            match self.kernel.kernel.drift().try_write() {
                Some(mut drift) => {
                    if let Err(e) =
                        drift.set_state(context_id, kaijutsu_types::ContextState::Archived)
                    {
                        log::error!("demote_context: drift set_state failed: {e}");
                    }
                }
                None => {
                    log::warn!(
                        "demote_context: drift router busy (write); DB archived, router state lags"
                    );
                }
            }
        }

        log::info!(
            "demote_context: context={} outcome={:?}",
            context_id.short(),
            outcome
        );
        results.get().set_success(true);
        Promise::ok(())
    }

    fn set_context_paused(
        self: Rc<Self>,
        params: kernel::SetContextPausedParams,
        mut results: kernel::SetContextPausedResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "set_context_paused").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let paused = p.get_paused();

        let db = self.kernel.kernel_db.lock();
        match db.set_context_paused(context_id, paused) {
            Ok(()) => {
                log::info!(
                    "set_context_paused: context={} paused={}",
                    context_id.short(),
                    paused
                );
                results.get().set_success(true);
            }
            Err(e) => {
                results.get().set_success(false);
                results.get().set_error(e.to_string());
            }
        }
        Promise::ok(())
    }

    /// Archive a single context — the well's single-keystroke archive
    /// action. Unlike the `kj context archive` builtin (latched, recurses
    /// into structural children), this is single-context, not latched, no
    /// subtree recursion. Idempotent: archiving an already-archived context
    /// succeeds (the DB write's `archived_at IS NULL` guard is already a
    /// no-op).
    fn archive_context(
        self: Rc<Self>,
        params: kernel::ArchiveContextParams,
        mut results: kernel::ArchiveContextResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "archive_context").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        // No drift-router existence pre-check (same stance as
        // promote_context): the DB is authoritative. Its WHERE guard makes
        // re-archiving a no-op `Ok(false)` — distinguish that idempotent
        // success from an unknown id, which must still fail loud.
        {
            let db = self.kernel.kernel_db.lock();
            match db.archive_context(context_id) {
                Ok(true) => {}
                Ok(false) => match db.get_context(context_id) {
                    Ok(Some(_)) => {} // already archived — idempotent success
                    Ok(None) => {
                        results.get().set_success(false);
                        results.get().set_error("context not found");
                        return Promise::ok(());
                    }
                    Err(e) => {
                        results.get().set_success(false);
                        results.get().set_error(e.to_string());
                        return Promise::ok(());
                    }
                },
                Err(e) => {
                    results.get().set_success(false);
                    results.get().set_error(e.to_string());
                    return Promise::ok(());
                }
            }
        }

        // Sync the in-memory drift router (M2-B3 pattern): without this an
        // active session could still resurrect the context with the next op.
        // try_write like `conclude` — the DB is authoritative, never block
        // the single-threaded RPC loop on a busy router.
        match self.kernel.kernel.drift().try_write() {
            Some(mut drift) => {
                if let Err(e) = drift.set_state(context_id, kaijutsu_types::ContextState::Archived)
                {
                    log::error!("archive_context: drift set_state failed: {e}");
                }
            }
            None => {
                log::warn!(
                    "archive_context: drift router busy (write); DB archived, router state lags"
                );
            }
        }

        log::info!("archive_context: context={}", context_id.short());
        results.get().set_success(true);
        Promise::ok(())
    }

    fn commit_capture(
        self: Rc<Self>,
        params: kernel::CommitCaptureParams,
        mut results: kernel::CommitCaptureResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "commit_capture").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let mime = pry!(pry!(p.get_mime()).to_str()).to_string();
        let cas_hash = pry!(pry!(p.get_cas_hash()).to_str()).to_string();
        let payload = pry!(p.get_payload()).to_vec();
        // played_by = the authenticated caller shipping the batch (a probe
        // context never produces via turns, so the attachment carries no
        // producer principal to borrow).
        let played_by = self.connection.borrow().principal.id;
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            // Same facade gate as the other context-injecting verbs (shell /
            // edit_input / submit_input): deny-by-default, granted by the
            // loadout. Broad seats carry `facade:*` (lib binding) so the
            // ambient ear works from any coder/default/mcp context; narrow
            // seats (musician, toolie) stay capture-mute by construction.
            kernel
                .kernel
                .broker()
                .check_facade(&context_id, "commit_capture")
                .await
                .map_err(|e| capnp::Error::failed(format!("commitCapture denied: {e}")))?;

            if !cas_hash.is_empty() {
                return Err(capnp::Error::failed(
                    "commitCapture: CAS payloads are not built yet (docs/issues.md \
                     'CAS write surface') — send the batch inline"
                        .into(),
                ));
            }
            if mime != kaijutsu_audio::MIDI_CAPTURE_MIME {
                return Err(capnp::Error::failed(format!(
                    "commitCapture: unsupported mime {mime} (this build accepts {})",
                    kaijutsu_audio::MIDI_CAPTURE_MIME
                )));
            }
            let rx = kernel
                .kernel
                .send_capture_commit(context_id, payload, played_by)
                .ok_or_else(|| capnp::Error::failed("no beat scheduler is running".into()))?;
            let block_id = rx
                .await
                .map_err(|_| {
                    capnp::Error::failed("beat scheduler dropped the capture commit".into())
                })?
                .map_err(capnp::Error::failed)?;

            log::info!(
                "commit_capture: context={} block={}",
                context_id.short(),
                block_id
            );
            let mut b = results.get().init_block_id();
            set_block_id_builder(&mut b, &block_id);
            Ok(())
        })
    }

    fn report_clock_estimate(
        self: Rc<Self>,
        params: kernel::ReportClockEstimateParams,
        _results: kernel::ReportClockEstimateResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "report_clock_estimate").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let beat = p.get_beat();
        let tempo_bps = p.get_tempo_bps();
        let epoch_ns = p.get_epoch_ns();
        let source = pry!(pry!(p.get_source()).to_str()).to_string();

        // No facade gate, on purpose (contrast commit_capture): an estimate
        // is inert sensor data unless the track was *slaved*, and the
        // authority moment is that slaving — `kj transport clock`, gated on
        // the Transport capability. Gate the decision, not the sensor.
        //
        // Fire-and-forget into the beat scheduler (a ~2 Hz reference stream;
        // the scheduler decides whether the sender's track is slaved). The
        // only wire error is "no scheduler at all" — a dropped single
        // reference is jitter the model rides out, but a missing scheduler
        // means every reference is going nowhere, which the observer should
        // hear about once per connection rather than silently.
        if !self.kernel.kernel.send_clock_estimate(context_id, beat, tempo_bps, epoch_ns, source)
        {
            return Promise::err(capnp::Error::failed(
                "no beat scheduler is running".into(),
            ));
        }
        Promise::ok(())
    }

    fn report_midi_presence(
        self: Rc<Self>,
        params: kernel::ReportMidiPresenceParams,
        _results: kernel::ReportMidiPresenceResults,
    ) -> Promise<(), capnp::Error> {
        use kaijutsu_kernel::midi_presence::{
            MidiPortFact, MidiPresenceRecord, Recorded, SinkAttribution,
        };

        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "report_midi_presence").entered();
        let device = pry!(pry!(p.get_device()).to_str()).to_string();
        let present = p.get_present();
        let backend = pry!(pry!(p.get_backend()).to_str()).to_string();
        let epoch_ns = p.get_epoch_ns();
        // Display/provenance only — whatever the sink calls its machine (empty
        // from a peer too old to say). The reaping key is the connection
        // below, which the sink neither chooses nor can take with it.
        let sink_host = pry!(pry!(p.get_sink_host()).to_str()).to_string();
        let mut ports: Vec<MidiPortFact> = Vec::new();
        for port in pry!(p.get_ports()).iter() {
            ports.push(MidiPortFact {
                name: pry!(pry!(port.get_name()).to_str()).to_string(),
                address: pry!(pry!(port.get_address()).to_str()).to_string(),
            });
        }

        // No facade gate, on purpose (the report_clock_estimate stance):
        // presence is inert sensor data about the rig, not a context
        // injection, and every player is inside the trust boundary.
        //
        // Attribution is stamped HERE, from the connection, never from
        // anything the sink says about itself: this is what makes the record
        // perishable when the sink dies without an unplug report
        // (`ConnectionState::drop` → `reap_connection`).
        let store = self.kernel.kernel.midi_presence().clone();
        let connection = {
            let mut conn = self.connection.borrow_mut();
            conn.attach_midi_presence(store.clone());
            conn.session_id
        };
        let record = MidiPresenceRecord::from_sink(
            &device,
            present,
            &backend,
            ports,
            epoch_ns,
            SinkAttribution::new(connection, sink_host),
        );
        match store.record(record) {
            // A malformed device key would mint an unaddressable /run/midi
            // path — refuse loudly rather than store a fiction.
            Err(e) => Promise::err(capnp::Error::failed(format!(
                "reportMidiPresence: {e}"
            ))),
            Ok(Recorded::StaleDropped) => {
                log::debug!(
                    "report_midi_presence: dropped stale report for {device} \
                     (at={epoch_ns} is older than the record on file)"
                );
                Promise::ok(())
            }
            Ok(Recorded::Stored) => {
                log::info!(
                    "report_midi_presence: {device} {} via {backend} on \
                     connection {}",
                    if present { "live" } else { "absent" },
                    connection.short()
                );
                Promise::ok(())
            }
        }
    }

    fn set_block_excluded(
        self: Rc<Self>,
        params: kernel::SetBlockExcludedParams,
        mut results: kernel::SetBlockExcludedResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "set_block_excluded").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let block_id_reader = pry!(p.get_block_id());
        let block_id = pry!(parse_block_id_from_reader(&block_id_reader));
        let excluded = p.get_excluded();

        // Enforce: excluded toggling allowed in Live or Staging state
        {
            let drift = match self.kernel.kernel.drift().try_read() {
                Some(d) => d,
                None => {
                    return Promise::err(capnp::Error::failed("drift router busy".into()));
                }
            };
            match drift.context_state(context_id) {
                Some(kaijutsu_types::ContextState::Live)
                | Some(kaijutsu_types::ContextState::Staging) => {} // allowed
                Some(state) => {
                    return Promise::err(capnp::Error::failed(format!(
                        "cannot toggle excluded in {state} state (only live/staging)"
                    )));
                }
                None => {
                    return Promise::err(capnp::Error::failed("context not found".into()));
                }
            }
        }

        if let Err(e) = self
            .kernel
            .documents
            .set_excluded(context_id, &block_id, excluded)
        {
            return Promise::err(capnp::Error::failed(e.to_string()));
        }

        match self.kernel.documents.version(context_id) {
            Ok(ack) => {
                results.get().set_ack_version(ack);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(e.to_string())),
        }
    }

    fn list_dead_letters(
        self: Rc<Self>,
        params: kernel::ListDeadLettersParams,
        mut results: kernel::ListDeadLettersResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "list_dead_letters").entered();
        let kernel = self.kernel.kernel.clone();
        Promise::from_future(async move {
            let drift = kernel.drift().read();
            let items = drift.dead_letters().to_vec();
            drop(drift);
            let count = items.len() as u32;
            let mut out = results.get().init_items(count);
            for (i, dl) in items.iter().enumerate() {
                let mut row = out.reborrow().get(i as u32);
                row.set_id(dl.id);
                row.set_source_ctx(dl.source_ctx.as_bytes());
                row.set_target_ctx(dl.target_ctx.as_bytes());
                row.set_content(&dl.content);
                if let Some(ref m) = dl.source_model {
                    row.set_source_model(m);
                    row.set_has_source_model(true);
                } else {
                    row.set_source_model("");
                    row.set_has_source_model(false);
                }
                row.set_drift_kind(dl.drift_kind.as_str());
                row.set_created_at(dl.created_at);
                row.set_retry_count(dl.retry_count);
            }
            Ok(())
        })
    }

    fn replay_dead_letter(
        self: Rc<Self>,
        params: kernel::ReplayDeadLetterParams,
        mut results: kernel::ReplayDeadLetterResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "replay_dead_letter").entered();
        let id = p.get_id();
        let kernel = self.kernel.kernel.clone();
        Promise::from_future(async move {
            let mut drift = kernel.drift().write();
            let replayed = drift.replay_dead_letter(id).is_some();
            results.get().set_replayed(replayed);
            Ok(())
        })
    }

    fn context_leave(
        self: Rc<Self>,
        params: kernel::ContextLeaveParams,
        mut results: kernel::ContextLeaveResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "context_leave").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );

        let connection = self.connection.clone();
        let session_id = connection.borrow().session_id;
        // Drop the binding only if it points at the context we're leaving —
        // never silently leave a different context the session also held.
        let mut left = false;
        let map = connection.borrow().session_contexts.clone();
        map.remove_if(&session_id, |_, ctx| {
            let matches = *ctx == context_id;
            if matches {
                left = true;
            }
            matches
        });
        results.get().set_left(left);
        Promise::ok(())
    }

    fn move_block(
        self: Rc<Self>,
        params: kernel::MoveBlockParams,
        mut results: kernel::MoveBlockResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _trace_guard = extract_rpc_trace(p.get_trace(), "move_block").entered();
        let context_id_bytes = pry!(p.get_context_id());
        let context_id = pry!(
            ContextId::try_from_slice(context_id_bytes)
                .ok_or_else(|| capnp::Error::failed("invalid context ID".into()))
        );
        let block_id_reader = pry!(p.get_block_id());
        let block_id = pry!(parse_block_id_from_reader(&block_id_reader));
        let after_id = if p.get_has_after() {
            let after_reader = pry!(p.get_after());
            Some(pry!(parse_block_id_from_reader(&after_reader)))
        } else {
            None
        };

        if let Err(e) = self
            .kernel
            .documents
            .move_block(context_id, &block_id, after_id.as_ref())
        {
            return Promise::err(capnp::Error::failed(e.to_string()));
        }

        match self.kernel.documents.version(context_id) {
            Ok(ack) => {
                results.get().set_ack_version(ack);
                Promise::ok(())
            }
            Err(e) => Promise::err(capnp::Error::failed(e.to_string())),
        }
    }

    /// Cheap liveness probe. Returns the kernel ID and wall-clock time.
    ///
    /// Used by the client's reconnect FSM to detect a wedged RPC system: if
    /// `ping` doesn't return within the client's per-ping deadline, the
    /// client transitions to `Closing` and reconnects. The handler must not
    /// touch any per-context lock, hit the database, or otherwise do work
    /// that could itself wedge — its purpose is purely to confirm that the
    /// RPC pipe is being drained and the kernel can produce a reply.
    fn ping(
        self: Rc<Self>,
        params: kernel::PingParams,
        mut results: kernel::PingResults,
    ) -> Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let _span = extract_rpc_trace(p.get_trace(), "ping").entered();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut r = results.get();
        r.set_kernel_id(self.kernel.id.as_bytes());
        r.set_server_time_ms(now_ms);
        Promise::ok(())
    }

}

// ============================================================================
// Shell Value Conversion Helpers
// ============================================================================

/// Convert kaish `ast::Value` → Cap'n Proto `ShellValue` builder.
fn value_to_shell_value(mut builder: shell_value::Builder<'_>, value: &kaish_kernel::ast::Value) {
    use kaish_kernel::ast::Value;
    match value {
        Value::Null => builder.set_null(()),
        Value::Bool(b) => builder.set_bool(*b),
        Value::Int(i) => builder.set_int(*i),
        Value::Float(f) => builder.set_float(*f),
        Value::String(s) => builder.set_string(s),
        Value::Json(j) => builder.set_json(serde_json::to_string(j).unwrap_or_default()),
        Value::Bytes(b) => builder.set_bytes(b),
    }
}

/// Convert Cap'n Proto `ShellValue` reader → kaish `ast::Value`.
fn shell_value_to_value(
    reader: shell_value::Reader<'_>,
) -> Result<kaish_kernel::ast::Value, capnp::Error> {
    use kaish_kernel::ast::Value;
    match reader.which()? {
        shell_value::Null(()) => Ok(Value::Null),
        shell_value::Bool(b) => Ok(Value::Bool(b)),
        shell_value::Int(i) => Ok(Value::Int(i)),
        shell_value::Float(f) => Ok(Value::Float(f)),
        shell_value::String(s) => Ok(Value::String(s?.to_str()?.to_owned())),
        shell_value::Json(j) => {
            let json_str = j?.to_str()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str)
                .map_err(|e| capnp::Error::failed(format!("invalid JSON: {}", e)))?;
            Ok(Value::Json(parsed))
        }
        shell_value::Bytes(b) => Ok(Value::Bytes(b?.to_vec())),
        // LEGACY: kaish 0.9 dropped Value::Blob, so this only arrives from an
        // older peer. The payload is a path string, not bytes — surface it as a
        // String rather than mis-decoding it as binary.
        shell_value::Blob(b) => Ok(Value::String(b?.to_str()?.to_owned())),
    }
}

// ============================================================================
// OutputData Build Helpers
// ============================================================================

fn entry_type_to_capnp(et: kaijutsu_types::OutputEntryType) -> crate::kaijutsu_capnp::EntryType {
    use crate::kaijutsu_capnp::EntryType;
    use kaijutsu_types::OutputEntryType;
    match et {
        OutputEntryType::Text => EntryType::Text,
        OutputEntryType::File => EntryType::File,
        OutputEntryType::Directory => EntryType::Directory,
        OutputEntryType::Executable => EntryType::Executable,
        OutputEntryType::Symlink => EntryType::Symlink,
        _ => EntryType::Text,
    }
}

fn build_output_node(
    mut builder: crate::kaijutsu_capnp::output_node::Builder<'_>,
    node: &kaijutsu_types::OutputNode,
) {
    builder.set_name(&node.name);
    builder.set_entry_type(entry_type_to_capnp(node.entry_type));
    if let Some(ref text) = node.text {
        builder.set_has_text(true);
        builder.set_text(text);
    }
    if !node.cells.is_empty() {
        let mut cells = builder.reborrow().init_cells(node.cells.len() as u32);
        for (i, cell) in node.cells.iter().enumerate() {
            cells.set(i as u32, cell);
        }
    }
    if !node.children.is_empty() {
        let mut children = builder.reborrow().init_children(node.children.len() as u32);
        for (i, child) in node.children.iter().enumerate() {
            build_output_node(children.reborrow().get(i as u32), child);
        }
    }
}

fn build_output_data(
    mut builder: crate::kaijutsu_capnp::output_data::Builder<'_>,
    data: &kaijutsu_types::OutputData,
) {
    if let Some(ref headers) = data.headers {
        builder.set_has_headers(true);
        let mut hlist = builder.reborrow().init_headers(headers.len() as u32);
        for (i, h) in headers.iter().enumerate() {
            hlist.set(i as u32, h);
        }
    }
    if !data.root.is_empty() {
        let mut root = builder.reborrow().init_root(data.root.len() as u32);
        for (i, node) in data.root.iter().enumerate() {
            build_output_node(root.reborrow().get(i as u32), node);
        }
    }
    if let Some(ref rich) = data.rich_json {
        match serde_json::to_string(rich) {
            Ok(s) => builder.set_rich_json(s.as_str()),
            // Display hint only — never fail the whole block build over a
            // JSON encode error (should be unreachable for serde_json::Value).
            Err(e) => log::error!("build_output_data: rich_json serialize failed: {e}"),
        }
    }
}

/// The `OutputData` to persist on a shell result block.
///
/// Prefer the result's own `.output` (a builtin that set real OutputData),
/// but kaish's output limiter runs `materialize()` on every result that
/// passes through `spill_if_needed` — which unconditionally drops `.output`
/// even when `.out` carries independent text (kaish-types
/// `ExecResult::materialize`, `result.rs`). So for `kj`/most builtins the
/// structured payload survives ONLY on the `.data` sideband. Bridge that
/// into a rich_json `OutputData` so the structured value still reaches the
/// block (→ app + MCP `data`). See `docs/issues.md`.
///
/// The two sidebands are MERGED, not chosen between: a real node-tree
/// `.output` (app-renderable) plus an independent `.data` sideband both
/// survive on the one `OutputData` — the tree in `root`, `.data` back-filled
/// into `rich_json` only when `.output` didn't already set it. Today
/// `.output` is nearly always `None` by the time a result reaches here (the
/// limiter clears it), so this mostly degenerates to the rich_json-only
/// path — but the contract shouldn't rely on that staying broken. Per
/// deepseek's review: this does mean the app (renders the node tree) and MCP
/// (returns `rich_json` verbatim) can show divergent views of the same
/// block when both are set — an accepted caveat, not a bug.
fn block_output_data(
    result: &kaish_kernel::interpreter::ExecResult,
) -> Option<kaijutsu_types::OutputData> {
    let output = result.output().cloned();
    let needs_rich_json = output.as_ref().is_none_or(|od| od.rich_json.is_none());
    if needs_rich_json && let Some(v) = result.data.as_ref() {
        let rich_json = kaish_kernel::interpreter::value_to_json(v);
        return Some(output.unwrap_or_default().with_rich_json(rich_json));
    }
    output
}

#[cfg(test)]
mod block_output_data_tests {
    //! `block_output_data` is the `.data`→block bridge that replaces the
    //! dead-end `.output` write kj_builtin used to attempt (see
    //! `kj_output_channel_does_not_survive_the_kaish_output_limiter` in
    //! kaijutsu-kernel and `docs/issues.md`): kaish's output limiter drops
    //! `.output` before a result like this ever reaches the persistence
    //! seam, so `.data` is the channel that actually carries a kj command's
    //! structured payload out.
    use super::block_output_data;
    use kaish_kernel::ast::Value;
    use kaish_kernel::interpreter::ExecResult;

    #[test]
    fn prefers_real_output_data_when_present() {
        let od = kaijutsu_types::OutputData::new().with_rich_json(serde_json::json!("x"));
        let result = ExecResult::with_output(od.clone());

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.rich_json, od.rich_json,
            "a builtin that set real .output must be used verbatim"
        );
    }

    #[test]
    fn falls_back_to_data_sideband_as_rich_json() {
        let data = Value::Json(serde_json::json!(["bass", "bassline"]));
        let result = ExecResult::success_with_data("bass\nbassline", data);
        assert!(
            result.output().is_none(),
            "test premise: no real .output, only .data"
        );

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.rich_json,
            Some(serde_json::json!(["bass", "bassline"])),
            "rich_json must equal the .data sideband, JSON-converted"
        );
    }

    #[test]
    fn neither_output_nor_data_yields_none() {
        let result = ExecResult::success("plain text, no structure");
        assert!(block_output_data(&result).is_none());
    }

    #[test]
    fn merges_data_sideband_onto_a_real_output_tree() {
        // A builtin can set BOTH a real node-tree `.output` (the
        // app-renderable shape) AND an independent `.data` sideband. The
        // resulting OutputData must carry both — the tree in `root`, `.data`
        // back-filled into `rich_json` — not one clobbering the other.
        let tree = kaijutsu_types::OutputData::nodes(vec![kaijutsu_types::OutputNode::new("row")]);
        assert!(
            tree.rich_json.is_none(),
            "test premise: the real tree carries no rich_json of its own"
        );
        let mut result = ExecResult::with_output(tree.clone());
        result.data = Some(Value::Json(serde_json::json!({"k": "v"})));

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.root, tree.root,
            "the real node tree must survive untouched"
        );
        assert_eq!(
            bridged.rich_json,
            Some(serde_json::json!({"k": "v"})),
            "rich_json must be back-filled from .data since .output didn't set its own"
        );
    }
}

#[cfg(test)]
mod build_output_data_tests {
    //! Encode-side coverage for the OutputData→capnp `richJson` wiring: the
    //! kj / builtin structured payload (`OutputData::rich_json`) must land on
    //! the wire as JSON text, and stay unset (empty string, per the
    //! kaijutsu.capnp "empty = none" convention) when absent. Decode-side
    //! (`parse_output_data`) has the symmetric test in kaijutsu-client.
    use super::build_output_data;
    use capnp::message::Builder as MessageBuilder;

    #[test]
    fn rich_json_present_lands_on_wire_as_json_text() {
        let data = kaijutsu_types::OutputData::new()
            .with_rich_json(serde_json::json!(["bass", "bassline"]));

        let mut message = MessageBuilder::new_default();
        build_output_data(
            message.init_root::<crate::kaijutsu_capnp::output_data::Builder>(),
            &data,
        );
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::output_data::Reader>()
            .unwrap();

        let wire_text = reader.get_rich_json().unwrap().to_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(wire_text).unwrap();
        assert_eq!(parsed, serde_json::json!(["bass", "bassline"]));
    }

    #[test]
    fn rich_json_absent_leaves_wire_field_empty() {
        let data = kaijutsu_types::OutputData::new();

        let mut message = MessageBuilder::new_default();
        build_output_data(
            message.init_root::<crate::kaijutsu_capnp::output_data::Builder>(),
            &data,
        );
        let reader = message
            .get_root_as_reader::<crate::kaijutsu_capnp::output_data::Reader>()
            .unwrap();

        assert_eq!(
            reader.get_rich_json().unwrap().to_str().unwrap(),
            "",
            "no rich_json → wire field must stay empty, never a stray value"
        );
    }
}

/// Fill a Cap'n Proto `BlockMetadata` builder from the typed metadata.
fn build_block_metadata(
    mut builder: crate::kaijutsu_capnp::block_metadata::Builder<'_>,
    meta: &kaijutsu_types::BlockMetadata,
) {
    if let Some(code) = meta.exit_code {
        builder.set_has_exit_code(true);
        builder.set_exit_code(code);
    }
    builder.set_is_error(meta.is_error);
    builder.set_content_type(meta.content_type.as_mime());
    builder.set_ephemeral(meta.ephemeral);
    if let Some(ref tui) = meta.tool_use_id {
        builder.set_tool_use_id(tui);
    }
    if let Some(ref stderr) = meta.stderr {
        builder.set_has_stderr(true);
        builder.set_stderr(stderr);
    }
    builder.set_task_status(meta.task_status.as_str());
}

/// Fill a Cap'n Proto `RenderCue` builder from the typed cue (docs/pcm.md "The
/// wire", docs/midi.md "Render is a wire cue"). Shared by both FlowBus bridges
/// (`subscribe_blocks` and `subscribe_blocks_filtered`) so the union-arm wiring
/// lives in one place. `lead` is a *relative* nanosecond count — a
/// process-local `Instant` can't cross the wire; the sink re-anchors at
/// `receipt + lead`. Saturates rather than truncates on the (implausible) case
/// of a lead past ~584 years.
fn set_render_cue(mut builder: crate::kaijutsu_capnp::render_cue::Builder<'_>, cue: &kaijutsu_audio::RenderCue) {
    builder.set_mime(&cue.mime);
    builder.set_lead_nanos(u64::try_from(cue.lead.as_nanos()).unwrap_or(u64::MAX));
    builder.set_epoch_ns(cue.epoch_ns);
    if let Some(onset_beat) = cue.onset_beat {
        builder.set_has_onset_beat(true);
        builder.set_onset_beat(onset_beat);
    }
    match &cue.payload {
        kaijutsu_audio::CuePayload::Inline(bytes) => builder.set_inline(bytes),
        kaijutsu_audio::CuePayload::Cas(hash) => builder.set_cas_hash(hash.to_string()),
    }
}

/// How much longer this bridge waits for the sink's `exchange` promise than
/// the sink was told to wait for the device. Sits deliberately BETWEEN the
/// client-side worker slack (0.5s) and the kernel's outer deadline (1s), so
/// whichever layer actually wedged is the layer whose error the player reads.
const EXCHANGE_CALL_SLACK: std::time::Duration = std::time::Duration::from_millis(750);

/// Bridge the kernel's [`MidiExchangeRegistry`](kaijutsu_kernel::midi_exchange::MidiExchangeRegistry)
/// to ONE connection's sink callback (`docs/midi-next.md` "SysEx: the exchange
/// pattern", slice 1 step 5).
///
/// This is the reverse-direction twin of the FlowBus bridges above, and the
/// two differ in exactly the way the doc says an exchange differs from a cue:
/// a FlowBus bridge fans an event out to every subscriber and forgets it; this
/// one carries a *call* to the single connection the kernel addressed, and
/// carries its answer back. It lives here rather than in the kernel because
/// the callback capability is `!Send` and pinned to this connection's
/// `LocalSet` — which is the whole reason the kernel talks to a channel
/// instead of holding the capability itself.
///
/// Exits on connection cancel or when the registration is replaced (its
/// channel closes), and withdraws only its OWN registration epoch on the way
/// out so a re-subscribe's live sink is never torn down by the task it
/// replaced.
fn spawn_midi_exchange_bridge(
    registry: Arc<kaijutsu_kernel::midi_exchange::MidiExchangeRegistry>,
    connection: SessionId,
    callback: crate::kaijutsu_capnp::block_events::Client,
    conn_cancel: CancellationToken,
) {
    let registration = registry.register(connection);
    let epoch = registration.epoch;
    let mut rx = registration.receiver;
    tokio::task::spawn_local(async move {
        log::debug!(
            "midi exchange: connection {} now answers exchanges",
            connection.short()
        );
        loop {
            let req = tokio::select! {
                _ = conn_cancel.cancelled() => break,
                req = rx.recv() => match req {
                    Some(req) => req,
                    // Registration replaced (a re-subscribe) or the kernel
                    // dropped the registry: nothing more will arrive here.
                    None => break,
                },
            };
            let mut call = callback.exchange_request();
            {
                let mut p = call.get();
                p.set_port_or_device(&req.port_or_device);
                p.set_payload(&req.payload[..]);
                p.set_reply_match(&req.reply_match[..]);
                p.set_timeout_ms(req.timeout_ms);
            }
            let deadline =
                std::time::Duration::from_millis(u64::from(req.timeout_ms)) + EXCHANGE_CALL_SLACK;
            // Cancellation is checked BETWEEN requests, not during one: a
            // connection that dies mid-exchange is bounded by `deadline`
            // rather than woken by `conn_cancel`. Deliberate — capnp fails an
            // outstanding promise on teardown anyway, so the slack is a
            // backstop for the case it doesn't, and racing a cancel against a
            // half-sent hardware dialogue buys nothing (the sink's own
            // timeout still governs the device).
            // Exchanges are serialized per connection by construction: this
            // loop awaits each answer before taking the next request. That is
            // the "serialized per port" rule at its coarsest safe grain — a
            // sink's own worker enforces it on the hardware side too.
            let answer = match tokio::time::timeout(deadline, call.send().promise).await {
                Ok(Ok(response)) => response
                    .get()
                    .and_then(|r| r.get_reply())
                    .map(|bytes| bytes.to_vec())
                    .map_err(|e| format!("malformed exchange reply from the sink: {e}")),
                Ok(Err(e)) => Err(format!("{e}")),
                Err(_) => Err(format!(
                    "the sink did not answer the exchange within {deadline:?}"
                )),
            };
            // Nobody to answer = the kernel-side caller gave up first; that
            // is its own error already, so this is not worth a warning.
            let _ = req.reply.send(answer);
        }
        registry.unregister_epoch(connection, epoch);
        log::debug!(
            "midi exchange bridge for connection {} ended",
            connection.short()
        );
    });
}

/// Fill a Cap'n Proto `BeatRef` builder from the typed reference (docs/midi.md
/// "The relative-lead timebase, analyzed"). Carries the sender's emission
/// wallclock (`epochNs`, mirroring `reportClockEstimate`) so the sink can
/// back-date it to its own emission instant instead of anchoring at receipt —
/// covers both forward-loop callers (`on_beat_sync_request` above).
fn set_beat_ref(mut builder: crate::kaijutsu_capnp::beat_ref::Builder<'_>, beat_ref: &kaijutsu_audio::BeatRef) {
    builder.set_beat(beat_ref.beat);
    builder.set_tempo_bps(beat_ref.tempo_bps);
    builder.set_epoch_ns(beat_ref.epoch_ns);
}

/// The FlowBus topic pattern a **filtered** client block-subscription listens on.
///
/// This pattern must be a *superset* of everything `BlockFlow::matches_filter`
/// can pass: the pattern gates what reaches the bridge, `matches_filter` gates
/// what is forwarded, and anything the pattern excludes is silently dropped
/// before `matches_filter` ever runs. `RenderCue` is a directive that
/// `matches_filter` passes **unconditionally** (docs/pcm.md — "every subscriber
/// hears it"), so every filtered subscription MUST include `block.render_cue`.
/// A single FlowBus pattern can't express "block.status OR block.render_cue",
/// so the narrow single-topic fast path is only safe when the lone requested
/// type *is* that directive; any other single type widens to `block.*` and lets
/// `matches_filter` do the gating. (Found by the deepseek review 2026-07-01: the
/// old inline code narrowed e.g. `[StatusChanged]` → `block.status` and silently
/// dropped render directives.) If a high-throughput narrow subscriber ever needs
/// the topic partition back, the fix is a *second* subscription for directive
/// topics, not re-narrowing here.
fn filtered_subscribe_pattern(event_types: &[kaijutsu_types::BlockFlowKind]) -> &'static str {
    match event_types {
        [kaijutsu_types::BlockFlowKind::RenderCue] => "block.render_cue",
        [kaijutsu_types::BlockFlowKind::BeatSync] => "block.beat_sync",
        _ => "block.*",
    }
}

// ============================================================================
// Peer Helper Functions
// ============================================================================

/// Set PeerInfo fields on a Cap'n Proto builder.
fn set_peer_info(builder: &mut peer_info::Builder, info: &PeerInfo) {
    builder.set_nick(&info.nick);
    builder.set_attached_at(info.attached_at);
}

// ============================================================================
// Shell Execution Dispatch
// ============================================================================
//
// LLM streaming + agentic loop moved to `crate::llm_stream`
// (build_tool_definitions, spawn_llm_for_prompt, tool_kind_for_category,
// process_llm_stream all live there).

/// Create kaish, insert ToolCall + ToolResult blocks, spawn execution.
///
/// Shared by `shell_execute` (direct RPC) and `submit_input` (shell mode).
/// Exit codes 0/2/3 map to Done; everything else is Error.
/// Render a kaish value as a string for durable `context_env` storage.
///
/// `context_env` is string-only (a normalized KV table — defense in depth), so
/// structured shell values collapse to their scalar/JSON text form on the way
/// to L1. This is the lossy half of the shell-var ⇄ env round-trip: a value set
/// via `setShellVar` and read back via `getShellVar` returns as a `String`.
fn value_to_env_string(value: &kaish_kernel::ast::Value) -> String {
    use kaish_kernel::ast::Value;
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Json(j) => serde_json::to_string(j).unwrap_or_default(),
        // String-only env is lossy by design; the base64 envelope is the most
        // faithful (round-trippable) text form for inline binary.
        Value::Bytes(_) => kaish_kernel::interpreter::value_to_json(value).to_string(),
    }
}

/// Read a context's durable cwd from L1 (`context_shell.cwd`). Returns `None`
/// when unset or unreadable. `get_cwd` (the interactive shell's `kj cwd`)
/// still applies its own `/docs` landing-dir default for display purposes;
/// every `ExecContext`-constructing call site instead passes the `Option`
/// straight through to `ExecContext::new`/`new_without_cwd` — a tool
/// context with no cwd is `None`, not a fabricated `/` (`servers/file.rs`
/// rejects it outright rather than resolving paths against a fake root).
fn context_cwd(kernel: &SharedKernelState, context_id: ContextId) -> Option<std::path::PathBuf> {
    kernel
        .kernel_db
        .lock()
        .get_context_shell(context_id)
        .ok()
        .flatten()
        .and_then(|row| row.cwd)
        .map(std::path::PathBuf::from)
}

/// Materialize a single-use context shell for this connection's current
/// identity (principal + active context + session), seeded from the context's
/// durable L1 state (`context_env` + `context_shell.cwd`).
///
/// The instance is throwaway: run exactly one command against it and drop it.
/// Durable changes flow through `kj context set`, never through this instance's
/// transient scope — so two callers never see each other's in-flight vars. The
/// factory hands back a kaish whose session→context map is *isolated* from the
/// connection's; callers that run context-switching commands (`kj context
/// switch`, `kj fork`) must read `kaish.context_id()` afterward and write any
/// change back to the connection's `session_contexts`.
async fn materialize_context_shell(
    kernel: &SharedKernelState,
    connection: &Rc<RefCell<ConnectionState>>,
) -> Result<EmbeddedKaish, capnp::Error> {
    let (name, principal, context_id, session_id) = {
        let conn = connection.borrow();
        (
            format!(
                "{}-{}-{}",
                kernel.name,
                conn.principal.username,
                conn.session_id.short()
            ),
            conn.principal.id,
            conn.require_context()?,
            conn.session_id,
        )
    };
    // One materialization path for both shells: read the index + block source
    // off the dispatcher (the server installs the index there at bootstrap via
    // `set_semantic_index`), the same accessors the in-kernel model shell uses.
    // `dispatcher.semantic_index()` mirrors `kernel.semantic_index` — installed
    // from the same Arc — so the human and model shells can never drift apart.
    kernel
        .kj_dispatcher
        .materialize_context_kaish(
            &name,
            principal,
            context_id,
            session_id,
            kernel.kj_dispatcher.semantic_index(),
            kernel.kj_dispatcher.block_source(),
        )
        .await
        .map_err(|e| capnp::Error::failed(format!("kaish materialization failed: {}", e)))
}

/// After a materialized shell runs, propagate any in-shell context switch
/// (`kj context switch` / `kj fork`) back to the connection's shared
/// `session_contexts`. The materialized shell carries an isolated map, so
/// without this an in-shell switch would be invisible to subsequent RPCs
/// (`require_context`). Returns the new context id when a switch occurred.
fn propagate_context_switch(
    kaish: &EmbeddedKaish,
    started_at: ContextId,
    connection: &Rc<RefCell<ConnectionState>>,
) -> Option<ContextId> {
    match kaish.context_id() {
        Some(new_id) if new_id != started_at => {
            let conn = connection.borrow();
            conn.session_contexts.insert(conn.session_id, new_id);
            Some(new_id)
        }
        _ => None,
    }
}

/// The durable surface of a context shell: working directory + exported env.
/// Snapshotted before and after a command so we can persist exactly what the
/// command changed back to L1, the same way a real shell's `cd` / `export`
/// outlive the command that ran them.
struct ShellStateSnapshot {
    cwd: std::path::PathBuf,
    env: std::collections::BTreeMap<String, String>,
}

async fn snapshot_shell_state(kaish: &EmbeddedKaish) -> ShellStateSnapshot {
    ShellStateSnapshot {
        cwd: kaish.cwd().await,
        env: kaish.exported_vars().await.into_iter().collect(),
    }
}

/// Persist a command's effect on the shell's durable surface (cwd + exported
/// env) back to L1, so the next materialized shell for this context lands where
/// the last command left off. Diffs `before`/`after`: only a moved cwd or an
/// added/changed/removed export is written — an `ls` touches nothing.
/// Last-writer-wins across concurrent peers, matching `propagate_context_switch`.
///
/// Caller must skip this when the command switched context (`kj context switch`
/// / `kj fork`): the snapshots straddle two contexts, and the outgoing cwd is
/// already saved inside kaish on switch (`KjBuiltin::save_context_cwd`).
fn persist_shell_state(
    kernel_db: &Arc<parking_lot::Mutex<KernelDb>>,
    context_id: ContextId,
    before: &ShellStateSnapshot,
    after: &ShellStateSnapshot,
) {
    let db = kernel_db.lock();

    // cwd: write only when the command moved it, so we never clobber a
    // concurrent peer's cwd with a value this command never touched.
    if after.cwd != before.cwd
        && let Err(e) = db.upsert_context_shell(&ContextShellRow {
            context_id,
            cwd: Some(after.cwd.to_string_lossy().into_owned()),
            updated_at: kaijutsu_types::now_millis() as i64,
        })
    {
        log::warn!("failed to persist context cwd: {}", e);
    }

    // exported env: upsert added/changed keys, delete keys the command unset.
    for (key, value) in &after.env {
        if before.env.get(key) != Some(value)
            && let Err(e) = db.set_context_env(context_id, key, value)
        {
            log::warn!("failed to persist context env {}: {}", key, e);
        }
    }
    for key in before.env.keys() {
        if !after.env.contains_key(key)
            && let Err(e) = db.delete_context_env(context_id, key)
        {
            log::warn!("failed to delete context env {}: {}", key, e);
        }
    }
}

async fn execute_shell_command(
    code: &str,
    context_id: ContextId,
    user_principal_id: PrincipalId,
    user_initiated: bool,
    kernel: &SharedKernelState,
    connection: &Rc<RefCell<ConnectionState>>,
) -> Result<kaijutsu_crdt::BlockId, capnp::Error> {
    // Materialize a single-use context shell seeded from L1 (durable env + cwd).
    // No caching: transient scope evaporates when this instance drops, so the
    // context's durable state only ever changes through `kj context set`.
    let kaish = materialize_context_shell(kernel, connection).await?;

    let documents = kernel.documents.clone();
    let kernel_arc = kernel.kernel.clone();

    // Link to context's long-running trace
    let trace_id = {
        let drift = kernel_arc.drift().read();
        drift.trace_id_for_context(context_id).unwrap_or([0u8; 16])
    };
    let _ctx_span = kaijutsu_telemetry::context_root_span(&trace_id, "shell_execute").entered();

    // Document must exist — join_context is the sole creator
    if documents.get(context_id).is_none() {
        return Err(capnp::Error::failed(format!(
            "context {} not found — call join_context first",
            context_id
        )));
    }

    // Create ToolCall block for the shell command (authored by user if user_initiated)
    let last_block = documents.last_block_id(context_id);
    let role = if user_initiated {
        Some(Role::User)
    } else {
        None
    };
    let command_block_id = documents
        .insert_tool_call_as(
            context_id,
            None,
            last_block.as_ref(),
            "shell",
            serde_json::json!({"code": code}),
            Some(TypesToolKind::Shell),
            Some(user_principal_id),
            None,
            role,
        )
        .map_err(|e| capnp::Error::failed(format!("failed to insert shell command: {}", e)))?;

    // Create ToolResult block (empty, will be filled by execution — system-authored)
    let output_block_id = documents
        .insert_tool_result_as(
            context_id,
            &command_block_id,
            Some(&command_block_id),
            "",
            false,
            None,
            Some(TypesToolKind::Shell),
            Some(PrincipalId::system()),
            None,
        )
        .map_err(|e| capnp::Error::failed(format!("failed to insert shell output: {}", e)))?;

    // Mark output block as Running — clients poll this to detect completion
    if let Err(e) = documents.set_status(context_id, &output_block_id, Status::Running) {
        log::warn!("Failed to set output block to Running: {}", e);
    }

    // User-initiated shell blocks are excluded from conversation by default.
    // Users can toggle inclusion via the block gutter controls.
    if user_initiated {
        if let Err(e) = documents.set_excluded(context_id, &command_block_id, true) {
            log::warn!("Failed to set shell command block excluded: {}", e);
        }
        if let Err(e) = documents.set_excluded(context_id, &output_block_id, true) {
            log::warn!("Failed to set shell output block excluded: {}", e);
        }
    }

    // Spawn execution in background
    let code = code.to_owned();
    let documents_clone = documents.clone();
    let output_block_id_clone = output_block_id;
    let command_block_id_clone = command_block_id;
    let block_flows = kernel_arc.block_flows().clone();
    let connection_switch = connection.clone();
    let kernel_db_for_persist = kernel.kernel_db.clone();

    tokio::task::spawn_local(async move {
        // Yield to let the event loop flush BlockInserted events to clients
        // before we start producing text ops. Without this, fast commands
        // (like `ls`) can emit edit_text before the client has processed the
        // BlockInserted, causing DataMissing errors on the client side.
        tokio::task::yield_now().await;

        // Snapshot the shell's durable surface (cwd + exported env) so we can
        // persist whatever this command changes (`cd`, `export`) back to L1.
        let state_before = snapshot_shell_state(&kaish).await;

        log::info!(
            "shell_execute: executing code via EmbeddedKaish: {:?}",
            code
        );
        match kaish
            .execute_with_options(&code, kaish_kernel::ExecuteOptions::default())
            .await
        {
            Ok(result) => {
                log::info!(
                    "shell_execute: kaish returned code={} original_code={:?} did_spill={} out_len={} err_len={}",
                    result.code,
                    result.original_code,
                    result.did_spill,
                    result.text_out().len(),
                    result.err.len()
                );

                // stdout → block content (DTE-tracked, app-observable, streams).
                // stderr → its own metadata field so callers can tell them apart
                // (a successful-with-warnings command carries stderr + exit 0).
                // The LLM still sees both: hydration merges stderr back into the
                // tool_result content (see hydrate.rs).
                let out_text = result.text_out().into_owned();
                if let Err(e) = documents_clone.edit_text_as(
                    context_id,
                    &output_block_id_clone,
                    0,
                    &out_text,
                    0,
                    Some(PrincipalId::system()),
                ) {
                    log::error!("Failed to update shell output: {}", e);
                }

                if !result.err.is_empty()
                    && let Err(e) = documents_clone.set_stderr(
                        context_id,
                        &output_block_id_clone,
                        Some(result.err.clone()),
                    )
                {
                    log::error!("Failed to set shell stderr: {}", e);
                }

                if let Some(output_data) = block_output_data(&result)
                    && let Err(e) = documents_clone.set_output(
                        context_id,
                        &output_block_id_clone,
                        Some(&output_data),
                    )
                {
                    log::error!("Failed to set output data: {}", e);
                }

                if let Some(ref ct_str) = result.content_type {
                    let ct = ContentType::from_mime(ct_str);
                    if ct != ContentType::Plain
                        && let Err(e) =
                            documents_clone.set_content_type(context_id, &output_block_id_clone, ct)
                    {
                        log::error!("Failed to set content_type: {}", e);
                    }
                }

                // Read baggage: mark blocks ephemeral if tool signaled it
                if result
                    .baggage
                    .get("kaijutsu.ephemeral")
                    .map(|v| v == "true")
                    .unwrap_or(false)
                {
                    for bid in [&command_block_id_clone, &output_block_id_clone] {
                        if let Err(e) = documents_clone.set_ephemeral(context_id, bid, true) {
                            log::error!("Failed to set ephemeral on block: {}", e);
                        }
                    }
                }

                // Persist the real kaish exit code on the ToolResult block
                // before flipping status. Consumers (MCP context_shell return,
                // BRP introspection, history views) read this to distinguish
                // exit codes that all map to the same Status::Error.
                // Clamp to i32 — POSIX exit codes are 0-255; saturating cast
                // covers the i64-to-i32 narrowing without surprise.
                let exit_code_i32: i32 = result.code.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                if let Err(e) = documents_clone.set_exit_code(
                    context_id,
                    &output_block_id_clone,
                    Some(exit_code_i32),
                ) {
                    log::error!("Failed to set output block exit_code: {}", e);
                }

                // Settle durable context state *before* flipping status to a
                // terminal value: clients (and our own e2e harness) treat the
                // ToolResult reaching Done/Error as "command finished" and may
                // fire their next command immediately. If we persisted after, a
                // back-to-back `cd /x` then `pwd` could re-materialize the shell
                // off stale L1. Detect an in-shell context switch (kj fork /
                // context switch) and propagate it to the connection's shared
                // map; otherwise persist this command's cwd/export changes to the
                // context it ran in. (On a switch the snapshots straddle two
                // contexts and the outgoing cwd is already saved inside kaish, so
                // we skip the write-back.)
                match propagate_context_switch(&kaish, context_id, &connection_switch) {
                    Some(new_context_id) => {
                        log::info!(
                            "shell_execute: context switched {} → {}",
                            context_id,
                            new_context_id
                        );
                        block_flows.publish(kaijutsu_kernel::flows::BlockFlow::ContextSwitched {
                            context_id: new_context_id,
                        });
                    }
                    None => {
                        let state_after = snapshot_shell_state(&kaish).await;
                        persist_shell_state(
                            &kernel_db_for_persist,
                            context_id,
                            &state_before,
                            &state_after,
                        );
                    }
                }

                // Exit 2: latch gate (rm/trash) — confirmation message shown, not a failure
                // Exit 3 / did_spill: output truncated to spill file — command ran, not a failure
                let final_status = match result.code {
                    0 | 2 | 3 => Status::Done,
                    _ => Status::Error,
                };
                if let Err(e) =
                    documents_clone.set_status(context_id, &output_block_id_clone, final_status)
                {
                    log::error!("Failed to set output block status: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &command_block_id_clone, final_status)
                {
                    log::error!("Failed to set command block status: {}", e);
                }
            }
            Err(e) => {
                let error_msg = format!("Error: {}", e);
                log::error!("Shell execution failed: {}", e);
                if let Err(e) = documents_clone.edit_text_as(
                    context_id,
                    &output_block_id_clone,
                    0,
                    &error_msg,
                    0,
                    Some(PrincipalId::system()),
                ) {
                    log::error!("Failed to update shell output with error: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &output_block_id_clone, Status::Error)
                {
                    log::error!("Failed to set output block error status: {}", e);
                }
                if let Err(e) =
                    documents_clone.set_status(context_id, &command_block_id_clone, Status::Error)
                {
                    log::error!("Failed to set command block error status: {}", e);
                }
            }
        }
    });

    Ok(command_block_id)
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Parse a BlockId from a Cap'n Proto BlockId reader (binary format).
fn parse_block_id_from_reader(
    reader: &crate::kaijutsu_capnp::block_id::Reader<'_>,
) -> Result<kaijutsu_crdt::BlockId, capnp::Error> {
    let context_id = ContextId::try_from_slice(reader.get_context_id()?)
        .ok_or_else(|| capnp::Error::failed("invalid context_id in BlockId".into()))?;
    let principal_id = PrincipalId::try_from_slice(reader.get_principal_id()?)
        .ok_or_else(|| capnp::Error::failed("invalid principal_id in BlockId".into()))?;
    Ok(kaijutsu_crdt::BlockId {
        context_id,
        principal_id,
        seq: reader.get_seq(),
    })
}

/// Set BlockId fields on a Cap'n Proto builder (binary format).
fn set_block_id_builder(
    builder: &mut crate::kaijutsu_capnp::block_id::Builder,
    block_id: &kaijutsu_crdt::BlockId,
) {
    builder.set_context_id(block_id.context_id.as_bytes());
    builder.set_principal_id(block_id.principal_id.as_bytes());
    builder.set_seq(block_id.seq);
}

/// Kernel [`TurnStopReason`] → its wire enumerant.
///
/// Total by construction — an exhaustive match, so adding a kernel-side ending
/// without giving it a wire name is a compile error rather than a stop reason
/// that silently arrives at every client as `endTurn`.
fn stop_reason_to_capnp(reason: TurnStopReason) -> crate::kaijutsu_capnp::TurnStopReason {
    use crate::kaijutsu_capnp::TurnStopReason as W;
    match reason {
        TurnStopReason::EndTurn => W::EndTurn,
        TurnStopReason::Cancelled { immediate: false } => W::CancelledSoft,
        TurnStopReason::Cancelled { immediate: true } => W::CancelledImmediate,
        TurnStopReason::MaxTokens => W::MaxTokens,
        TurnStopReason::MaxIterations => W::MaxIterations,
    }
}

/// Kernel [`TurnOrigin`] → its wire enumerant.
fn turn_origin_to_capnp(origin: TurnOrigin) -> crate::kaijutsu_capnp::TurnOrigin {
    match origin {
        TurnOrigin::Interactive => crate::kaijutsu_capnp::TurnOrigin::Interactive,
        TurnOrigin::Autonomous => crate::kaijutsu_capnp::TurnOrigin::Autonomous,
    }
}

/// Set BlockSnapshot fields on a Cap'n Proto builder.
fn set_block_snapshot(
    builder: &mut crate::kaijutsu_capnp::block_snapshot::Builder,
    block: &kaijutsu_crdt::BlockSnapshot,
) {
    // Set ID
    {
        let mut id = builder.reborrow().init_id();
        set_block_id_builder(&mut id, &block.id);
    }

    // Set parent_id if present
    if let Some(ref parent) = block.parent_id {
        builder.set_has_parent_id(true);
        let mut pid = builder.reborrow().init_parent_id();
        set_block_id_builder(&mut pid, parent);
    } else {
        builder.set_has_parent_id(false);
    }

    // Set role
    builder.set_role(match block.role {
        kaijutsu_crdt::Role::User => crate::kaijutsu_capnp::Role::User,
        kaijutsu_crdt::Role::Model => crate::kaijutsu_capnp::Role::Model,
        kaijutsu_crdt::Role::System => crate::kaijutsu_capnp::Role::System,
        kaijutsu_crdt::Role::Tool => crate::kaijutsu_capnp::Role::Tool,
        kaijutsu_crdt::Role::Asset => crate::kaijutsu_capnp::Role::Asset,
    });

    // Set status
    builder.set_status(match block.status {
        kaijutsu_crdt::Status::Pending => crate::kaijutsu_capnp::Status::Pending,
        kaijutsu_crdt::Status::Running => crate::kaijutsu_capnp::Status::Running,
        kaijutsu_crdt::Status::Done => crate::kaijutsu_capnp::Status::Done,
        kaijutsu_crdt::Status::Error => crate::kaijutsu_capnp::Status::Error,
    });

    // Set kind
    builder.set_kind(match block.kind {
        kaijutsu_crdt::BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
        kaijutsu_crdt::BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
        kaijutsu_crdt::BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
        kaijutsu_crdt::BlockKind::ToolResult => crate::kaijutsu_capnp::BlockKind::ToolResult,
        kaijutsu_crdt::BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
        kaijutsu_crdt::BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
        kaijutsu_crdt::BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
        kaijutsu_crdt::BlockKind::Notification => crate::kaijutsu_capnp::BlockKind::Notification,
        kaijutsu_crdt::BlockKind::Resource => crate::kaijutsu_capnp::BlockKind::Resource,
        kaijutsu_crdt::BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
        kaijutsu_crdt::BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
    });

    // Set basic fields (no author — derived from id.principal_id)
    builder.set_content(&block.content);
    builder.set_collapsed(block.collapsed);
    builder.set_created_at(block.created_at);

    // Set tool-specific fields
    if let Some(ref name) = block.tool_name {
        builder.set_tool_name(name);
    }
    if let Some(ref input) = block.tool_input {
        builder.set_tool_input(input);
    }
    if let Some(ref tc_id) = block.tool_call_id {
        let mut tcid = builder.reborrow().init_tool_call_id();
        set_block_id_builder(&mut tcid, tc_id);
    }
    if let Some(code) = block.exit_code {
        builder.set_has_exit_code(true);
        builder.set_exit_code(code);
    }
    builder.set_is_error(block.is_error);
    if let Some(ref stderr) = block.stderr {
        builder.set_has_stderr(true);
        builder.set_stderr(stderr);
    }
    if let Some(ref signature) = block.signature {
        builder.set_has_signature(true);
        builder.set_signature(signature);
    }

    // Set output data if present
    if let Some(ref output) = block.output {
        build_output_data(builder.reborrow().init_output_data(), output);
    }

    // Set tool mechanism metadata
    if let Some(tk) = block.tool_kind {
        builder.set_has_tool_kind(true);
        builder.set_tool_kind(tool_kind_to_capnp(tk));
    }

    // Set file metadata
    if let Some(ref path) = block.file_path {
        builder.set_file_path(path);
    }

    // Set tool_use_id (LLM-assigned tool invocation ID)
    if let Some(ref tui) = block.tool_use_id {
        builder.set_tool_use_id(tui);
    }

    // Set content_type hint (MIME type)
    if block.content_type != ContentType::Plain {
        builder.set_content_type(block.content_type.as_mime());
    }

    // Set task lifecycle status (meaningful only on BlockKind::Task; "" on
    // the wire falls back to Open, same convention as content_type/Plain)
    if block.task_status != TaskStatus::default() {
        builder.set_task_status(block.task_status.as_str());
    }

    // Set ephemeral flag
    builder.set_ephemeral(block.ephemeral);
    builder.set_excluded(block.excluded);

    // Set hyoushigi timeline coordinate if present
    if let Some(tick) = block.tick {
        builder.set_has_tick(true);
        builder.set_tick(tick.get());
    }

    // Set hyoushigi track (lane identity) if present — Some only on materialized
    // timeline cells. The lane, never the author (author stays id.principalId).
    if let Some(ref track) = block.track {
        builder.set_has_track(true);
        builder.set_track(track.as_str());
    }

    // Set drift-specific fields if present
    if let Some(ref ctx) = block.source_context {
        builder.set_source_context(ctx.as_bytes());
    }
    if let Some(ref model) = block.source_model {
        builder.set_source_model(model);
    }
    if let Some(dk) = block.drift_kind {
        builder.set_has_drift_kind(true);
        builder.set_drift_kind(drift_kind_to_capnp(dk));
    }

    // Set error payload if present
    if let Some(ref payload) = block.error {
        builder.set_has_error_payload(true);
        let mut ep = builder.reborrow().init_error_payload();
        ep.set_category(error_category_to_capnp(payload.category));
        ep.set_severity(error_severity_to_capnp(payload.severity));
        if let Some(ref code) = payload.code {
            ep.set_code(code);
        }
        if let Some(ref detail) = payload.detail {
            ep.set_detail(detail);
        }
        if let Some(ref span) = payload.span {
            ep.set_has_span(true);
            ep.set_span_line(span.line);
            ep.set_span_column(span.column);
            ep.set_span_length(span.length);
        }
        if let Some(sk) = payload.source_kind {
            ep.set_has_source_kind(true);
            ep.set_source_kind(match sk {
                kaijutsu_crdt::BlockKind::Text => crate::kaijutsu_capnp::BlockKind::Text,
                kaijutsu_crdt::BlockKind::Thinking => crate::kaijutsu_capnp::BlockKind::Thinking,
                kaijutsu_crdt::BlockKind::ToolCall => crate::kaijutsu_capnp::BlockKind::ToolCall,
                kaijutsu_crdt::BlockKind::ToolResult => {
                    crate::kaijutsu_capnp::BlockKind::ToolResult
                }
                kaijutsu_crdt::BlockKind::Drift => crate::kaijutsu_capnp::BlockKind::Drift,
                kaijutsu_crdt::BlockKind::File => crate::kaijutsu_capnp::BlockKind::File,
                kaijutsu_crdt::BlockKind::Error => crate::kaijutsu_capnp::BlockKind::Error,
                kaijutsu_crdt::BlockKind::Notification => {
                    crate::kaijutsu_capnp::BlockKind::Notification
                }
                kaijutsu_crdt::BlockKind::Resource => {
                    crate::kaijutsu_capnp::BlockKind::Resource
                }
                kaijutsu_crdt::BlockKind::Trace => crate::kaijutsu_capnp::BlockKind::Trace,
                kaijutsu_crdt::BlockKind::Task => crate::kaijutsu_capnp::BlockKind::Task,
            });
        }
    }

    // Set notification payload if present
    if let Some(ref payload) = block.notification {
        builder.set_has_notification_payload(true);
        let mut np = builder.reborrow().init_notification_payload();
        np.set_instance(&payload.instance);
        np.set_kind(match payload.kind {
            kaijutsu_crdt::NotificationKind::ToolAdded => {
                crate::kaijutsu_capnp::NotificationKind::ToolAdded
            }
            kaijutsu_crdt::NotificationKind::ToolRemoved => {
                crate::kaijutsu_capnp::NotificationKind::ToolRemoved
            }
            kaijutsu_crdt::NotificationKind::Log => crate::kaijutsu_capnp::NotificationKind::Log,
            kaijutsu_crdt::NotificationKind::PromptsChanged => {
                crate::kaijutsu_capnp::NotificationKind::PromptsChanged
            }
            kaijutsu_crdt::NotificationKind::Coalesced => {
                crate::kaijutsu_capnp::NotificationKind::Coalesced
            }
        });
        if let Some(level) = payload.level {
            np.set_has_level(true);
            np.set_level(match level {
                kaijutsu_crdt::LogLevel::Trace => crate::kaijutsu_capnp::LogLevel::Trace,
                kaijutsu_crdt::LogLevel::Debug => crate::kaijutsu_capnp::LogLevel::Debug,
                kaijutsu_crdt::LogLevel::Info => crate::kaijutsu_capnp::LogLevel::Info,
                kaijutsu_crdt::LogLevel::Warn => crate::kaijutsu_capnp::LogLevel::Warn,
                kaijutsu_crdt::LogLevel::Error => crate::kaijutsu_capnp::LogLevel::Error,
            });
        }
        if !payload.tools.is_empty() {
            let mut tools_builder = np
                .reborrow()
                .init_tools(payload.tools.len() as u32);
            for (i, name) in payload.tools.iter().enumerate() {
                tools_builder.set(i as u32, name);
            }
        }
        if let Some(count) = payload.count {
            np.set_has_count(true);
            np.set_count(count as u32);
        }
        if let Some(ref detail) = payload.detail {
            np.set_detail(detail);
        }
    }

    // Set resource payload if present (Phase 3 — D-42).
    if let Some(ref payload) = block.resource {
        builder.set_has_resource_payload(true);
        let mut rp = builder.reborrow().init_resource_payload();
        rp.set_instance(&payload.instance);
        rp.set_uri(&payload.uri);
        if let Some(ref mime) = payload.mime_type {
            rp.set_has_mime_type(true);
            rp.set_mime_type(mime);
        }
        if let Some(size) = payload.size {
            rp.set_has_size(true);
            rp.set_size(size);
        }
        if let Some(ref text) = payload.text {
            rp.set_has_text(true);
            rp.set_text(text);
        }
        if let Some(ref blob) = payload.blob_base64 {
            rp.set_has_blob(true);
            rp.set_blob_base64(blob);
        }
        if let Some(ref parent) = payload.parent_resource_block_id {
            rp.set_has_parent_resource_block_id(true);
            let mut pid = rp.reborrow().init_parent_resource_block_id();
            set_block_id_builder(&mut pid, parent);
        }
    }
}

/// Convert a CRDT ErrorCategory to Cap'n Proto.
fn error_category_to_capnp(
    cat: kaijutsu_crdt::ErrorCategory,
) -> crate::kaijutsu_capnp::ErrorCategory {
    match cat {
        kaijutsu_crdt::ErrorCategory::Tool => crate::kaijutsu_capnp::ErrorCategory::Tool,
        kaijutsu_crdt::ErrorCategory::Stream => crate::kaijutsu_capnp::ErrorCategory::Stream,
        kaijutsu_crdt::ErrorCategory::Rpc => crate::kaijutsu_capnp::ErrorCategory::Rpc,
        kaijutsu_crdt::ErrorCategory::Render => crate::kaijutsu_capnp::ErrorCategory::Render,
        kaijutsu_crdt::ErrorCategory::Parse => crate::kaijutsu_capnp::ErrorCategory::Parse,
        kaijutsu_crdt::ErrorCategory::Validation => {
            crate::kaijutsu_capnp::ErrorCategory::Validation
        }
        kaijutsu_crdt::ErrorCategory::Kernel => crate::kaijutsu_capnp::ErrorCategory::Kernel,
    }
}

/// Convert a CRDT ErrorSeverity to Cap'n Proto.
fn error_severity_to_capnp(
    sev: kaijutsu_crdt::ErrorSeverity,
) -> crate::kaijutsu_capnp::ErrorSeverity {
    match sev {
        kaijutsu_crdt::ErrorSeverity::Warning => crate::kaijutsu_capnp::ErrorSeverity::Warning,
        kaijutsu_crdt::ErrorSeverity::Error => crate::kaijutsu_capnp::ErrorSeverity::Error,
        kaijutsu_crdt::ErrorSeverity::Fatal => crate::kaijutsu_capnp::ErrorSeverity::Fatal,
    }
}

/// Convert a CRDT ToolKind to Cap'n Proto ToolKind.
fn tool_kind_to_capnp(tk: kaijutsu_crdt::ToolKind) -> crate::kaijutsu_capnp::ToolKind {
    match tk {
        kaijutsu_crdt::ToolKind::Shell => crate::kaijutsu_capnp::ToolKind::Shell,
        kaijutsu_crdt::ToolKind::Mcp => crate::kaijutsu_capnp::ToolKind::Mcp,
        kaijutsu_crdt::ToolKind::Builtin => crate::kaijutsu_capnp::ToolKind::Builtin,
    }
}

/// Convert a CRDT DriftKind to Cap'n Proto DriftKind.
fn drift_kind_to_capnp(dk: kaijutsu_crdt::DriftKind) -> crate::kaijutsu_capnp::DriftKind {
    match dk {
        kaijutsu_crdt::DriftKind::Push => crate::kaijutsu_capnp::DriftKind::Push,
        kaijutsu_crdt::DriftKind::Pull => crate::kaijutsu_capnp::DriftKind::Pull,
        kaijutsu_crdt::DriftKind::Merge => crate::kaijutsu_capnp::DriftKind::Merge,
        kaijutsu_crdt::DriftKind::Distill => crate::kaijutsu_capnp::DriftKind::Distill,
        kaijutsu_crdt::DriftKind::Notification => crate::kaijutsu_capnp::DriftKind::Notification,
        kaijutsu_crdt::DriftKind::Fork => crate::kaijutsu_capnp::DriftKind::Fork,
    }
}

/// Parse a Cap'n Proto BlockQuery union into a Rust BlockQuery.
fn parse_block_query(
    reader: &crate::kaijutsu_capnp::block_query::Reader<'_>,
) -> Result<kaijutsu_types::BlockQuery, capnp::Error> {
    match reader.which()? {
        crate::kaijutsu_capnp::block_query::All(()) => Ok(kaijutsu_types::BlockQuery::All),
        crate::kaijutsu_capnp::block_query::ByIds(ids_reader) => {
            let ids_reader = ids_reader?;
            let mut ids = Vec::with_capacity(ids_reader.len() as usize);
            for id_reader in ids_reader.iter() {
                ids.push(parse_block_id_from_reader(&id_reader)?);
            }
            Ok(kaijutsu_types::BlockQuery::ByIds(ids))
        }
        crate::kaijutsu_capnp::block_query::ByFilter(filter_reader) => {
            let filter = parse_block_filter(&filter_reader?)?;
            Ok(kaijutsu_types::BlockQuery::ByFilter(filter))
        }
    }
}

/// Parse a Cap'n Proto BlockFilter into a Rust BlockFilter.
fn parse_block_filter(
    reader: &crate::kaijutsu_capnp::block_filter::Reader<'_>,
) -> Result<kaijutsu_types::BlockFilter, capnp::Error> {
    let kinds = if reader.get_has_kinds() {
        let kinds_reader = reader.get_kinds()?;
        let mut kinds = Vec::with_capacity(kinds_reader.len() as usize);
        for k in kinds_reader.iter() {
            kinds.push(match k? {
                crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
                crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
                crate::kaijutsu_capnp::BlockKind::Resource => BlockKind::Resource,
                crate::kaijutsu_capnp::BlockKind::Trace => BlockKind::Trace,
                crate::kaijutsu_capnp::BlockKind::Task => BlockKind::Task,
            });
        }
        if kinds.is_empty() {
            return Err(capnp::Error::failed(
                "hasKinds=true but kinds list is empty".into(),
            ));
        }
        kinds
    } else {
        vec![]
    };

    let roles = if reader.get_has_roles() {
        let roles_reader = reader.get_roles()?;
        let mut roles = Vec::with_capacity(roles_reader.len() as usize);
        for r in roles_reader.iter() {
            roles.push(match r? {
                crate::kaijutsu_capnp::Role::User => Role::User,
                crate::kaijutsu_capnp::Role::Model => Role::Model,
                crate::kaijutsu_capnp::Role::System => Role::System,
                crate::kaijutsu_capnp::Role::Tool => Role::Tool,
                crate::kaijutsu_capnp::Role::Asset => Role::Asset,
            });
        }
        if roles.is_empty() {
            return Err(capnp::Error::failed(
                "hasRoles=true but roles list is empty".into(),
            ));
        }
        roles
    } else {
        vec![]
    };

    let statuses = if reader.get_has_statuses() {
        let statuses_reader = reader.get_statuses()?;
        let mut statuses = Vec::with_capacity(statuses_reader.len() as usize);
        for s in statuses_reader.iter() {
            statuses.push(match s? {
                crate::kaijutsu_capnp::Status::Pending => Status::Pending,
                crate::kaijutsu_capnp::Status::Running => Status::Running,
                crate::kaijutsu_capnp::Status::Done => Status::Done,
                crate::kaijutsu_capnp::Status::Error => Status::Error,
            });
        }
        if statuses.is_empty() {
            return Err(capnp::Error::failed(
                "hasStatuses=true but statuses list is empty".into(),
            ));
        }
        statuses
    } else {
        vec![]
    };

    let parent_id = if reader.get_has_parent_id() {
        Some(parse_block_id_from_reader(&reader.get_parent_id()?)?)
    } else {
        None
    };

    Ok(kaijutsu_types::BlockFilter {
        kinds,
        roles,
        statuses,
        limit: reader.get_limit(),
        max_depth: reader.get_max_depth(),
        parent_id,
    })
}

/// Parse a BlockEventFilter from a Cap'n Proto reader.
fn parse_block_event_filter(
    reader: crate::kaijutsu_capnp::block_event_filter::Reader<'_>,
) -> kaijutsu_types::BlockEventFilter {
    let context_ids = if reader.get_has_context_ids() {
        reader
            .get_context_ids()
            .map(|list| {
                list.iter()
                    .filter_map(|bytes| bytes.ok().and_then(ContextId::try_from_slice))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let event_types = if reader.get_has_event_types() {
        reader
            .get_event_types()
            .map(|list| {
                list.iter()
                    .filter_map(|k| {
                        k.ok().map(|k| match k {
                            crate::kaijutsu_capnp::BlockFlowKind::Inserted => {
                                kaijutsu_types::BlockFlowKind::Inserted
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::TextOps => {
                                kaijutsu_types::BlockFlowKind::TextOps
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::Deleted => {
                                kaijutsu_types::BlockFlowKind::Deleted
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::StatusChanged => {
                                kaijutsu_types::BlockFlowKind::StatusChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::CollapsedChanged => {
                                kaijutsu_types::BlockFlowKind::CollapsedChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::ExcludedChanged => {
                                kaijutsu_types::BlockFlowKind::ExcludedChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::Moved => {
                                kaijutsu_types::BlockFlowKind::Moved
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::SyncReset => {
                                kaijutsu_types::BlockFlowKind::SyncReset
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::OutputChanged => {
                                kaijutsu_types::BlockFlowKind::OutputChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::MetadataChanged => {
                                kaijutsu_types::BlockFlowKind::MetadataChanged
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::ContextSwitched => {
                                kaijutsu_types::BlockFlowKind::ContextSwitched
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::RenderCue => {
                                kaijutsu_types::BlockFlowKind::RenderCue
                            }
                            crate::kaijutsu_capnp::BlockFlowKind::BeatSync => {
                                kaijutsu_types::BlockFlowKind::BeatSync
                            }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    let block_kinds = if reader.get_has_block_kinds() {
        reader
            .get_block_kinds()
            .map(|list| {
                list.iter()
                    .filter_map(|k| {
                        k.ok().map(|k| match k {
                            crate::kaijutsu_capnp::BlockKind::Text => BlockKind::Text,
                            crate::kaijutsu_capnp::BlockKind::Thinking => BlockKind::Thinking,
                            crate::kaijutsu_capnp::BlockKind::ToolCall => BlockKind::ToolCall,
                            crate::kaijutsu_capnp::BlockKind::ToolResult => BlockKind::ToolResult,
                            crate::kaijutsu_capnp::BlockKind::Drift => BlockKind::Drift,
                            crate::kaijutsu_capnp::BlockKind::File => BlockKind::File,
                            crate::kaijutsu_capnp::BlockKind::Error => BlockKind::Error,
                            crate::kaijutsu_capnp::BlockKind::Notification => BlockKind::Notification,
                            crate::kaijutsu_capnp::BlockKind::Resource => BlockKind::Resource,
                            crate::kaijutsu_capnp::BlockKind::Trace => BlockKind::Trace,
                            crate::kaijutsu_capnp::BlockKind::Task => BlockKind::Task,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };

    kaijutsu_types::BlockEventFilter {
        context_ids,
        event_types,
        block_kinds,
    }
}

/// Fill an `EditorState` capnp builder from the kernel's editor state + the
/// session id (carried alongside, since the kernel state struct omits it).
/// `mode: None` maps to `""` on the wire.
fn set_editor_state(
    mut b: crate::kaijutsu_capnp::editor_state::Builder<'_>,
    session_id: u64,
    state: &kaijutsu_kernel::editor::EditorState,
) {
    b.set_session(session_id);
    b.set_text(&state.text);
    b.set_cursor(state.cursor as u64);
    b.set_mode(state.mode.as_deref().unwrap_or(""));
    b.set_dirty(state.dirty);
    b.set_command_line(state.command_line.as_deref().unwrap_or(""));
    b.set_message(state.message.as_deref().unwrap_or(""));
}

// ============================================================================
// Block-event bridge — FlowBus → BlockEvents callback
// ============================================================================
//
// One implementation, shared by `subscribe_blocks` and
// `subscribe_blocks_filtered`, which used to carry two ~250-line copies of the
// same per-variant dispatch.
//
// Three properties it owes the client (2026-08-05 backpressure rework):
//
//  1. **Lossless or loud.** The kernel bus hands this task a terminated
//     subscription rather than a hole; the bridge relays that as
//     `onSubscriptionTerminated` and then drops the connection, so the client
//     reconnects and resyncs instead of rendering a stale turn forever.
//  2. **Coalesced firehose.** Consecutive per-token text ops for one block are
//     merged over a ~15 ms window into a single `onBlockTextOpsBatch` — the
//     round-trip per token was the reason the queue backed up at all. Content
//     is preserved exactly: the ops ride as a list, applied in order, because
//     each blob is an independently-encoded DTE op set and concatenating the
//     bytes would be corruption.
//  3. **Musical time is never delayed.** The coalescing window closes the
//     instant anything that is not a text op arrives, and the bus already
//     drains the timing lane ahead of the ordered one.

/// Per-callback wall-clock bound for a block bridge.
///
/// Capnp callbacks share the SSH socket with all RPC traffic; if the client's
/// read side has stalled, the server's write buffer fills and `promise.await`
/// blocks indefinitely, which blocks the capnp RpcSystem from polling reads —
/// the exact wedge we observed (CLOSE_WAIT, unread bytes). 5s is generous for a
/// healthy peer and short enough that one stuck callback can't pin the LocalSet.
const BLOCK_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the bridge lingers collecting more text ops for the same block
/// before flushing them as one batch.
///
/// 15 ms is below the threshold where a human reads streaming text as anything
/// but continuous, and at typical token rates it merges tens of ops into one
/// call. It is a *ceiling*, not a fixed delay: anything that is not a text op
/// closes the window immediately, and a batch that fills flushes at once.
const TEXT_OPS_COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(15);

/// Most ops merged into one `onBlockTextOpsBatch` call. Bounds the size of a
/// single capnp message so a long stall can't produce one enormous frame.
const TEXT_OPS_COALESCE_MAX: usize = 512;

/// Most events pulled off the bus in one drain pass before we start sending.
const BRIDGE_DRAIN_MAX: usize = 1024;

/// What a connection's push subscriptions can handle, as the client declared it
/// via `declareEventCapabilities`.
///
/// Defaults are the conservative shape on purpose: a binary that predates the
/// rework never declares anything, so it keeps receiving exactly the events it
/// already understands. No probing, no unimplemented-error fallback, no
/// guessing from a version string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventCapabilities {
    /// Client implements `BlockEvents.onBlockTextOpsBatch`.
    pub text_ops_batch: bool,
    /// Client implements `BlockEvents.onSubscriptionTerminated`.
    pub subscription_terminated: bool,
}

/// Per-subscription delivery counters handed to the client as `subSeq`.
///
/// Two lanes, counted separately, because they promise different things: a gap
/// in `ordered` can only mean "terminated or resubscribed", while a gap in
/// `timing` is the honest report that beats were missed (docs/midi.md).
#[derive(Default)]
struct BridgeSeq {
    ordered: u64,
    timing: u64,
}

impl BridgeSeq {
    fn next(&mut self, topic: &str) -> u64 {
        match <BlockFlow as kaijutsu_kernel::flows::FlowTopics>::topic_class(topic) {
            kaijutsu_kernel::flows::TopicClass::Timing => {
                self.timing += 1;
                self.timing
            }
            kaijutsu_kernel::flows::TopicClass::Ordered => {
                self.ordered += 1;
                self.ordered
            }
        }
    }

    /// Input-doc events ride the ordered lane too — they are content, not time.
    fn next_ordered(&mut self) -> u64 {
        self.ordered += 1;
        self.ordered
    }
}

/// One item the bridge has pulled off a bus and still owes the client.
enum BridgeItem {
    Block(FlowMessage<BlockFlow>),
    Input(FlowMessage<InputDocFlow>),
}

/// Forward ONE block event. Returns whether the peer accepted it.
async fn send_block_event(
    callback: &block_events::Client,
    payload: &BlockFlow,
    sub_seq: u64,
    kernel_id: impl std::fmt::Display + Copy,
) -> bool {
    match payload {
        BlockFlow::Inserted {
            context_id,
            block,
            after_id,
            ops,
            ..
        } => {
            let mut req = callback.on_block_inserted_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_has_after_id(after_id.is_some());
                if let Some(after) = after_id {
                    set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                }
                params.set_ops(ops);
                params.set_sub_seq(sub_seq);
                let mut block_state = params.init_block();
                set_block_snapshot(&mut block_state, block);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::Deleted {
            context_id,
            block_id,
            ..
        } => {
            let mut req = callback.on_block_deleted_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::StatusChanged {
            context_id,
            block_id,
            status,
            ..
        } => {
            let mut req = callback.on_block_status_changed_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_status(status_to_capnp(*status));
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::CollapsedChanged {
            context_id,
            block_id,
            collapsed,
            ..
        } => {
            let mut req = callback.on_block_collapsed_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_collapsed(*collapsed);
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::ExcludedChanged {
            context_id,
            block_id,
            excluded,
            ..
        } => {
            let mut req = callback.on_block_excluded_changed_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_excluded(*excluded);
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::Moved {
            context_id,
            block_id,
            after_id,
            ..
        } => {
            let mut req = callback.on_block_moved_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_has_after_id(after_id.is_some());
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                if let Some(after) = after_id {
                    set_block_id_builder(&mut params.reborrow().init_after_id(), after);
                }
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::TextOps {
            context_id,
            block_id,
            ops,
            seq_num,
            ..
        } => {
            let mut req = callback.on_block_text_ops_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_ops(ops);
                params.set_seq_num(*seq_num);
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::SyncReset {
            context_id,
            generation,
        } => {
            let mut req = callback.on_sync_reset_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_generation(*generation);
                params.set_sub_seq(sub_seq);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::OutputChanged {
            context_id,
            block_id,
            output,
            ..
        } => {
            let mut req = callback.on_block_output_changed_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                if let Some(output_data) = output {
                    build_output_data(params.reborrow().init_output(), output_data);
                }
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::MetadataChanged {
            context_id,
            block_id,
            metadata,
            ..
        } => {
            let mut req = callback.on_block_metadata_changed_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
                set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
                build_block_metadata(params.reborrow().init_metadata(), metadata);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::ContextSwitched { context_id } => {
            let mut req = callback.on_context_switched_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::RenderCue { context_id, cue } => {
            let mut req = callback.on_render_cue_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
                set_render_cue(params.reborrow().init_cue(), cue);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        BlockFlow::BeatSync {
            context_id,
            beat_ref,
        } => {
            let mut req = callback.on_beat_sync_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
                set_beat_ref(params.reborrow().init_beat_ref(), beat_ref);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
    }
}

/// Forward a run of consecutive text ops for ONE block as a single call.
///
/// Applying `ops` in order is byte-for-byte identical to the individual
/// `onBlockTextOps` calls it replaces — this trades N round trips for one, it
/// does not merge, reorder, or rewrite content.
async fn send_text_ops_batch(
    callback: &block_events::Client,
    context_id: ContextId,
    block_id: &kaijutsu_crdt::BlockId,
    ops: &[std::sync::Arc<[u8]>],
    first_seq_num: u64,
    sub_seq: u64,
    kernel_id: impl std::fmt::Display + Copy,
) -> bool {
    let mut req = callback.on_block_text_ops_batch_request();
    {
        let mut params = req.get();
        params.set_context_id(context_id.as_bytes());
        params.set_first_seq_num(first_seq_num);
        params.set_sub_seq(sub_seq);
        set_block_id_builder(&mut params.reborrow().init_block_id(), block_id);
        let mut list = params.init_ops(ops.len() as u32);
        for (i, blob) in ops.iter().enumerate() {
            list.set(i as u32, blob);
        }
    }
    await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
}

/// Forward one input-doc (compose scratchpad) event.
async fn send_input_event(
    callback: &block_events::Client,
    payload: &InputDocFlow,
    sub_seq: u64,
    kernel_id: impl std::fmt::Display + Copy,
) -> bool {
    match payload {
        InputDocFlow::TextOps {
            context_id,
            ops,
            seq_num,
            ..
        } => {
            let mut req = callback.on_input_text_ops_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_ops(ops);
                params.set_seq_num(*seq_num);
                params.set_sub_seq(sub_seq);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
        InputDocFlow::Cleared { context_id } => {
            let mut req = callback.on_input_cleared_request();
            {
                let mut params = req.get();
                params.set_context_id(context_id.as_bytes());
                params.set_sub_seq(sub_seq);
            }
            await_editor_callback(req.send().promise, BLOCK_CALLBACK_TIMEOUT, kernel_id).await
        }
    }
}

/// Tell the client its subscription was terminated, then take the connection
/// down so it reconnects and resyncs.
///
/// Best-effort and short-deadline on purpose: the client we are talking to is
/// by definition not draining well, so we must not spend the full callback
/// budget explaining ourselves. Dropping the connection is what actually makes
/// the client recover — the courtesy call only tells it *why*, and an older
/// binary that never declared the capability recovers just the same, via the
/// ordinary reconnect path.
async fn kick_slow_subscriber(
    callback: &block_events::Client,
    info: &kaijutsu_kernel::flows::FlowTermination,
    caps: EventCapabilities,
    kernel_id: impl std::fmt::Display + Copy,
    label: &str,
) {
    tracing::error!(
        bridge = label,
        kernel = %kernel_id,
        topic = info.topic,
        delivered = info.delivered,
        capacity = info.capacity,
        "client fell behind its event queue — terminating the subscription and \
         dropping the connection so it resyncs (no lossy delivery)"
    );
    if caps.subscription_terminated {
        const KICK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
        let mut req = callback.on_subscription_terminated_request();
        {
            let mut p = req.get();
            p.set_reason(crate::kaijutsu_capnp::SubscriptionEndReason::SlowSubscriber);
            p.set_topic(info.topic);
            p.set_delivered(info.delivered);
            p.set_capacity(info.capacity as u64);
        }
        if tokio::time::timeout(KICK_TIMEOUT, req.send().promise)
            .await
            .is_err()
        {
            log::debug!("slow subscriber did not even accept its termination notice");
        }
    }
}

/// Is this message a text-op event (the coalescable one)?
fn text_ops_parts(msg: &FlowMessage<BlockFlow>) -> Option<(ContextId, kaijutsu_crdt::BlockId)> {
    match &msg.payload {
        BlockFlow::TextOps {
            context_id,
            block_id,
            ..
        } => Some((*context_id, *block_id)),
        _ => None,
    }
}

/// The shared FlowBus→BlockEvents bridge loop.
///
/// `filter` is `None` for `subscribe_blocks` (everything) and `Some` for
/// `subscribe_blocks_filtered`; the two RPCs differ in nothing else.
#[allow(clippy::too_many_arguments)]
async fn run_block_bridge(
    callback: block_events::Client,
    mut block_sub: kaijutsu_kernel::flows::Subscription<BlockFlow>,
    mut input_sub: Option<kaijutsu_kernel::flows::Subscription<InputDocFlow>>,
    filter: Option<kaijutsu_types::BlockEventFilter>,
    caps: EventCapabilities,
    kernel_id: kaijutsu_types::KernelId,
    conn_cancel: CancellationToken,
    disconnect: CancellationToken,
    label: &'static str,
) {
    use kaijutsu_kernel::flows::FlowRecv;

    let mut health = SubscriberHealth::new(SUBSCRIBER_FAILURE_STREAK_TIMEOUT);
    let mut seq = BridgeSeq::default();
    let mut pending: Vec<BridgeItem> = Vec::new();

    // Drain everything already queued, without waiting. Returns Err(term) if
    // the bus terminated us, Ok(false) if a stream closed.
    macro_rules! drain_ready {
        () => {{
            let mut outcome: Result<bool, kaijutsu_kernel::flows::FlowTermination> = Ok(true);
            while pending.len() < BRIDGE_DRAIN_MAX {
                match block_sub.try_recv_event() {
                    Some(FlowRecv::Message(m)) => pending.push(BridgeItem::Block(m)),
                    Some(FlowRecv::Terminated(info)) => {
                        outcome = Err(info);
                        break;
                    }
                    None => break,
                }
            }
            if outcome.is_ok()
                && let Some(sub) = input_sub.as_mut()
            {
                while pending.len() < BRIDGE_DRAIN_MAX {
                    match sub.try_recv_event() {
                        Some(FlowRecv::Message(m)) => pending.push(BridgeItem::Input(m)),
                        Some(FlowRecv::Terminated(info)) => {
                            outcome = Err(info);
                            break;
                        }
                        None => break,
                    }
                }
            }
            outcome
        }};
    }

    log::debug!("{label} bridge online for kernel {}", kernel_id.to_hex());

    'outer: loop {
        // 1. Take everything that is already sitting in our queues. Draining
        //    before awaiting is what makes the `select!` below safe to cancel:
        //    we never park while our own mailbox has work in it.
        match drain_ready!() {
            Ok(_) => {}
            Err(info) => {
                kick_slow_subscriber(&callback, &info, caps, kernel_id, label).await;
                disconnect.cancel();
                break;
            }
        }

        // 2. Nothing ready — wait for the next event (or the connection dying).
        if pending.is_empty() {
            // Which bus woke us. Both arms are cancel-safe because we always
            // drain before parking: a lost `Notify` permit can only ever
            // correspond to an event we have already taken.
            enum Woke {
                Block(FlowRecv<BlockFlow>),
                Input(FlowRecv<InputDocFlow>),
                Closed,
            }
            let woke = tokio::select! {
                _ = conn_cancel.cancelled() => {
                    log::debug!("{label} bridge cancelled with connection");
                    break;
                }
                ev = block_sub.recv_event() => ev.map(Woke::Block).unwrap_or(Woke::Closed),
                ev = async {
                    match input_sub.as_mut() {
                        Some(sub) => sub.recv_event().await,
                        None => std::future::pending().await,
                    }
                } => ev.map(Woke::Input).unwrap_or(Woke::Closed),
            };
            match woke {
                Woke::Closed => break,
                Woke::Block(FlowRecv::Message(m)) => pending.push(BridgeItem::Block(m)),
                Woke::Input(FlowRecv::Message(m)) => pending.push(BridgeItem::Input(m)),
                Woke::Block(FlowRecv::Terminated(info))
                | Woke::Input(FlowRecv::Terminated(info)) => {
                    kick_slow_subscriber(&callback, &info, caps, kernel_id, label).await;
                    disconnect.cancel();
                    break;
                }
            }
            match drain_ready!() {
                Ok(_) => {}
                Err(info) => {
                    kick_slow_subscriber(&callback, &info, caps, kernel_id, label).await;
                    disconnect.cancel();
                    break;
                }
            }
        }

        // 3. Coalescing window. Only when the batch currently ENDS in a text op
        //    and the client can accept a batch — otherwise there is nothing to
        //    merge and no reason to wait. Anything that is not a text op closes
        //    the window at once, so a beat, a status patch, or a render cue is
        //    never held back (docs/midi.md: musical time is latency-first).
        if caps.text_ops_batch
            && pending.len() < TEXT_OPS_COALESCE_MAX
            && matches!(pending.last(), Some(BridgeItem::Block(m)) if text_ops_parts(m).is_some())
        {
            let deadline = tokio::time::Instant::now() + TEXT_OPS_COALESCE_WINDOW;
            while pending.len() < TEXT_OPS_COALESCE_MAX {
                let next = tokio::select! {
                    _ = conn_cancel.cancelled() => break,
                    ev = tokio::time::timeout_at(deadline, block_sub.recv_event()) => ev,
                };
                match next {
                    // Window expired — flush what we have.
                    Err(_) => break,
                    Ok(None) => break,
                    Ok(Some(FlowRecv::Terminated(info))) => {
                        kick_slow_subscriber(&callback, &info, caps, kernel_id, label).await;
                        disconnect.cancel();
                        break 'outer;
                    }
                    Ok(Some(FlowRecv::Message(m))) => {
                        let still_ops = text_ops_parts(&m).is_some();
                        pending.push(BridgeItem::Block(m));
                        if !still_ops {
                            break;
                        }
                    }
                }
            }
        }

        // 4. Ship the batch, preserving publish order exactly.
        let mut i = 0;
        while i < pending.len() {
            let success = match &pending[i] {
                BridgeItem::Input(msg) => {
                    let s = seq.next_ordered();
                    let ok = send_input_event(&callback, &msg.payload, s, kernel_id).await;
                    i += 1;
                    ok
                }
                BridgeItem::Block(msg) => {
                    // Server-side filter before serializing to the wire.
                    if let Some(f) = &filter
                        && !msg.payload.matches_filter(f)
                    {
                        i += 1;
                        continue;
                    }
                    match text_ops_parts(msg) {
                        // A run of consecutive ops for the SAME block collapses
                        // into one call. `caps.text_ops_batch` gates it, so an
                        // older client still gets one call per op.
                        Some((ctx, block_id)) if caps.text_ops_batch => {
                            let mut ops: Vec<std::sync::Arc<[u8]>> = Vec::new();
                            let mut first_seq_num = 0u64;
                            let mut j = i;
                            while j < pending.len() {
                                let BridgeItem::Block(m) = &pending[j] else {
                                    break;
                                };
                                let Some((c, b)) = text_ops_parts(m) else {
                                    break;
                                };
                                if c != ctx || b != block_id {
                                    break;
                                }
                                if let BlockFlow::TextOps { ops: o, seq_num, .. } = &m.payload {
                                    if ops.is_empty() {
                                        first_seq_num = *seq_num;
                                    }
                                    ops.push(o.clone());
                                }
                                j += 1;
                            }
                            let s = seq.next("block.text_ops");
                            let ok = if ops.len() == 1 {
                                send_block_event(&callback, &pending[i].block_payload(), s, kernel_id)
                                    .await
                            } else {
                                send_text_ops_batch(
                                    &callback,
                                    ctx,
                                    &block_id,
                                    &ops,
                                    first_seq_num,
                                    s,
                                    kernel_id,
                                )
                                .await
                            };
                            i = j;
                            ok
                        }
                        _ => {
                            let s = seq.next(msg.topic);
                            let ok = send_block_event(&callback, &msg.payload, s, kernel_id).await;
                            i += 1;
                            ok
                        }
                    }
                }
            };

            // Tolerate a transient client-executor stall; reap only on
            // sustained failure (see SubscriberHealth). Breaking on the first
            // failure was the 2026-06-17 "every shell call times out after
            // restart" bug — it silently and permanently severed delivery.
            if !health.record(success) {
                log::warn!(
                    "{label} bridge for kernel {} stopping: callback failures \
                     continuous for over {:?} — reaping subscriber",
                    kernel_id,
                    SUBSCRIBER_FAILURE_STREAK_TIMEOUT,
                );
                break 'outer;
            }
        }
        pending.clear();
    }

    log::debug!("{label} bridge task for kernel {} ended", kernel_id.to_hex());
}

impl BridgeItem {
    /// The block payload, for the single-op fast path inside the batcher.
    fn block_payload(&self) -> BlockFlow {
        match self {
            BridgeItem::Block(m) => m.payload.clone(),
            BridgeItem::Input(_) => unreachable!("block_payload on an input item"),
        }
    }
}

/// Await an editor-callback round-trip with the shared callback timeout,
/// returning `true` on success (mirrors the per-event Ok/timeout/err handling
/// in `subscribe_blocks`). Generic over the callback's response type since the
/// bridge only cares whether the peer accepted the push.
async fn await_editor_callback<T>(
    promise: impl std::future::Future<Output = Result<T, capnp::Error>>,
    timeout: std::time::Duration,
    kernel_id: impl std::fmt::Display,
) -> bool {
    match tokio::time::timeout(timeout, promise).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            log::debug!("editor callback failed for {kernel_id}: {e}");
            false
        }
        Err(_) => {
            log::warn!(
                "editor callback timed out after {timeout:?} for kernel {kernel_id} \
                 — peer is not reading; dropping subscriber"
            );
            false
        }
    }
}

/// Failure tolerance for a FlowBus→client callback bridge.
///
/// The per-callback 5s timeout (see `CALLBACK_TIMEOUT`) is load-bearing: a
/// stalled client must not pin the server's RpcSystem by blocking
/// `promise.await` on the shared SSH socket. But a *single* timeout or error
/// does NOT mean the peer is dead — it usually means the client's
/// single-threaded executor was transiently busy (e.g. an MCP client mid
/// `from_sync_state`, or burst-processing a kernel-wide event stream). The old
/// behavior — `break` on the first failure — turned that transient stall into a
/// *permanent, silent* loss of the subscription: the bridge task ended, the
/// client was never told, and every later shell poll timed out forever (the
/// 2026-06-17 "every external shell call times out after restart" bug).
///
/// A *consecutive-strike* counter (3 failures ⇒ reap) fixed that but created a
/// second bug: a completed shell command fires a burst of ~4 events in quick
/// succession (TextOps, two MetadataChanged, StatusChanged), each forwarded as
/// its own callback. ONE transient write stall on the client's socket — busy
/// decoding a large prior response, say — times out every callback in that
/// burst, so 3 strikes land in under a second and the bridge is reaped even
/// though the peer is perfectly healthy (the 2026-07-17 "MCP shell calls time
/// out ~50% of the time" bug). A strike count can't tell "one blip that
/// happened to repeat" from "sustained wedge" apart.
///
/// So this tracks *streak duration* instead: the timestamp of the first
/// failure in the current unbroken losing streak, reaping only once a
/// subsequent failure lands `SUBSCRIBER_FAILURE_STREAK_TIMEOUT` or later after
/// that first failure. A burst-sized blip (well under a second) can no longer
/// trip it. A truly wedged/starved peer is still caught — it keeps failing
/// every callback, so the streak just keeps growing past the threshold,
/// reaped well before the ~90s SSH keepalive would tear the whole connection
/// down. A single success resets the streak.
#[derive(Debug)]
struct SubscriberHealth {
    /// Wall-clock time the current unbroken run of failures started. `None`
    /// when the last recorded outcome was a success — no streak in progress.
    streak_started_at: Option<std::time::Instant>,
    /// Failures recorded in the current streak. Duration alone would let a
    /// sparse subscriber (editor idle between edits) be reaped by just TWO
    /// failures that happen to straddle the window — one blip, 15s of
    /// silence, one blip. Requiring a minimum count too keeps the reap
    /// meaning "kept failing the whole time", not "failed twice, far apart".
    streak_failures: u32,
    /// How long a losing streak must run, unbroken, before `record` reaps it.
    min_streak_duration: std::time::Duration,
}

/// Failures a streak must also accumulate before reaping — see
/// `streak_failures`. A wedged subscriber failing at the 5s
/// `CALLBACK_TIMEOUT` cadence hits both this and the duration threshold on
/// the same (fourth) failure, so this changes nothing for genuine wedges.
const SUBSCRIBER_MIN_STREAK_FAILURES: u32 = 3;

impl SubscriberHealth {
    fn new(min_streak_duration: std::time::Duration) -> Self {
        Self {
            streak_started_at: None,
            streak_failures: 0,
            min_streak_duration,
        }
    }

    /// Record a callback outcome. Returns `true` to keep the subscription
    /// alive, `false` to reap it. A success clears the streak. A failure
    /// starts the streak (if none is running) or extends it, and reaps once
    /// the streak has both run continuously for `min_streak_duration` AND
    /// accumulated `SUBSCRIBER_MIN_STREAK_FAILURES` failures.
    fn record(&mut self, ok: bool) -> bool {
        if ok {
            self.streak_started_at = None;
            self.streak_failures = 0;
            true
        } else {
            let started = *self
                .streak_started_at
                .get_or_insert_with(std::time::Instant::now);
            self.streak_failures += 1;
            started.elapsed() < self.min_streak_duration
                || self.streak_failures < SUBSCRIBER_MIN_STREAK_FAILURES
        }
    }
}

/// Minimum duration a FlowBus/editor callback failure streak must run,
/// unbroken, before its subscriber is reaped. At the 5s per-callback
/// `CALLBACK_TIMEOUT`, a completed shell command's ~4-event burst can only
/// burn a few seconds even if every callback in it times out; 15s gives that
/// generous headroom while still catching a genuinely wedged subscriber well
/// inside the ~90s SSH keepalive window.
const SUBSCRIBER_FAILURE_STREAK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Floor on `subscribeVfsActivity`'s requested tick interval (Lane K, FSN
/// slice-1) — a client asking for anything tighter is silently clamped up
/// rather than rejected. Heat is decorative, not a control signal; there is
/// no reason to let a misconfigured client hammer the digest computation
/// faster than this.
const VFS_ACTIVITY_MIN_INTERVAL_MS: u32 = 500;

/// Default tick interval when a client passes `intervalMs = 0` ("use the
/// server's judgment").
const VFS_ACTIVITY_DEFAULT_INTERVAL_MS: u32 = 1000;

/// Hard cap on entries per activity digest tick — bounds one push's wire
/// size the same way `SNAPSHOT_MAX_ENTRIES` bounds a snapshot reply. Entries
/// beyond the cap aren't lost, just deferred: see `MountTable::
/// activity_digest`'s largest-delta-first capping and the two-phase
/// diff/commit that lets a dropped entry return on a later tick.
const VFS_ACTIVITY_DIGEST_MAX_ENTRIES: usize = 256;

/// Convert a CRDT Status to Cap'n Proto Status.
fn status_to_capnp(status: kaijutsu_crdt::Status) -> crate::kaijutsu_capnp::Status {
    match status {
        kaijutsu_crdt::Status::Pending => crate::kaijutsu_capnp::Status::Pending,
        kaijutsu_crdt::Status::Running => crate::kaijutsu_capnp::Status::Running,
        kaijutsu_crdt::Status::Done => crate::kaijutsu_capnp::Status::Done,
        kaijutsu_crdt::Status::Error => crate::kaijutsu_capnp::Status::Error,
    }
}

/// Resolve one context's `(contextWindow, contextUsedTokens, contextUsedPct)`
/// wire triple (`ContextHandleInfo` in `kaijutsu.capnp`) from its optional
/// usage snapshot and the window already resolved for it (via
/// `LlmRegistry::context_window_for_live`, awaited by the caller since it
/// needs the registry lock the loop in `list_contexts` holds once for the
/// whole listing).
///
/// Pulled out of `list_contexts` so the sentinel selection — 0 = unknown
/// window/no usage, -1.0 = unknown percentage — is unit-testable
/// independent of the capnp builder (`context_usage_wire_tests` below).
/// Never fabricates: no usage row means every field is the honest
/// "unknown"/"absent" sentinel, never a guessed number.
fn resolve_usage_wire_fields(
    usage: Option<&ContextUsageRow>,
    window: Option<u64>,
) -> (u64, u64, f32) {
    match usage {
        Some(u) => {
            let pct = context_used_pct(u, window);
            (
                window.unwrap_or(0),
                u.total_tokens() as u64,
                pct.map(|p| p as f32).unwrap_or(-1.0),
            )
        }
        None => (0, 0, -1.0),
    }
}

#[cfg(test)]
mod context_usage_wire_tests {
    //! `resolve_usage_wire_fields` — the exact plumbing `list_contexts` uses
    //! to fill `ContextHandleInfo.contextWindow` / `contextUsedTokens` /
    //! `contextUsedPct`. Pins down the sentinel selection this task's
    //! `-1.0` convention exists for: a genuinely 0%-used KNOWN window must
    //! never collide with "window unknown" or "no usage yet".
    use super::{resolve_usage_wire_fields, ContextUsageRow};
    use kaijutsu_types::ContextId;

    fn usage(input_tokens: i64, output_tokens: i64) -> ContextUsageRow {
        ContextUsageRow {
            context_id: ContextId::new(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            updated_at: 1,
        }
    }

    #[test]
    fn no_usage_row_is_every_sentinel() {
        assert_eq!(resolve_usage_wire_fields(None, None), (0, 0, -1.0));
        // A window resolved with no usage would be a contradiction the
        // caller shouldn't produce, but the helper still refuses to
        // fabricate a percentage/token count from it.
        assert_eq!(resolve_usage_wire_fields(None, Some(200_000)), (0, 0, -1.0));
    }

    #[test]
    fn usage_present_window_unknown_shows_tokens_but_unknown_pct() {
        let u = usage(40_000, 10_000);
        assert_eq!(resolve_usage_wire_fields(Some(&u), None), (0, 50_000, -1.0));
    }

    /// The case the `-1.0` sentinel exists for: a genuinely 0%-used
    /// context against a KNOWN window must be `Some(0.0)`-shaped
    /// (`(window, 0, 0.0)`), never confused with the unknown triple above.
    #[test]
    fn zero_usage_known_window_is_real_zero_not_unknown() {
        let u = usage(0, 0);
        assert_eq!(
            resolve_usage_wire_fields(Some(&u), Some(200_000)),
            (200_000, 0, 0.0)
        );
    }

    #[test]
    fn usage_and_window_both_known_computes_pct() {
        let u = usage(40_000, 10_000);
        assert_eq!(
            resolve_usage_wire_fields(Some(&u), Some(200_000)),
            (200_000, 50_000, 25.0)
        );
    }
}

/// Resolve one context's background-process ambient-state wire quintuple
/// (`ContextHandleInfo.background*`, `kaijutsu.capnp` @23-@27) from its
/// `BackgroundSummary` (`kaijutsu_kernel::background_exec`). Pulled out of
/// `list_contexts` so the sentinel selection — `0`/empty-string/`-1` for
/// "nothing running, nothing finished" — is unit-testable independent of
/// the capnp builder (`background_wire_tests` below). Never fabricates: a
/// context with no background history at all gets every field's honest
/// "none" sentinel, same spirit as `resolve_usage_wire_fields` above.
fn resolve_background_wire_fields(
    summary: &kaijutsu_kernel::background_exec::BackgroundSummary,
) -> (u32, u64, u64, &'static str, i32) {
    let oldest_running = summary.oldest_running_started_at_unix_ms.unwrap_or(0);
    match &summary.last_finished {
        Some(f) => (
            summary.running_count,
            oldest_running,
            f.finished_at_unix_ms,
            f.status,
            f.exit_code.unwrap_or(-1),
        ),
        None => (summary.running_count, oldest_running, 0, "", -1),
    }
}

#[cfg(test)]
mod background_wire_tests {
    //! `resolve_background_wire_fields` — the exact plumbing `list_contexts`
    //! uses to fill `ContextHandleInfo.background*`. Pins down the sentinel
    //! convention: `0`/empty/`-1` mean "nothing to report", and must never
    //! collide with a real (and legitimately zero-ish) value — mirroring
    //! `context_usage_wire_tests` above for the `contextUsedPct` sentinel.
    use super::resolve_background_wire_fields;
    use kaijutsu_kernel::background_exec::{BackgroundFinishedSummary, BackgroundSummary};

    #[test]
    fn nothing_running_or_finished_is_every_sentinel() {
        let summary = BackgroundSummary::default();
        assert_eq!(
            resolve_background_wire_fields(&summary),
            (0, 0, 0, "", -1),
            "a context with no background history must get every honest 'none' sentinel"
        );
    }

    #[test]
    fn running_only_reports_count_and_oldest_but_no_finished_fields() {
        let summary = BackgroundSummary {
            running_count: 2,
            oldest_running_started_at_unix_ms: Some(1_000),
            last_finished: None,
        };
        assert_eq!(
            resolve_background_wire_fields(&summary),
            (2, 1_000, 0, "", -1),
            "running state must not fabricate a finished outcome"
        );
    }

    /// The case the `-1` sentinel exists for: a real exit code of `0` (a
    /// perfectly ordinary successful exit) must round-trip as `Some(0)`-shaped
    /// wire data, never confused with "no exit code to report".
    #[test]
    fn exited_zero_is_a_real_zero_not_unknown() {
        let summary = BackgroundSummary {
            running_count: 0,
            oldest_running_started_at_unix_ms: None,
            last_finished: Some(BackgroundFinishedSummary {
                status: "exited",
                exit_code: Some(0),
                finished_at_unix_ms: 5_000,
            }),
        };
        assert_eq!(
            resolve_background_wire_fields(&summary),
            (0, 0, 5_000, "exited", 0),
            "a genuinely successful exit(0) must NOT be confused with 'no exit code'"
        );
    }

    #[test]
    fn exited_nonzero_reports_the_real_code() {
        let summary = BackgroundSummary {
            running_count: 0,
            oldest_running_started_at_unix_ms: None,
            last_finished: Some(BackgroundFinishedSummary {
                status: "exited",
                exit_code: Some(9),
                finished_at_unix_ms: 5_000,
            }),
        };
        assert_eq!(resolve_background_wire_fields(&summary), (0, 0, 5_000, "exited", 9));
    }

    /// A killed process has no exit code at all — the wire must carry the
    /// `-1` "none" sentinel, never a fabricated `0` or a signal number.
    #[test]
    fn killed_has_no_exit_code_wire_sentinel() {
        let summary = BackgroundSummary {
            running_count: 0,
            oldest_running_started_at_unix_ms: None,
            last_finished: Some(BackgroundFinishedSummary {
                status: "killed",
                exit_code: None,
                finished_at_unix_ms: 5_000,
            }),
        };
        assert_eq!(
            resolve_background_wire_fields(&summary),
            (0, 0, 5_000, "killed", -1),
            "a killed process must report the -1 'no exit code' sentinel, not a fabricated 0"
        );
    }

    /// Some still running AND a past finish both known at once (a job
    /// finished while another is still going) — both halves must surface
    /// together, neither one suppressing the other.
    #[test]
    fn running_and_last_finished_coexist() {
        let summary = BackgroundSummary {
            running_count: 1,
            oldest_running_started_at_unix_ms: Some(2_000),
            last_finished: Some(BackgroundFinishedSummary {
                status: "exited",
                exit_code: Some(1),
                finished_at_unix_ms: 1_500,
            }),
        };
        assert_eq!(
            resolve_background_wire_fields(&summary),
            (1, 2_000, 1_500, "exited", 1)
        );
    }
}

// ============================================================================
// VFS Implementation
// ============================================================================

struct VfsImpl {
    kernel: Arc<Kernel>,
}

impl VfsImpl {
    fn new(kernel: Arc<Kernel>) -> Self {
        Self { kernel }
    }
}

/// Convert VfsError to capnp Error
fn vfs_err_to_capnp(e: kaijutsu_kernel::VfsError) -> capnp::Error {
    capnp::Error::failed(format!("{}", e))
}

/// Helper to extract path string from capnp text reader
fn get_path_str(text: capnp::text::Reader<'_>) -> Result<String, capnp::Error> {
    text.to_str()
        .map(|s| s.to_owned())
        .map_err(|e| capnp::Error::failed(format!("invalid UTF-8: {}", e)))
}

/// Helper to build FileAttr result
fn set_file_attr(
    builder: &mut crate::kaijutsu_capnp::file_attr::Builder,
    attr: &kaijutsu_kernel::FileAttr,
) {
    builder.set_size(attr.size);
    builder.set_kind(match attr.kind {
        kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
        kaijutsu_kernel::FileType::Directory => crate::kaijutsu_capnp::FileType::Directory,
        kaijutsu_kernel::FileType::Symlink => crate::kaijutsu_capnp::FileType::Symlink,
    });
    builder.set_perm(attr.perm);
    // Convert SystemTime to duration since epoch
    let duration = attr
        .mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    builder.set_mtime_secs(duration.as_secs());
    builder.set_mtime_nanos(duration.subsec_nanos());
    builder.set_nlink(attr.nlink);
}

impl vfs::Server for VfsImpl {
    fn getattr(
        self: Rc<Self>,
        params: vfs::GetattrParams,
        mut results: vfs::GetattrResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .getattr(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn readdir(
        self: Rc<Self>,
        params: vfs::ReaddirParams,
        mut results: vfs::ReaddirResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let entries = kernel
                .readdir(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_entries(entries.len() as u32);
            for (i, entry) in entries.iter().enumerate() {
                let mut e = builder.reborrow().get(i as u32);
                e.set_name(&entry.name);
                e.set_kind(match entry.kind {
                    kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
                    kaijutsu_kernel::FileType::Directory => {
                        crate::kaijutsu_capnp::FileType::Directory
                    }
                    kaijutsu_kernel::FileType::Symlink => crate::kaijutsu_capnp::FileType::Symlink,
                });
            }
            Ok(())
        })
    }

    fn read(
        self: Rc<Self>,
        params: vfs::ReadParams,
        mut results: vfs::ReadResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let offset = params.get_offset();
        let size = params.get_size();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let data = kernel
                .read(Path::new(&path), offset, size)
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_data(&data);
            Ok(())
        })
    }

    fn readlink(
        self: Rc<Self>,
        params: vfs::ReadlinkParams,
        mut results: vfs::ReadlinkResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let target = kernel
                .readlink(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_target(target.to_string_lossy());
            Ok(())
        })
    }

    fn write(
        self: Rc<Self>,
        params: vfs::WriteParams,
        mut results: vfs::WriteResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let offset = params.get_offset();
        let data = match params.get_data() {
            Ok(d) => d.to_vec(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let written = kernel
                .write(Path::new(&path), offset, &data)
                .await
                .map_err(vfs_err_to_capnp)?;
            results.get().set_written(written);
            Ok(())
        })
    }

    fn create(
        self: Rc<Self>,
        params: vfs::CreateParams,
        mut results: vfs::CreateResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let mode = params.get_mode();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .create(Path::new(&path), mode)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn mkdir(
        self: Rc<Self>,
        params: vfs::MkdirParams,
        mut results: vfs::MkdirResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let mode = params.get_mode();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .mkdir(Path::new(&path), mode)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn unlink(
        self: Rc<Self>,
        params: vfs::UnlinkParams,
        _results: vfs::UnlinkResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .unlink(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn rmdir(
        self: Rc<Self>,
        params: vfs::RmdirParams,
        _results: vfs::RmdirResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .rmdir(Path::new(&path))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn rename(
        self: Rc<Self>,
        params: vfs::RenameParams,
        _results: vfs::RenameResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let from = match params.get_from().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let to = match params.get_to().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .rename(Path::new(&from), Path::new(&to))
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn truncate(
        self: Rc<Self>,
        params: vfs::TruncateParams,
        _results: vfs::TruncateResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let size = params.get_size();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            kernel
                .truncate(Path::new(&path), size)
                .await
                .map_err(vfs_err_to_capnp)?;
            Ok(())
        })
    }

    fn setattr(
        self: Rc<Self>,
        params: vfs::SetattrParams,
        mut results: vfs::SetattrResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let attr_reader = match params.get_attr() {
            Ok(a) => a,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };

        // Convert to kernel SetAttr
        let set_attr = kaijutsu_kernel::SetAttr {
            size: if attr_reader.get_has_size() {
                Some(attr_reader.get_size())
            } else {
                None
            },
            perm: if attr_reader.get_has_perm() {
                Some(attr_reader.get_perm())
            } else {
                None
            },
            mtime: if attr_reader.get_has_mtime() {
                Some(
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(attr_reader.get_mtime_secs()),
                )
            } else {
                None
            },
            atime: None, // Not in capnp schema
            uid: None,   // Not in capnp schema
            gid: None,   // Not in capnp schema
        };

        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .setattr(Path::new(&path), set_attr)
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_new_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn symlink(
        self: Rc<Self>,
        params: vfs::SymlinkParams,
        mut results: vfs::SymlinkResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let target = match params.get_target().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let attr = kernel
                .symlink(Path::new(&path), Path::new(&target))
                .await
                .map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_attr();
            set_file_attr(&mut builder, &attr);
            Ok(())
        })
    }

    fn read_only(
        self: Rc<Self>,
        _params: vfs::ReadOnlyParams,
        mut results: vfs::ReadOnlyResults,
    ) -> Promise<(), capnp::Error> {
        results.get().set_read_only(self.kernel.vfs().read_only());
        Promise::ok(())
    }

    fn statfs(
        self: Rc<Self>,
        _params: vfs::StatfsParams,
        mut results: vfs::StatfsResults,
    ) -> Promise<(), capnp::Error> {
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let stat = kernel.statfs().await.map_err(vfs_err_to_capnp)?;
            let mut builder = results.get().init_stat();
            builder.set_blocks(stat.blocks);
            builder.set_bfree(stat.bfree);
            builder.set_bavail(stat.bavail);
            builder.set_files(stat.files);
            builder.set_ffree(stat.ffree);
            builder.set_bsize(stat.bsize);
            builder.set_namelen(stat.namelen);
            Ok(())
        })
    }

    fn real_path(
        self: Rc<Self>,
        params: vfs::RealPathParams,
        mut results: vfs::RealPathResults,
    ) -> Promise<(), capnp::Error> {
        let path = match params.get().and_then(|p| p.get_path()) {
            Ok(p) => match p.to_str() {
                Ok(s) => s.to_owned(),
                Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
            },
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            match kernel.real_path(Path::new(&path)).await {
                Ok(Some(real)) => {
                    results.get().set_real_path(real.to_string_lossy());
                    Ok(())
                }
                Ok(None) => {
                    // Virtual backend (MemoryBackend) - return empty string
                    results.get().set_real_path("");
                    Ok(())
                }
                Err(e) => Err(vfs_err_to_capnp(e)),
            }
        })
    }

    fn snapshot(
        self: Rc<Self>,
        params: vfs::SnapshotParams,
        mut results: vfs::SnapshotResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let path = match params.get_path().and_then(|p| get_path_str(p)) {
            Ok(s) => s.to_owned(),
            Err(e) => return Promise::err(capnp::Error::failed(format!("{}", e))),
        };
        let depth = params.get_depth();
        let max_entries = params.get_max_entries();
        let kernel = self.kernel.clone();

        Promise::from_future(async move {
            let snap = kernel
                .snapshot(Path::new(&path), depth, max_entries)
                .await
                .map_err(vfs_err_to_capnp)?;
            {
                let mut root_builder = results.get().init_root();
                set_snapshot_node(&mut root_builder, &snap.root);
            }
            results.get().set_generation(snap.generation);
            results.get().set_truncated(snap.truncated);
            Ok(())
        })
    }
}

/// Recursively fill a capnp `SnapshotNode` builder from the owned kernel tree
/// (`Vfs.snapshot`, FSN world stage-0/1 plumbing — docs/scenes/vfs.md).
fn set_snapshot_node(
    builder: &mut crate::kaijutsu_capnp::snapshot_node::Builder,
    node: &kaijutsu_kernel::SnapshotNode,
) {
    builder.set_name(&node.name);
    builder.set_kind(match node.kind {
        kaijutsu_kernel::FileType::File => crate::kaijutsu_capnp::FileType::File,
        kaijutsu_kernel::FileType::Directory => crate::kaijutsu_capnp::FileType::Directory,
        kaijutsu_kernel::FileType::Symlink => crate::kaijutsu_capnp::FileType::Symlink,
    });
    builder.set_size(node.size);
    builder.set_mtime_secs(node.mtime_secs);
    builder.set_child_count(node.child_count);
    builder.set_ignored(node.ignored);
    builder.set_generation(node.generation);
    builder.set_truncated_here(node.truncated_here);
    builder.set_denied(node.denied);
    let mut children = builder.reborrow().init_children(node.children.len() as u32);
    for (i, child) in node.children.iter().enumerate() {
        let mut child_builder = children.reborrow().get(i as u32);
        set_snapshot_node(&mut child_builder, child);
    }
}

// ============================================================================
// Synthesis (Rhai-driven keyword extraction + representative block selection)
// ============================================================================

#[cfg(test)]
mod filtered_subscribe_pattern_tests {
    //! The filtered block-subscription FlowBus pattern must always be a superset
    //! of what `matches_filter` passes — in particular it must never narrow away
    //! `RenderCue` directives (which `matches_filter` passes unconditionally).
    use super::filtered_subscribe_pattern;
    use kaijutsu_types::BlockFlowKind::*;

    #[test]
    fn lone_directive_filter_can_use_its_own_topic() {
        assert_eq!(filtered_subscribe_pattern(&[RenderCue]), "block.render_cue");
    }

    #[test]
    fn single_non_directive_filter_widens_to_catch_directives() {
        // The old inline code narrowed these to "block.status" / "block.text_ops"
        // and silently dropped RenderCue directives — this is the regression guard.
        assert_eq!(filtered_subscribe_pattern(&[StatusChanged]), "block.*");
        assert_eq!(filtered_subscribe_pattern(&[TextOps]), "block.*");
        assert_eq!(filtered_subscribe_pattern(&[Inserted]), "block.*");
    }

    #[test]
    fn empty_and_multi_type_filters_widen() {
        assert_eq!(filtered_subscribe_pattern(&[]), "block.*");
        assert_eq!(filtered_subscribe_pattern(&[StatusChanged, Inserted]), "block.*");
    }
}

#[cfg(test)]
mod live_status_tests {
    //! `derive_context_live_status` — the per-context pulse signal the time well
    //! reads off `listContexts`. Locks in the running-wins / error-only-if-tail /
    //! non-sticky semantics.
    use super::derive_context_live_status;
    use kaijutsu_crdt::Status;

    #[test]
    fn empty_is_idle() {
        assert_eq!(derive_context_live_status(&[]), Status::Pending);
    }

    #[test]
    fn all_done_is_idle() {
        assert_eq!(
            derive_context_live_status(&[Status::Done, Status::Done]),
            Status::Pending
        );
    }

    #[test]
    fn any_running_wins() {
        // A running block anywhere means the context is actively working, even if
        // the tail block is already Done or the head errored earlier.
        assert_eq!(
            derive_context_live_status(&[Status::Done, Status::Running, Status::Done]),
            Status::Running
        );
        assert_eq!(
            derive_context_live_status(&[Status::Error, Status::Running]),
            Status::Running
        );
    }

    #[test]
    fn tail_error_is_error() {
        assert_eq!(
            derive_context_live_status(&[Status::Done, Status::Error]),
            Status::Error
        );
    }

    #[test]
    fn non_tail_error_is_superseded() {
        // An error that is not the most recent block has been superseded by later
        // work — no red rim. (Non-sticky: a new turn appended Done after it.)
        assert_eq!(
            derive_context_live_status(&[Status::Error, Status::Done]),
            Status::Pending
        );
    }
}

#[cfg(test)]
mod subscriber_health_tests {
    //! `SubscriberHealth` — tracks *streak duration*, not strike count, so a
    //! transient client stall can't permanently sever a FlowBus subscription
    //! (2026-06-17 "every external shell call times out after restart") NOR
    //! can a completed shell command's ~4-event completion burst — all of
    //! which can time out from one blip — trip a false reap (2026-07-17 "MCP
    //! shell calls time out ~50% of the time"). Reaping is for a failure
    //! streak that runs *continuously* past the threshold; a success resets.
    use super::SubscriberHealth;
    use std::time::Duration;

    #[test]
    fn single_failure_does_not_reap() {
        let mut h = SubscriberHealth::new(Duration::from_secs(15));
        // One transient timeout must NOT kill the subscription — this is the
        // whole bug: the old code broke here and the MCP went silent forever.
        assert!(h.record(false), "a single failure must keep the bridge alive");
    }

    #[test]
    fn burst_of_failures_within_window_does_not_reap() {
        // A completed shell command fans out ~4 callbacks back-to-back; if a
        // transient write stall times out all of them, that whole burst lands
        // in well under a second. None of it should trip a threshold this
        // much larger than the burst.
        let mut h = SubscriberHealth::new(Duration::from_millis(200));
        for n in 1..=4 {
            assert!(
                h.record(false),
                "burst failure #{n} landed inside the window — must not reap"
            );
        }
    }

    #[test]
    fn failures_spanning_past_threshold_reap() {
        // A genuinely wedged/starved subscriber keeps failing every callback;
        // once that losing streak has run continuously past the threshold
        // (and isn't just a pair of blips — see the sparse test below), the
        // next failure reaps it.
        let threshold = Duration::from_millis(30);
        let mut h = SubscriberHealth::new(threshold);
        assert!(h.record(false), "first failure starts the streak, not a reap");
        assert!(h.record(false), "second failure inside the window survives");
        std::thread::sleep(threshold * 2);
        assert!(
            !h.record(false),
            "a sustained streak past the threshold with enough failures reaps"
        );
    }

    #[test]
    fn two_sparse_failures_straddling_the_window_do_not_reap() {
        // A low-frequency subscriber (editor idle between edits) can go 15s+
        // between callbacks. One blip, a long quiet stretch, then another
        // blip technically *spans* the duration threshold — but two isolated
        // failures are not a wedge, and reaping here would be harsher than
        // even the old 3-strike rule. The minimum-failure-count guard exists
        // for exactly this shape (kaibo or-kimi review find, 2026-07-17).
        let threshold = Duration::from_millis(30);
        let mut h = SubscriberHealth::new(threshold);
        assert!(h.record(false), "first blip survives");
        std::thread::sleep(threshold * 2);
        assert!(
            h.record(false),
            "a second, far-apart blip is not a sustained wedge — must not reap"
        );
        assert!(
            !h.record(false),
            "but a third failure in the same long-running streak does reap"
        );
    }

    #[test]
    fn success_resets_the_streak() {
        let threshold = Duration::from_millis(30);
        let mut h = SubscriberHealth::new(threshold);
        assert!(h.record(false), "streak starts");
        std::thread::sleep(threshold * 2);
        assert!(h.record(true), "success clears the streak before it reaps");
        // The streak restarted at the success above, so a fresh failure right
        // now is well inside the threshold again — even though the *first*
        // failure was long enough ago that an un-reset streak would reap.
        assert!(
            h.record(false),
            "streak was reset by the success — a fresh failure must not reap"
        );
    }

    #[test]
    fn steady_success_never_reaps() {
        let mut h = SubscriberHealth::new(Duration::from_secs(15));
        for _ in 0..100 {
            assert!(h.record(true));
        }
    }
}

#[cfg(test)]
mod shell_value_conversion_tests {
    //! `value_to_shell_value` / `shell_value_to_value` — the kaish `ast::Value`
    //! ⇄ Cap'n Proto `ShellValue` bridge. Locks in the kaish 0.9 migration where
    //! `Value::Blob(BlobRef)` became `Value::Bytes(Vec<u8>)`: inline binary must
    //! survive the wire byte-for-byte (no base64-string fudge, no lossy decode),
    //! and a legacy `blob` from an older peer must surface as a String path.
    use super::{shell_value_to_value, value_to_shell_value};
    use crate::kaijutsu_capnp::shell_value;
    use kaish_kernel::ast::Value;

    fn round_trip(value: &Value) -> Value {
        let mut msg = capnp::message::Builder::new_default();
        value_to_shell_value(msg.init_root::<shell_value::Builder>(), value);
        let reader = msg
            .get_root_as_reader::<shell_value::Reader>()
            .expect("read back shell_value");
        shell_value_to_value(reader).expect("decode shell_value")
    }

    #[test]
    fn bytes_round_trip_is_byte_exact() {
        // Includes a NUL and a non-UTF-8 byte (0xFF) — exactly the payloads a
        // base64-into-Text shortcut or a lossy String decode would corrupt.
        let original = Value::Bytes(vec![0x00, 0x01, 0xFF, 0xFE, b'h', b'i']);
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn empty_bytes_survive() {
        assert_eq!(round_trip(&Value::Bytes(vec![])), Value::Bytes(vec![]));
    }

    #[test]
    fn scalars_round_trip() {
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::Int(-7),
            Value::String("こんにちは".into()),
        ] {
            assert_eq!(round_trip(&v), v);
        }
    }

    #[test]
    fn legacy_blob_decodes_to_string_path() {
        // kaish 0.9 never produces `blob`, but an older peer might. It carries a
        // path string, so it must come back as a String — never mis-typed as
        // binary, never dropped.
        let mut msg = capnp::message::Builder::new_default();
        msg.init_root::<shell_value::Builder>()
            .set_blob("/v/cas/deadbeef");
        let reader = msg.get_root_as_reader::<shell_value::Reader>().unwrap();
        assert_eq!(
            shell_value_to_value(reader).unwrap(),
            Value::String("/v/cas/deadbeef".into())
        );
    }
}

#[cfg(test)]
mod connection_state_tests {
    //! Drop semantics for `ConnectionState`.
    //!
    //! These exist to lock in the wedge defenses added in 2026-05-10:
    //!   * Dropping the connection cancels `conn_cancel` so background
    //!     `spawn_local` tasks (FlowBus bridges, peer-invoke bridge) exit
    //!     promptly via `tokio::select!` rather than pinning the LocalSet.
    //!   * Dropping the connection removes the per-session entry from
    //!     `session_contexts` even if `run_rpc` never completes — the
    //!     explicit remove used to live at the tail of `run_rpc` and was
    //!     skipped when the RPC system wedged.
    use super::*;
    use kaijutsu_kernel::runtime::context_engine::session_context_map;

    fn test_principal() -> Principal {
        Principal {
            id: kaijutsu_types::PrincipalId::new(),
            username: "drop-test".into(),
            display_name: "drop-test".into(),
        }
    }

    #[test]
    fn drop_cancels_conn_cancel_token() {
        let session_contexts = session_context_map();
        let state = ConnectionState::new(test_principal(), session_contexts);
        let token = state.cancel_token();
        assert!(!token.is_cancelled(), "fresh token must not be cancelled");
        drop(state);
        assert!(
            token.is_cancelled(),
            "ConnectionState::Drop must cancel its conn_cancel token so \
             background spawn_local tasks unwind",
        );
    }

    #[test]
    fn drop_removes_session_contexts_entry() {
        let session_contexts = session_context_map();
        let ctx_id = kaijutsu_types::ContextId::new();
        let state = ConnectionState::new(test_principal(), session_contexts.clone());
        let session_id = state.session_id;
        // Simulate join_context inserting an active context for this session.
        session_contexts.insert(session_id, ctx_id);
        assert!(session_contexts.get(&session_id).is_some());
        drop(state);
        assert!(
            session_contexts.get(&session_id).is_none(),
            "ConnectionState::Drop must remove its session_id from \
             session_contexts so wedged threads don't leak entries",
        );
    }

    /// The crashed-sink hole, closed at the seam that actually fires: a sink
    /// that dies never sends `present=false`, so `ConnectionState::Drop` must
    /// take its presence records with it. Same lifecycle as the subscription
    /// and `session_contexts` cleanup above — no separate teardown to forget.
    #[test]
    fn drop_reaps_this_connections_midi_presence() {
        use kaijutsu_kernel::midi_presence::{
            MidiPresenceRecord, MidiPresenceStore, SinkAttribution,
        };

        let store = Arc::new(MidiPresenceStore::new());
        let mut state = ConnectionState::new(test_principal(), session_context_map());
        // What the report handler does on every report, before it records.
        state.attach_midi_presence(store.clone());
        let session_id = state.session_id;

        // What this connection's sink reported…
        store
            .record(MidiPresenceRecord::from_sink(
                "keystep-pro",
                true,
                "alsa",
                vec![],
                100,
                SinkAttribution::new(session_id, "moltar"),
            ))
            .unwrap();
        // …and what a *different* sink reported, which must survive.
        let other = kaijutsu_types::SessionId::new();
        store
            .record(MidiPresenceRecord::from_sink(
                "minibrute",
                true,
                "alsa",
                vec![],
                100,
                SinkAttribution::new(other, "zorak"),
            ))
            .unwrap();

        drop(state);

        assert!(
            store.get("keystep-pro").is_none(),
            "ConnectionState::Drop must un-know what a now-dead sink claimed \
             (removed → unknown, never a fabricated present=false)",
        );
        assert!(
            store.get("minibrute").is_some(),
            "another sink's connection is still up; its records stand",
        );
    }

    /// A connection that never reported presence carries no store handle, so
    /// its Drop reaps nothing at all — the reap is driven by the handle the
    /// report path attached, not by a session id fished out of somewhere
    /// global. (In production the two are inseparable: the handler attaches
    /// before it records, so a record bearing this session id can only exist
    /// on an attached connection.)
    #[test]
    fn drop_without_a_presence_report_touches_nothing() {
        use kaijutsu_kernel::midi_presence::{
            MidiPresenceRecord, MidiPresenceStore, SinkAttribution,
        };

        let store = Arc::new(MidiPresenceStore::new());
        let state = ConnectionState::new(test_principal(), session_context_map());
        let session_id = state.session_id;
        // A record keyed to this very session, but never attached: an
        // unattached connection is not a reaper.
        store
            .record(MidiPresenceRecord::from_sink(
                "keystep-pro",
                true,
                "alsa",
                vec![],
                100,
                SinkAttribution::new(session_id, "moltar"),
            ))
            .unwrap();
        drop(state);
        assert!(store.get("keystep-pro").is_some());
    }

    #[test]
    fn cancel_token_is_shared_with_clones() {
        // Each spawn_local task captures `cancel_token()`; verify all
        // captured handles fire from a single Drop.
        let session_contexts = session_context_map();
        let state = ConnectionState::new(test_principal(), session_contexts);
        let a = state.cancel_token();
        let b = state.cancel_token();
        drop(state);
        assert!(a.is_cancelled() && b.is_cancelled());
    }
}

/// Pins the `subscribe_blocks_filtered` dedupe contract described at
/// `SharedKernelState::subscription_registry`:
/// * a reconnect from the SAME (principal, instance) replaces the prior live
///   subscription rather than stacking a second one — the design the
///   registry exists to implement;
/// * TWO DIFFERENT instances for the same principal must coexist — this is
///   the regression this file exists to catch. `kaijutsu-mcp`
///   (`crates/kaijutsu-mcp/src/lib.rs:459`) and `kaijutsu-app`
///   (`crates/kaijutsu-app/src/connection/actor_plugin.rs:239,633`) used to
///   pass a hardcoded literal instance ("mcp-server" / "bevy-client"), so
///   every process of that client claimed the same registry slot and
///   silently evicted each other's block-event bridge (docs/issues.md, "MCP
///   shell delay" — measured 5.4s tax on MCP shell calls). A test built
///   directly against `register_subscription` — the exact map mutation the
///   live RPC path calls — reproduces that failure mode without a live SSH
///   session: construct it with the *old* buggy call shape (same literal
///   instance for two "processes") and the coexistence assertion below
///   fails, because the second insert evicts the first regardless of which
///   session registered it. That is the theft bug, caught in-process.
///
/// Also pins the observability behavior added alongside the fix: a
/// same-connection replacement is quiet (`ReplacedSameConnection`), a
/// cross-connection replacement is flagged (`ReplacedDifferentConnection`,
/// carrying the prior session and the replaced subscription's age) so an
/// operator can tell a reconnect from a theft.
#[cfg(test)]
mod subscription_registry_tests {
    use super::*;

    /// A no-op task standing in for a live FlowBus bridge task: never
    /// resolves, so `AbortHandle::is_finished()` reads false until aborted —
    /// matching a subscription bridge that's actually still running.
    fn live_abort_handle() -> tokio::task::AbortHandle {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            // Never resolves on its own (sender is dropped only when the
            // whole runtime tears down at test end) — stands in for a live
            // bridge task blocked in `tokio::select!`.
            let _ = rx.await;
        });
        handle.abort_handle()
    }

    fn principal() -> PrincipalId {
        PrincipalId::new()
    }

    #[tokio::test]
    async fn same_instance_reconnect_replaces_not_stacks() {
        let registry: parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>> =
            parking_lot::Mutex::new(HashMap::new());
        let p = principal();
        let key = (p, "stable-instance".to_string());

        let first_session = SessionId::new();
        let outcome = register_subscription(&registry, key.clone(), first_session, live_abort_handle());
        assert_eq!(outcome, SubscriptionRegistration::Fresh);
        assert_eq!(registry.lock().len(), 1, "first subscribe registers one entry");

        // Reconnect: same (principal, instance), new SSH session (a fresh
        // TCP connection always mints a new SessionId) — this is the
        // intended "replace my own prior subscription" path.
        let second_session = SessionId::new();
        let outcome = register_subscription(&registry, key.clone(), second_session, live_abort_handle());
        match outcome {
            SubscriptionRegistration::ReplacedDifferentConnection {
                prior_session_id,
                age,
            } => {
                assert_eq!(
                    prior_session_id, first_session,
                    "must report the session that owned the replaced entry",
                );
                assert!(
                    age < std::time::Duration::from_secs(5),
                    "age of a subscription registered microseconds ago must \
                     be small, got {age:?}",
                );
            }
            other => panic!(
                "a reconnect (new session, same instance) must replace, not \
                 stack — and is reported as cross-connection since \
                 reconnects always mint a fresh SessionId; got {other:?}"
            ),
        }
        assert_eq!(
            registry.lock().len(),
            1,
            "replacing the (principal, instance) slot must not leave two \
             entries behind",
        );
    }

    #[tokio::test]
    async fn different_instances_for_same_principal_coexist() {
        let registry: parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>> =
            parking_lot::Mutex::new(HashMap::new());
        let p = principal();

        // Two DIFFERENT processes of the same principal, each minting its
        // own per-process instance (the fixed shape) — NOT the pre-fix
        // hardcoded-literal shape, which would collapse both keys into one
        // and trigger the eviction this test forbids.
        let instance_a = format!("kaijutsu-mcp-{}", uuid::Uuid::new_v4());
        let instance_b = format!("kaijutsu-mcp-{}", uuid::Uuid::new_v4());
        assert_ne!(instance_a, instance_b);

        let outcome_a = register_subscription(
            &registry,
            (p, instance_a.clone()),
            SessionId::new(),
            live_abort_handle(),
        );
        let outcome_b = register_subscription(
            &registry,
            (p, instance_b.clone()),
            SessionId::new(),
            live_abort_handle(),
        );

        assert_eq!(outcome_a, SubscriptionRegistration::Fresh);
        assert_eq!(outcome_b, SubscriptionRegistration::Fresh);
        assert_eq!(
            registry.lock().len(),
            2,
            "two distinct instances for the same principal must coexist as \
             two live entries — this is the exact case the pre-fix hardcoded \
             instance literal collapsed into one, causing the silent \
             block-event-bridge theft in docs/issues.md",
        );

        let a_alive = registry
            .lock()
            .get(&(p, instance_a))
            .map(|e| !e.abort.is_finished());
        let b_alive = registry
            .lock()
            .get(&(p, instance_b))
            .map(|e| !e.abort.is_finished());
        assert_eq!(
            a_alive,
            Some(true),
            "registering instance B must not abort instance A's subscription",
        );
        assert_eq!(b_alive, Some(true));
    }

    #[tokio::test]
    async fn same_connection_replacing_its_own_subscription_is_quiet() {
        let registry: parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>> =
            parking_lot::Mutex::new(HashMap::new());
        let p = principal();
        let key = (p, "stable-instance".to_string());
        let session = SessionId::new();

        register_subscription(&registry, key.clone(), session, live_abort_handle());
        // The SAME connection re-issues subscribeBlocksFiltered (e.g. the
        // client changed its filter) — same SessionId both times.
        let outcome = register_subscription(&registry, key, session, live_abort_handle());

        assert_eq!(
            outcome,
            SubscriptionRegistration::ReplacedSameConnection,
            "a same-session replacement must be classified as ordinary \
             (not the cross-connection variant that warns)",
        );
    }

    #[tokio::test]
    async fn replacing_an_already_finished_subscription_is_not_a_live_theft() {
        let registry: parking_lot::Mutex<HashMap<(PrincipalId, String), SubscriptionEntry>> =
            parking_lot::Mutex::new(HashMap::new());
        let p = principal();
        let key = (p, "stable-instance".to_string());

        let finished = tokio::spawn(async {}).abort_handle();
        // Give the trivial task a chance to actually complete.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(finished.is_finished(), "test setup: task must have finished");

        register_subscription(&registry, key.clone(), SessionId::new(), finished);
        let outcome = register_subscription(&registry, key, SessionId::new(), live_abort_handle());

        assert_eq!(
            outcome,
            SubscriptionRegistration::ReplacedFinished,
            "a prior entry whose task already finished naturally (dropped \
             connection, or SubscriberHealth reap) is not a live steal",
        );
    }

}

#[cfg(test)]
mod turn_event_wire_mapping_tests {
    //! The kernel→wire half of the turn-outcome encoding.
    //!
    //! The client crate pins the wire→client half (`turn_events_tests` in
    //! `kaijutsu-client`), so between them the whole chain — kernel
    //! `TurnStopReason` → capnp enumerant → client `TurnCompletedStopReason` —
    //! is pinned without needing a live turn to hit every ending. That matters
    //! because the endings hardest to reach in an e2e test (a cancel winning a
    //! race against a mock provider, a real token-ceiling truncation) are the
    //! ones a collapsed mapping would hide.

    use super::*;

    /// Each kernel reason gets its own enumerant. A collapsed mapping would
    /// report a cancelled turn to every client as a clean end of turn.
    #[test]
    fn every_stop_reason_has_its_own_enumerant() {
        use crate::kaijutsu_capnp::TurnStopReason as W;
        let pairs = [
            (TurnStopReason::EndTurn, W::EndTurn),
            (TurnStopReason::Cancelled { immediate: false }, W::CancelledSoft),
            (
                TurnStopReason::Cancelled { immediate: true },
                W::CancelledImmediate,
            ),
            (TurnStopReason::MaxTokens, W::MaxTokens),
            (TurnStopReason::MaxIterations, W::MaxIterations),
        ];
        for (kernel_reason, wire) in pairs {
            assert_eq!(
                stop_reason_to_capnp(kernel_reason),
                wire,
                "{kernel_reason:?} must encode as {wire:?}"
            );
        }

        // Distinctness, stated directly: five reasons, five enumerants.
        let encoded: Vec<_> = pairs.iter().map(|(r, _)| stop_reason_to_capnp(*r)).collect();
        for (i, a) in encoded.iter().enumerate() {
            for b in encoded.iter().skip(i + 1) {
                assert_ne!(a, b, "two stop reasons collapsed onto one enumerant");
            }
        }
    }

    /// The soft/hard cancel split survives encoding — the flag that tells the
    /// beat scheduler (and an ACP frontend) whether the output is a whole
    /// phrase or a fragment.
    #[test]
    fn soft_and_hard_cancels_stay_distinct_on_the_wire() {
        assert_ne!(
            stop_reason_to_capnp(TurnStopReason::Cancelled { immediate: false }),
            stop_reason_to_capnp(TurnStopReason::Cancelled { immediate: true }),
        );
    }

    #[test]
    fn origin_maps_both_ways() {
        assert_eq!(
            turn_origin_to_capnp(TurnOrigin::Interactive),
            crate::kaijutsu_capnp::TurnOrigin::Interactive
        );
        assert_eq!(
            turn_origin_to_capnp(TurnOrigin::Autonomous),
            crate::kaijutsu_capnp::TurnOrigin::Autonomous
        );
    }
}

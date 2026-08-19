//! Background host-process execution + registry.
//!
//! `docs/issues.md` ("Background shell + process management"): `builtin.shell`
//! is synchronous-only — a model can't start `cargo build`/a test suite/a dev
//! server and get control back. This module is the kernel-owned substrate that
//! closes the gap: a durable registry of spawned host processes, keyed by an
//! opaque [`BackgroundId`], with their output streaming into a kernel block as
//! it arrives.
//!
//! # Why not kaish's own `&`/`JobManager`?
//!
//! kaish 0.13 ships real job control (`&`, `bg`/`fg`/`jobs`/`kill`, a
//! `JobManager`, `/v/jobs/{id}/{stdout,stderr,status}`). It is NOT reusable
//! here for two independent reasons:
//!
//! 1. **Lifetime mismatch.** Every `shell` tool call materializes a throwaway
//!    [`crate::runtime::embedded_kaish::EmbeddedKaish`] — a fresh
//!    `kaish_kernel::Kernel` (and thus a fresh `JobManager`) per call
//!    (`kj/context_shell.rs`). A job started with `cmd &` would be invisible
//!    to the *next* `shell` call — its `JobManager` is gone.
//! 2. **The job layer discards liveness.** kaish's `execute_background`
//!    (`kaish-kernel-0.13.0/src/kernel.rs`) awaits `runner.run(...)` to
//!    completion inside the spawned task and only then writes the aggregate
//!    `result.text_out()` into the job's `BoundedStream` — so
//!    `/v/jobs/{id}/stdout` reads empty while a command is still running.
//!
//!    NOT because the bytes are unavailable: `try_execute_external`'s capture
//!    path spawns `drain_to_stream(child.stdout, …)`
//!    (`kaish-kernel-0.13.0/src/scheduler/stream.rs:223`), which appends to a
//!    `BoundedStream` per 8 KiB chunk *as the child emits it*. The live bytes
//!    exist one layer down; `execute_background` just doesn't forward them.
//!    (An earlier revision of this comment claimed "no per-chunk write
//!    anywhere in kaish's external-command spawn path" — that was wrong for
//!    0.13 and is corrected here, because it undersold how small the upstream
//!    fix is.)
//!
//! Both are load-bearing for what a "poll a still-running build" tool needs:
//! a registry that outlives a single call, and output visible before exit.
//! So this module spawns the host process directly (mirroring kaish's own
//! external-command spawn conventions — own process group via `setpgid`,
//! `env_clear()` + explicit vars, piped stdout/stderr) rather than routing
//! through the kaish interpreter. The tradeoff: a backgrounded command is a
//! single `/bin/sh -c <command>` invocation, not a kaish script — no `kj`
//! verbs, no kaish variables/pipes-as-kaish-AST inside it. Shell syntax
//! (`|`, `&&`, `>`) still works via `sh -c`, which covers the actual use case
//! (`cargo build 2>&1 | tee build.log`).
//!
//! # Ownership and cleanup
//!
//! The registry lives on [`crate::Kernel`] (`Kernel::background_processes`),
//! not on any per-call object, so it survives across `shell` tool
//! invocations. Three independent mechanisms bound a process's lifetime:
//!
//! - **Context removal** (`kj context remove`) cancels every `Running` entry
//!   for that context (`BackgroundRegistry::kill_all_for_context`, wired from
//!   `kj/context.rs`) — a removed context's processes are killed, not
//!   orphaned into invisibility.
//! - **Kernel death of any kind** (crash, `kill -9`, a `systemctl restart`
//!   that isn't cgroup-scoped, `./contrib/kaijutsu-runner.sh` restart during
//!   dev) is covered by `PR_SET_PDEATHSIG` set in the child's `pre_exec`
//!   (Linux-only — `#[cfg(target_os = "linux")]`): the OS kills the child the
//!   instant its parent (this kernel process) dies, for ANY reason, with no
//!   dependency on a graceful-shutdown code path running at all. This was the
//!   deciding factor over relying solely on `tokio`'s `kill_on_drop` (which
//!   only fires on a clean `Drop` inside this process — never on `kill -9`)
//!   or on systemd `KillMode=control-group` (not guaranteed, and not in play
//!   at all for the `./contrib/kaijutsu-runner.sh` dev loop `CLAUDE.md`
//!   documents as the primary test loop).
//! - **Explicit `kill_background_process`** cancels one entry by id.
//!
//! Terminal entries (`Exited`/`Killed`) are retained for
//! [`DEFAULT_RETENTION`] after finishing, then reaped either opportunistically
//! on the next registry read (`list_for_context`/`get_for_context` both call
//! `reap_expired`) or by the periodic backstop `Kernel::new` starts via
//! [`BackgroundRegistry::spawn_reaper`] — the read-path reap alone isn't
//! sufficient (a context can be removed, killing its processes, with no
//! context ever polling again), so both exist. Long enough for a poller to
//! observe the final status, bounded so the registry doesn't grow forever
//! (`docs/issues.md`'s "must reap exited processes rather than leaking
//! them"). The registry itself is the durable source of truth for exit
//! status: `mark_exited`/`mark_killed` run unconditionally, even if the
//! block write failed (e.g. the context's document was deleted mid-run) — a
//! `list_background_processes` poll never silently loses the exit code. A
//! watchdog task additionally guards against the supervising task itself
//! panicking (see `spawn_background`'s trailing comment) — every fallible
//! step there already returns `Result` and is handled, but a `Running` entry
//! must never be able to get permanently stuck even if a genuine bug slips
//! through.
//!
//! **The one hole we can't close from here**: the watchdog catches a
//! *panicking* supervisor, not a *hung* one. A child wedged in uninterruptible
//! sleep (D state) doesn't die on SIGKILL, so `child.wait()` blocks, the
//! supervisor's `JoinHandle` never resolves, and the entry stays `Running`
//! indefinitely. That is a Unix property, not something a timeout here could
//! honestly fix — reaping the registry entry while the process is still alive
//! would make the registry lie in the other direction. PDEATHSIG still
//! guarantees it can't outlive the kernel.
//!
//! # Output bounding
//!
//! Each background block caps at [`DEFAULT_OUTPUT_CAP`] bytes of appended
//! text (combined stdout+stderr, interleaved by arrival — same "stdout then
//! stderr, no per-line tag" convention `shell_result_to_kernel` uses for the
//! synchronous tool). Past the cap, a single loud marker is appended and
//! further chunks are discarded — but the pipe keeps being drained (never
//! `read()`-starved) so a `yes`-style runaway process can't block on a full
//! pipe buffer and hang. This is deliberately smaller than kaish's own 10MB
//! `BoundedStream` ring: that ring is ephemeral, in-process-only memory
//! discarded when the shell materialization drops; a kernel block is
//! persistent and replicated to every connected viewer, so unbounded growth
//! there is a much sharper cost.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kaijutsu_types::{BlockId, Status};
use kaijutsu_types::{ContextId, PrincipalId};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::block_store::SharedBlockStore;

/// Combined stdout+stderr byte cap per background block. See the module docs
/// "Output bounding" section for why this is much smaller than kaish's own
/// 10MB ephemeral ring.
pub const DEFAULT_OUTPUT_CAP: usize = 256 * 1024;

/// How long a terminal (`Exited`/`Killed`) entry stays queryable after
/// finishing, before a lazy sweep reaps it. Long enough for a slow poller to
/// see the final status; bounded so the registry can't grow forever.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 60);

/// Opaque handle for a background process. UUIDv7 (time-ordered), matching
/// every other kaijutsu id — see `kaijutsu-types/src/ids.rs`'s
/// `impl_typed_id!` for the convention this hand-rolls (this type doesn't use
/// the macro because it has no wire/capnp surface — MCP JSON only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackgroundId(Uuid);

impl BackgroundId {
    fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Parse from a full hyphenated UUID or 32-char hex string. There is no
    /// short-prefix resolution (unlike `ContextId`/`BlockId`) — a background
    /// id is always handed back verbatim in the same tool call's response and
    /// echoed verbatim by the caller, never typed by a human.
    pub fn parse(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(Self)
    }
}

impl std::fmt::Display for BackgroundId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.as_simple())
    }
}

/// Lifecycle status of a background process. Terminal states
/// (`Exited`/`Killed`) never revert — `BackgroundRegistry` only ever writes
/// `Running` → one terminal state, exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundStatus {
    Running,
    Exited(i32),
    Killed,
}

impl BackgroundStatus {
    fn tag(&self) -> &'static str {
        match self {
            BackgroundStatus::Running => "running",
            BackgroundStatus::Exited(_) => "exited",
            BackgroundStatus::Killed => "killed",
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            BackgroundStatus::Exited(c) => Some(*c),
            _ => None,
        }
    }

    fn is_running(&self) -> bool {
        matches!(self, BackgroundStatus::Running)
    }
}

impl std::fmt::Display for BackgroundStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackgroundStatus::Running => write!(f, "running"),
            BackgroundStatus::Exited(c) => write!(f, "exited({c})"),
            BackgroundStatus::Killed => write!(f, "killed"),
        }
    }
}

/// A tracked background process. Internal — holds the `CancellationToken`
/// that drives `kill`, which must never leak into a serialized response (a
/// client has no use for it and it isn't `Serialize`). Callers read
/// [`BackgroundSnapshot`] instead.
struct BackgroundEntry {
    id: BackgroundId,
    pid: u32,
    command: String,
    context_id: ContextId,
    #[allow(dead_code)] // attribution only today; read by future `list --mine`-style filtering
    principal_id: PrincipalId,
    block_id: BlockId,
    started_at: SystemTime,
    finished_at: Option<SystemTime>,
    status: BackgroundStatus,
    cancel: CancellationToken,
}

fn unix_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl BackgroundEntry {
    fn snapshot(&self) -> BackgroundSnapshot {
        BackgroundSnapshot {
            id: self.id.to_string(),
            pid: self.pid,
            command: self.command.clone(),
            status: self.status.tag().to_string(),
            exit_code: self.status.exit_code(),
            block_id: self.block_id.to_key(),
            started_at_unix_ms: unix_millis(self.started_at),
            finished_at_unix_ms: self.finished_at.map(unix_millis),
        }
    }
}

/// A serializable, client-facing view of a [`BackgroundEntry`] — what
/// `list_background_processes`/`read_background_output` hand back.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackgroundSnapshot {
    pub id: String,
    pub pid: u32,
    pub command: String,
    /// `"running"` | `"exited"` | `"killed"`.
    pub status: String,
    pub exit_code: Option<i32>,
    pub block_id: String,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
}

/// Per-context ambient-state summary for the APP's dock — not the MCP tools'
/// per-process detail ([`BackgroundSnapshot`]), but the aggregate a human
/// glances at: is anything running right now, for how long, and how did the
/// last one end. Crosses the wire via `ContextHandleInfo`'s `background*`
/// fields (`kaijutsu.capnp` @23-@27) — see
/// `kaijutsu-server::rpc::resolve_background_wire_fields` for the sentinel
/// encoding and `kaijutsu-client::rpc::parse_context_info` for the decode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundSummary {
    /// How many of this context's background processes are `Running` right
    /// now. `0` when none are — the honest "nothing running" state, not a
    /// sentinel.
    pub running_count: u32,
    /// Unix-ms `started_at` of the OLDEST still-`Running` entry — the
    /// elapsed-time anchor for "how long has this been going". `None` when
    /// nothing is running.
    pub oldest_running_started_at_unix_ms: Option<u64>,
    /// The most-recently-finished (`Exited`/`Killed`) entry, by
    /// `finished_at`. `None` when nothing has finished yet — including a
    /// process that finished more than [`DEFAULT_RETENTION`] ago and has
    /// since been reaped: the registry's own forgetting is this field's
    /// natural TTL, no separate expiry tracked here.
    pub last_finished: Option<BackgroundFinishedSummary>,
}

/// The outcome half of [`BackgroundSummary`] — one finished background
/// process, reduced to what the dock needs to say "how did it end".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundFinishedSummary {
    /// `"exited"` or `"killed"` — the same tag [`BackgroundStatus::tag`]
    /// produces, never `"running"` (a `Running` entry is never selected as
    /// `last_finished`).
    pub status: &'static str,
    /// `Some(code)` for `"exited"`, `None` for `"killed"`.
    pub exit_code: Option<i32>,
    pub finished_at_unix_ms: u64,
}

/// Kernel-owned registry of background processes. See the module docs for the
/// ownership/cleanup/output-bounding contract.
pub struct BackgroundRegistry {
    entries: dashmap::DashMap<BackgroundId, BackgroundEntry>,
    retention: Duration,
}

impl BackgroundRegistry {
    pub fn new() -> Self {
        Self::with_retention(DEFAULT_RETENTION)
    }

    /// Test/tuning seam — production always uses [`DEFAULT_RETENTION`].
    pub fn with_retention(retention: Duration) -> Self {
        Self {
            entries: dashmap::DashMap::new(),
            retention,
        }
    }

    /// Drop terminal entries older than `retention`. Called opportunistically
    /// on every read path (`list_for_context`/`get_for_context`) AND
    /// periodically by [`Self::spawn_reaper`] — reads alone aren't a
    /// sufficient trigger: a workload that removes contexts (cancelling
    /// their background processes) without any context ever calling
    /// `list_background_processes`/`read_background_output` afterward would
    /// otherwise leak one terminal entry per process forever, since nothing
    /// else revisits the registry.
    fn reap_expired(&self) {
        let now = SystemTime::now();
        let retention = self.retention;
        self.entries.retain(|_, e| match e.finished_at {
            Some(t) => now.duration_since(t).map(|age| age < retention).unwrap_or(true),
            None => true,
        });
    }

    fn insert(&self, entry: BackgroundEntry) {
        self.entries.insert(entry.id, entry);
    }

    /// Sweep cadence for [`Self::spawn_reaper`]. Fixed rather than derived
    /// from `retention`: `reap_expired` is a cheap `dashmap::retain` over
    /// what's normally a tiny map, so there's no real cost to sweeping this
    /// often regardless of how long terminal entries are kept, and a flat
    /// constant is simpler to reason about than a per-registry interval
    /// nobody needs to tune.
    const REAP_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

    /// Start a lightweight periodic sweep so terminal entries are reaped
    /// even when nothing ever reads the registry again — the backstop for
    /// the leak the read-path-only reap can't close (see
    /// [`Self::reap_expired`]'s doc comment). Holds only a `Weak` reference
    /// so the task exits on its own once the registry (and the `Kernel` that
    /// owns it) drops — no explicit shutdown wiring needed. Call once, right
    /// after wrapping a fresh registry in `Arc` (`Kernel::new`); calling it
    /// twice on the same registry just runs two redundant sweep loops (no
    /// correctness issue, just wasted wakeups), so it isn't idempotent-safe
    /// to call per-use.
    pub fn spawn_reaper(self: &Arc<Self>) {
        self.spawn_reaper_every(Self::REAP_SWEEP_INTERVAL);
    }

    /// Test seam for [`Self::spawn_reaper`] — production always uses the
    /// fixed [`Self::REAP_SWEEP_INTERVAL`]. `reap_expired`'s aging is real
    /// `std::time::SystemTime`, not a tokio `Instant`, so `tokio::time`'s
    /// paused/virtual clock can't fast-forward it — a test that wants to
    /// observe the sweep uses a short REAL interval + short REAL retention
    /// instead.
    fn spawn_reaper_every(self: &Arc<Self>, interval: Duration) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(registry) = weak.upgrade() else {
                    break; // owning Kernel (and this registry) is gone
                };
                registry.reap_expired();
            }
        });
    }

    /// Every process for `context_id`, terminal entries included (until
    /// reaped). Never reports another context's processes — see the module
    /// docs; each background process belongs to exactly the context that
    /// started it.
    pub fn list_for_context(&self, context_id: ContextId) -> Vec<BackgroundSnapshot> {
        self.reap_expired();
        self.entries
            .iter()
            .filter(|e| e.context_id == context_id)
            .map(|e| e.snapshot())
            .collect()
    }

    /// A single process's snapshot, scoped to `context_id` — `None` both when
    /// the id doesn't exist and when it exists under a different context (no
    /// existence leak across contexts).
    pub fn get_for_context(&self, id: BackgroundId, context_id: ContextId) -> Option<BackgroundSnapshot> {
        self.reap_expired();
        self.entries
            .get(&id)
            .filter(|e| e.context_id == context_id)
            .map(|e| e.snapshot())
    }

    /// Every context's [`BackgroundSummary`] in one pass — the dock's "one
    /// query for the whole kernel" counterpart to [`Self::list_for_context`],
    /// mirroring the `usage_map`/`track_of` batching convention
    /// `kaijutsu-server::rpc::list_contexts` already uses for
    /// per-context-usage and per-context-track lookups. A context with no
    /// background processes at all (past or present) is simply absent from
    /// the map — the caller treats a missing entry exactly like
    /// [`BackgroundSummary::default`] (nothing running, nothing finished).
    pub fn summary_by_context(&self) -> HashMap<ContextId, BackgroundSummary> {
        self.reap_expired();

        #[derive(Default)]
        struct Accum {
            running_count: u32,
            oldest_running: Option<SystemTime>,
            last_finished: Option<(SystemTime, BackgroundFinishedSummary)>,
        }

        let mut acc: HashMap<ContextId, Accum> = HashMap::new();
        for e in self.entries.iter() {
            let a = acc.entry(e.context_id).or_default();
            if e.status.is_running() {
                a.running_count += 1;
                if a.oldest_running.map(|t| e.started_at < t).unwrap_or(true) {
                    a.oldest_running = Some(e.started_at);
                }
                continue;
            }
            // Terminal entries always carry `finished_at` (set atomically
            // with the status in `mark_exited`/`mark_killed`) — the `if let`
            // is defensive, not an expected-empty case.
            if let Some(finished_at) = e.finished_at {
                let is_newer = a
                    .last_finished
                    .as_ref()
                    .map(|(prev, _)| finished_at > *prev)
                    .unwrap_or(true);
                if is_newer {
                    a.last_finished = Some((
                        finished_at,
                        BackgroundFinishedSummary {
                            status: e.status.tag(),
                            exit_code: e.status.exit_code(),
                            finished_at_unix_ms: unix_millis(finished_at),
                        },
                    ));
                }
            }
        }

        acc.into_iter()
            .map(|(ctx_id, a)| {
                (
                    ctx_id,
                    BackgroundSummary {
                        running_count: a.running_count,
                        oldest_running_started_at_unix_ms: a.oldest_running.map(unix_millis),
                        last_finished: a.last_finished.map(|(_, s)| s),
                    },
                )
            })
            .collect()
    }

    /// Signal a running process to stop. Returns `true` iff a running entry
    /// matching `(id, context_id)` was found and cancelled; `false` for
    /// unknown id, wrong context, or an already-terminal process (killing a
    /// dead process is a no-op, not an error the caller needs to react to
    /// beyond re-checking status).
    pub fn cancel(&self, id: BackgroundId, context_id: ContextId) -> bool {
        match self.entries.get(&id) {
            Some(e) if e.context_id == context_id && e.status.is_running() => {
                e.cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Cancel every `Running` entry owned by `context_id`. Fire-and-forget by
    /// design: the supervising tasks observe the cancellation and tear down
    /// the OS process independently — a caller (`kj context remove`) doesn't
    /// block on their exit. Returns how many were signalled.
    pub fn kill_all_for_context(&self, context_id: ContextId) -> usize {
        let mut n = 0;
        for e in self.entries.iter() {
            if e.context_id == context_id && e.status.is_running() {
                e.cancel.cancel();
                n += 1;
            }
        }
        n
    }

    fn mark_exited(&self, id: BackgroundId, code: i32) {
        if let Some(mut e) = self.entries.get_mut(&id) {
            e.status = BackgroundStatus::Exited(code);
            e.finished_at = Some(SystemTime::now());
        }
    }

    fn mark_killed(&self, id: BackgroundId) {
        if let Some(mut e) = self.entries.get_mut(&id) {
            e.status = BackgroundStatus::Killed;
            e.finished_at = Some(SystemTime::now());
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for BackgroundRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything needed to start a background process and stream its output
/// into a pre-created kernel block.
pub struct SpawnBackgroundParams {
    pub command: String,
    pub cwd: PathBuf,
    /// The child's *entire* environment (hermetic — the child never inherits
    /// this kernel process's OS env; caller builds the full set explicitly,
    /// mirroring `EmbeddedKaish`'s `ExternalExec::Allow` policy).
    pub env: Vec<(String, String)>,
    pub context_id: ContextId,
    pub principal_id: PrincipalId,
    /// A block already inserted with `Status::Running` — this call appends
    /// to it and eventually transitions it to `Done`/`Error`.
    pub block_id: BlockId,
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnBackgroundError {
    #[error("failed to spawn background process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("background process started with no observable pid")]
    NoPid,
}

/// Start `params.command` as `/bin/sh -c <command>` in its own process group,
/// register it in `registry`, and spawn a supervising task that streams
/// stdout+stderr into `params.block_id` (capped, see module docs) and
/// finalizes the block + registry entry on exit or cancellation. Returns as
/// soon as the process is spawned and registered — never waits for it to run.
pub fn spawn_background(
    registry: &Arc<BackgroundRegistry>,
    blocks: &SharedBlockStore,
    params: SpawnBackgroundParams,
) -> Result<BackgroundId, SpawnBackgroundError> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&params.command);
    cmd.current_dir(&params.cwd);
    cmd.env_clear();
    for (k, v) in &params.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Backstop only: kills the child if THIS process cleanly drops the
    // `Child` (e.g. a panic unwind). The real orphan guard is PDEATHSIG
    // below, which also covers `kill -9` and process crashes where no Drop
    // ever runs — see the module docs "Ownership and cleanup".
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        // SAFETY: `setpgid`/`set_parent_process_death_signal` are
        // async-signal-safe per POSIX; safe to call between fork and exec.
        // Own process group so `kill_process_group` reaches the whole tree
        // (matches kaish's own external-command spawn convention —
        // `kaish-kernel-0.13.0/src/dispatch.rs`).
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setpgid(None, None)
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                #[cfg(target_os = "linux")]
                rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::Kill))
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(SpawnBackgroundError::Spawn)?;
    let pid = child.id().ok_or(SpawnBackgroundError::NoPid)?;

    let id = BackgroundId::new();
    let cancel = CancellationToken::new();
    registry.insert(BackgroundEntry {
        id,
        pid,
        command: params.command.clone(),
        context_id: params.context_id,
        principal_id: params.principal_id,
        block_id: params.block_id,
        started_at: SystemTime::now(),
        finished_at: None,
        status: BackgroundStatus::Running,
        cancel: cancel.clone(),
    });

    let stdout = child.stdout.take().expect("stdout piped at spawn");
    let stderr = child.stderr.take().expect("stderr piped at spawn");

    let task_blocks = blocks.clone();
    let task_registry = registry.clone();
    // A second clone for the watchdog below — see its comment for why a
    // panic inside the supervising task needs its own path to a terminal
    // registry/block state, independent of the task it's watching.
    let watchdog_blocks = blocks.clone();
    let watchdog_registry = registry.clone();
    let ctx_id = params.context_id;
    let block_id = params.block_id;
    let principal_id = params.principal_id;

    let supervisor = tokio::spawn(async move {
        let blocks = task_blocks;
        let registry = task_registry;
        let written = Arc::new(AtomicUsize::new(0));
        let capped = Arc::new(AtomicBool::new(false));
        // One projection state for the block, shared by both pipes — see
        // `AnsiDrain` for why it can't be per-task.
        let ansi = Arc::new(parking_lot::Mutex::new(AnsiDrain::new()));

        let out_task = tokio::spawn(drain_to_block(
            stdout,
            blocks.clone(),
            ctx_id,
            block_id,
            principal_id,
            written.clone(),
            capped.clone(),
            ansi.clone(),
        ));
        let err_task = tokio::spawn(drain_to_block(
            stderr,
            blocks.clone(),
            ctx_id,
            block_id,
            principal_id,
            written,
            capped,
            ansi.clone(),
        ));

        enum WaitOutcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            Cancelled,
        }

        // `biased` polls the cancel branch first, so a simultaneous
        // cancel-and-exit deterministically reports Cancelled.
        //
        // KNOWN, ACCEPTED RACE: if the process exits on its own in the window
        // between this select resolving on `cancel` and the
        // `kill_process_group` below, the entry is recorded `Killed` (and the
        // block `Status::Error`) even though the command actually finished —
        // possibly with code 0, which `wait()` will faithfully report just
        // below. We keep this rather than reconciling status against the
        // observed exit code, because "you asked to kill it" is the honest
        // account of what the caller did, and silently reporting a clean
        // `Exited(0)` would hide that a kill was issued at all. Anyone
        // reading a snapshot with `status: "killed"` next to `exit_code: 0`
        // is looking at this race.
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => WaitOutcome::Cancelled,
            status = child.wait() => WaitOutcome::Exited(status),
        };

        let was_cancelled = matches!(outcome, WaitOutcome::Cancelled);
        if was_cancelled {
            #[cfg(unix)]
            if let Some(pg) = rustix::process::Pid::from_raw(pid as i32) {
                // ESRCH (already dead) is expected and fine — `let _` drops it.
                let _ = rustix::process::kill_process_group(pg, rustix::process::Signal::Kill);
            }
        }

        // Always reap — even after a cancel-triggered kill — so we (a) never
        // leave a zombie and (b) record the real terminal wait() outcome.
        let final_status = match outcome {
            WaitOutcome::Exited(s) => s,
            WaitOutcome::Cancelled => child.wait().await,
        };

        // Let the drains finish (they hit EOF once the child's pipes close).
        let _ = out_task.await;
        let _ = err_task.await;

        // Buffer-until-done provenance (docs/ansi-and-beyond.md): the spans
        // and the original bytes land once, after the final append, so oplog
        // replay — which re-applies appends and would clear spans set early —
        // reproduces exactly this state.
        finalize_ansi(&blocks, ctx_id, block_id, ansi);

        match final_status {
            Ok(status) => {
                let code = exit_code_from_status(&status);
                if let Err(e) = blocks.set_exit_code(ctx_id, &block_id, Some(code)) {
                    tracing::warn!(background_id = %id, error = %e, "background: set_exit_code failed (document likely removed)");
                }
                let terminal_block_status = if was_cancelled || code != 0 {
                    Status::Error
                } else {
                    Status::Done
                };
                if let Err(e) = blocks.set_status(ctx_id, &block_id, terminal_block_status) {
                    tracing::warn!(background_id = %id, error = %e, "background: set_status failed (document likely removed)");
                }
                if was_cancelled {
                    registry.mark_killed(id);
                } else {
                    registry.mark_exited(id, code);
                }
            }
            Err(e) => {
                // wait() itself failed (not the command exiting nonzero) — a
                // genuine "we lost track of the process" case. Still record a
                // terminal status rather than leaving it Running forever.
                tracing::warn!(background_id = %id, error = %e, "background: wait() failed");
                let _ = blocks.set_status(ctx_id, &block_id, Status::Error);
                registry.mark_killed(id);
            }
        }
    });

    // Watchdog: every fallible step inside the supervisor above returns a
    // `Result` and is handled (warn + continue), so it isn't expected to
    // panic — but "isn't expected to" is not the same guarantee as "cannot
    // reach the CLAUDE.md line": a genuine bug there must never leave a
    // `Running` registry entry (and its block) stuck forever with an
    // unsupervised child. If `supervisor` panics, `JoinHandle::await`
    // returns `Err` here and we finalize the entry ourselves. Note this
    // path does NOT itself re-attempt `kill_process_group` — a panicking
    // task could have panicked before OR after already signalling the
    // child, and PDEATHSIG (Linux) plus `kill_on_drop` remain the actual
    // orphan guards regardless; this watchdog's job is only to stop the
    // registry/block from lying about the process still being `Running`.
    tokio::spawn(async move {
        if let Err(join_err) = supervisor.await {
            tracing::error!(
                background_id = %id,
                error = %join_err,
                "background: supervising task panicked — marking the entry terminal so it isn't stuck Running forever",
            );
            watchdog_registry.mark_killed(id);
            let _ = watchdog_blocks.set_status(ctx_id, &block_id, Status::Error);
        }
    });

    Ok(id)
}

/// Map a `sh -c` child's exit status to a shell-style code: the normal exit
/// code, or `128 + signal` for a signal death (matches kaish's own
/// `exit_code_from_status` convention — `kaish-kernel-0.13.0/src/kernel.rs`).
fn exit_code_from_status(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}

/// The `ansi-strip` ingest state for ONE background block — shared by both
/// drain tasks, because both of them append into that same block.
///
/// This is the streaming half of `docs/ansi-and-beyond.md`, and the only place
/// in kaijutsu where escape sequences can straddle a read boundary. Three
/// things live here, all of which have to agree:
///
/// - **One [`StripParser`], not one per stream.** The two drains interleave
///   into a single block, so the block's content is the concatenation of both
///   pipes in arrival order. Span offsets address *that* concatenation, and the
///   CI invariant `strip(original) == (content, spans)` only holds if one
///   parser saw one byte order. Two parsers would each produce offsets into
///   their own stream and neither would fit the block. (It also means SGR state
///   carries across the streams — which is what a terminal showing both on one
///   tty does anyway.)
/// - **A cursor into `parser.text()`.** The parser's text grows monotonically;
///   each drain iteration appends only the slice past the cursor.
/// - **`raw` and `raw_committed`.** `raw` is everything fed; `raw_committed` is
///   the prefix whose projection actually reached the block. On completion the
///   tail is dropped, so the stored original is exactly "what was ingested
///   before the cap tripped" and `strip(original)` reproduces the block content
///   verbatim.
struct AnsiDrain {
    parser: kaijutsu_ansi::StripParser,
    /// Bytes of `parser.text()` already appended to the block.
    cursor: usize,
    /// Every raw byte fed to the parser.
    raw: Vec<u8>,
    /// Length of the `raw` prefix corresponding to `cursor`.
    raw_committed: usize,
    /// Whether any escape byte was ever seen. False ⇒ the whole fast path:
    /// no tag, no row, no spans, and `raw` is dropped unread.
    saw_escape: bool,
}

impl AnsiDrain {
    fn new() -> Self {
        Self {
            parser: kaijutsu_ansi::StripParser::new(),
            cursor: 0,
            raw: Vec::new(),
            raw_committed: 0,
            saw_escape: false,
        }
    }

    /// Feed one chunk; return the newly-projected text to append (may be
    /// empty when the chunk was pure escape sequence, or ended mid-codepoint).
    fn feed(&mut self, chunk: &[u8]) -> String {
        if chunk.contains(&0x1b) {
            self.saw_escape = true;
        }
        self.raw.extend_from_slice(chunk);
        self.parser.feed(chunk);
        self.parser.text()[self.cursor..].to_string()
    }

    /// Acknowledge that the text `feed` just returned reached the block.
    /// Called only on a successful append, so a chunk dropped by the cap
    /// leaves both cursors where they were.
    fn commit(&mut self) {
        self.cursor = self.parser.text().len();
        self.raw_committed = self.raw.len();
    }

    /// The finished projection: the original bytes to store, and the spans
    /// that describe the text actually in the block. `None` when no escape
    /// byte was ever seen — the block is untouched by this transform.
    fn finish(mut self) -> Option<(Vec<u8>, Vec<kaijutsu_ansi::StyleSpan>)> {
        if !self.saw_escape {
            return None;
        }
        let committed = self.cursor;
        self.raw.truncate(self.raw_committed);
        let (_, spans) = self.parser.finish();
        let spans = spans
            .into_iter()
            .filter(|s| (s.start as usize) < committed)
            .map(|mut s| {
                s.end = s.end.min(committed as u32);
                s
            })
            .collect();
        Some((self.raw, spans))
    }
}

/// Drain one pipe (stdout or stderr) into `block_id`, capping combined bytes
/// (shared via `written`/`capped` with the sibling stream) at
/// [`DEFAULT_OUTPUT_CAP`]. Keeps reading past the cap and discarding — never
/// stops draining — so a runaway producer can't deadlock on a full pipe
/// buffer once the cap trips.
///
/// Raw bytes go through the shared [`AnsiDrain`] before anything is appended,
/// so what lands in the block is the clean projection. Two consequences worth
/// stating:
///
/// - **The cap now counts clean bytes.** `DEFAULT_OUTPUT_CAP` bounds what the
///   block actually holds, which is the cost we care about (persistent,
///   replicated to every viewer). Escape sequences no longer eat a colorful
///   command's output budget.
/// - **Provenance is pre-cap in the sense that matters.** The stored original
///   is the byte prefix whose projection reached the block; the trailing
///   "output capped" marker is kaijutsu's own text and is deliberately NOT in
///   the original. So for a capped block, `content == strip(original) +
///   marker` rather than `== strip(original)`. Spans stay exact either way.
#[allow(clippy::too_many_arguments)]
async fn drain_to_block(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    blocks: SharedBlockStore,
    context_id: ContextId,
    block_id: BlockId,
    principal_id: PrincipalId,
    written: Arc<AtomicUsize>,
    capped: Arc<AtomicBool>,
    ansi: Arc<parking_lot::Mutex<AnsiDrain>>,
) {
    const CHUNK: usize = 8192;
    let mut buf = [0u8; CHUNK];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        // SeqCst, not Relaxed: every other access to `capped`/`written` in
        // this function is SeqCst, and the two drain tasks (stdout + stderr)
        // race on both. A Relaxed load could read a stale `false` after the
        // sibling task's SeqCst store on a weakly-ordered target (ARM), which
        // costs at most one extra written chunk — harmless, but not worth the
        // inconsistency in a function whose whole job is a shared bound.
        if capped.load(Ordering::SeqCst) {
            continue; // keep draining, discard — never re-enable
        }

        // The lock spans feed-and-append so the byte order the parser saw is
        // the byte order the block got. No `.await` inside it.
        let mut ansi = ansi.lock();
        let text = ansi.feed(&buf[..n]);
        if text.is_empty() {
            continue; // chunk was pure escape sequence, or a split codepoint
        }

        let prev = written.fetch_add(text.len(), Ordering::SeqCst);
        if prev >= DEFAULT_OUTPUT_CAP {
            if !capped.swap(true, Ordering::SeqCst) {
                // Loud on failure, exactly like the normal-output append
                // below: if this write is lost the block ends up truncated
                // with NO marker explaining why, which is precisely the
                // silent-fallback failure mode CLAUDE.md rejects. The reader
                // would see output simply stop with no evidence of a cap.
                if let Err(e) = blocks.append_text_as(
                    context_id,
                    &block_id,
                    "\n[kaijutsu: background output capped at \
                     DEFAULT_OUTPUT_CAP bytes — further output is discarded; \
                     the process is still running and its exit status will \
                     still be recorded]\n",
                    Some(principal_id),
                ) {
                    tracing::warn!(
                        error = %e,
                        "background: failed to append the output-cap marker (document likely removed); \
                         the block is truncated with no in-band explanation"
                    );
                }
            }
            continue;
        }

        if let Err(e) = blocks.append_text_as(context_id, &block_id, &text, Some(principal_id)) {
            tracing::warn!(error = %e, "background: append_text_as failed (document likely removed); dropping further output for this process");
            capped.store(true, Ordering::SeqCst);
        } else {
            ansi.commit();
        }
    }
}

/// Land the finished ANSI projection on a background block, once both drains
/// have ended.
///
/// Ordering: this runs strictly after the last append. `append_text` keeps
/// spans (offsets into earlier text stay valid), so the discipline is belt and
/// braces here — but `edit_text` *clears* them, and "spans go last" is the one
/// rule that survives someone later swapping an append for an edit.
fn finalize_ansi(
    blocks: &SharedBlockStore,
    context_id: ContextId,
    block_id: BlockId,
    ansi: Arc<parking_lot::Mutex<AnsiDrain>>,
) {
    // Both drain tasks have joined, so this is the only remaining holder.
    let state = std::mem::replace(&mut *ansi.lock(), AnsiDrain::new());
    let Some((original, spans)) = state.finish() else {
        return; // no escape bytes ever seen — the free path
    };
    crate::ansi_ingest::record(blocks, context_id, &block_id, spans, &original);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use kaijutsu_types::{BlockKind, ContentType, Role};
    use kaijutsu_types::DocKind;
    use std::time::Duration as StdDuration;

    /// Poll `f` until it returns `Some`, or panic after `timeout`. Background
    /// process completion is inherently async (a real child process, a real
    /// tokio::spawn) — tests must wait for it, not assume instant completion.
    async fn wait_for<T>(timeout: StdDuration, mut f: impl FnMut() -> Option<T>) -> T {
        let start = std::time::Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            if start.elapsed() > timeout {
                panic!("wait_for timed out after {timeout:?}");
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    }

    fn test_env() -> Vec<(String, String)> {
        vec![
            ("PATH".to_string(), std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())),
            ("HOME".to_string(), std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())),
        ]
    }

    fn setup() -> (SharedBlockStore, ContextId, PrincipalId) {
        let principal = PrincipalId::new();
        let blocks = shared_block_store(principal);
        let ctx_id = ContextId::new();
        blocks.create_document(ctx_id, DocKind::Conversation, None).expect("create doc");
        (blocks, ctx_id, principal)
    }

    fn running_block(blocks: &SharedBlockStore, ctx_id: ContextId, principal: PrincipalId) -> BlockId {
        blocks
            .insert_block_as(
                ctx_id,
                None,
                None,
                Role::Tool,
                BlockKind::ToolResult,
                String::new(),
                Status::Running,
                ContentType::Plain,
                Some(principal),
            )
            .expect("insert running block")
    }

    #[tokio::test]
    async fn spawn_background_streams_output_and_records_exit_code() {
        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "echo hello-background".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "exited").then_some(s)
        })
        .await;

        assert_eq!(snap.exit_code, Some(0), "echo should exit 0");
        assert!(snap.pid > 0, "pid should be recorded");

        let content = blocks
            .get_block_snapshot(ctx_id, &block_id)
            .unwrap()
            .unwrap()
            .content;
        assert!(
            content.contains("hello-background"),
            "block content should carry the streamed stdout, got: {content:?}"
        );

        let final_snap = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(final_snap.status, Status::Done, "successful exit must transition the block to Done");
        assert_eq!(final_snap.exit_code, Some(0));
    }

    #[tokio::test]
    async fn spawn_background_nonzero_exit_marks_block_error_and_records_code() {
        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "exit 7".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "exited").then_some(s)
        })
        .await;

        assert_eq!(snap.exit_code, Some(7), "exit status must never be lost");
        let final_snap = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(final_snap.status, Status::Error);
        assert_eq!(final_snap.exit_code, Some(7));
    }

    #[tokio::test]
    async fn output_cap_truncates_with_a_loud_marker_and_still_records_exit() {
        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        // A tiny cap so the test doesn't need to produce hundreds of KB.
        let registry = Arc::new(BackgroundRegistry::new());

        // `yes` never stops on its own; bound it with `head -c` well past the
        // (tiny, module-level) DEFAULT_OUTPUT_CAP isn't practical to override
        // per-call, so exercise the real cap with a command that emits more
        // than DEFAULT_OUTPUT_CAP but still terminates on its own — proving
        // BOTH the cap marker AND that the process (and its exit code) are
        // still tracked to completion even though most of its output was
        // discarded.
        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: format!("yes x | head -c {}", DEFAULT_OUTPUT_CAP * 3),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        let snap = wait_for(StdDuration::from_secs(10), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            s.status.eq("exited").then_some(s)
        })
        .await;
        assert_eq!(snap.exit_code, Some(0), "exit status must still be recorded past the cap");

        let content = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap().content;
        assert!(
            content.contains("capped"),
            "content should carry the loud truncation marker, got len={}",
            content.len()
        );
        assert!(
            content.len() < DEFAULT_OUTPUT_CAP * 2,
            "content must not grow unbounded past the cap, got {} bytes",
            content.len()
        );
    }

    #[tokio::test]
    async fn kill_background_process_stops_it_and_marks_killed() {
        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 30".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        // Give it a moment to actually be running before we kill it.
        wait_for(StdDuration::from_secs(2), || {
            registry.get_for_context(id, ctx_id).filter(|s| s.status == "running")
        })
        .await;

        assert!(registry.cancel(id, ctx_id), "cancel should find the running entry");

        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "killed").then_some(s)
        })
        .await;
        assert_eq!(snap.status, "killed");

        let final_snap = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(final_snap.status, Status::Error, "a killed process's block must not read as a clean Done");
    }

    #[tokio::test]
    async fn cancel_is_a_noop_for_unknown_or_cross_context_or_terminal_entries() {
        let (blocks, ctx_id, principal) = setup();
        let other_ctx = ContextId::new();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "true".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        assert!(!registry.cancel(BackgroundId::new(), ctx_id), "unknown id");
        assert!(!registry.cancel(id, other_ctx), "wrong context must not be able to kill it");

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id, ctx_id).filter(|s| s.status == "exited")
        })
        .await;
        assert!(!registry.cancel(id, ctx_id), "an already-terminal process is a no-op, not an error");
    }

    #[tokio::test]
    async fn kill_all_for_context_only_touches_that_context() {
        let (blocks, ctx_a, principal) = setup();
        let ctx_b = ContextId::new();
        blocks.create_document(ctx_b, DocKind::Conversation, None).unwrap();
        let block_a = running_block(&blocks, ctx_a, principal);
        let block_b = running_block(&blocks, ctx_b, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id_a = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 30".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_a,
                principal_id: principal,
                block_id: block_a,
            },
        )
        .unwrap();
        let id_b = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 30".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_b,
                principal_id: principal,
                block_id: block_b,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(2), || {
            registry.get_for_context(id_a, ctx_a).filter(|s| s.status == "running")
        })
        .await;

        let n = registry.kill_all_for_context(ctx_a);
        assert_eq!(n, 1, "only ctx_a's running process should be signalled");

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id_a, ctx_a).filter(|s| s.status == "killed")
        })
        .await;

        // ctx_b's process must still be running — kill_all_for_context(ctx_a)
        // must not reach across context boundaries.
        let b_status = registry.get_for_context(id_b, ctx_b).unwrap().status;
        assert_eq!(b_status, "running", "ctx_b's process must be unaffected");

        // Clean up so the test doesn't leak a live `sleep 30`.
        registry.cancel(id_b, ctx_b);
    }

    #[tokio::test]
    async fn list_for_context_filters_and_reaps_expired_terminal_entries() {
        let (blocks, ctx_a, principal) = setup();
        let ctx_b = ContextId::new();
        blocks.create_document(ctx_b, DocKind::Conversation, None).unwrap();
        let block_a = running_block(&blocks, ctx_a, principal);
        let block_b = running_block(&blocks, ctx_b, principal);
        // A short but non-zero retention: long enough that a poll can observe
        // the terminal state before it's swept (a literal zero retention is a
        // degenerate case — an entry would become unobservable on the exact
        // same reap pass that first notices it's terminal, since every read
        // path reaps before it filters).
        let retention = StdDuration::from_millis(150);
        let registry = Arc::new(BackgroundRegistry::with_retention(retention));

        let id_a = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "true".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_a,
                principal_id: principal,
                block_id: block_a,
            },
        )
        .unwrap();
        spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "true".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_b,
                principal_id: principal,
                block_id: block_b,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id_a, ctx_a).filter(|s| s.status == "exited")
        })
        .await;

        let a_list = registry.list_for_context(ctx_a);
        assert_eq!(a_list.len(), 1, "must only see ctx_a's own process");

        // Past the retention window, a fresh access must reap the expired
        // terminal entries. `list_for_context` reaps as a side effect
        // (that's the point under test) — drive the sweep by calling it
        // rather than reading `len()` directly. `+ margin` so a slow CI host
        // doesn't flake the boundary.
        tokio::time::sleep(retention + StdDuration::from_millis(200)).await;
        wait_for(StdDuration::from_secs(5), || {
            let _ = registry.list_for_context(ctx_a);
            (registry.len() == 0).then_some(())
        })
        .await;
    }

    /// `spawn_reaper`'s periodic sweep must reap an expired terminal entry
    /// on its own — with NO `list`/`get` call ever made — closing the leak
    /// the read-path-only reap can't (a context removed via
    /// `kill_all_for_context`, with nothing ever polling afterward). Uses
    /// real (short) durations rather than tokio's paused/virtual clock:
    /// `reap_expired`'s aging is `std::time::SystemTime`, which a paused
    /// tokio `Instant` clock cannot fast-forward.
    #[tokio::test]
    async fn spawn_reaper_prunes_terminal_entries_with_no_read_ever() {
        let retention = StdDuration::from_millis(50);
        let registry = Arc::new(BackgroundRegistry::with_retention(retention));
        registry.spawn_reaper_every(StdDuration::from_millis(20));

        let id = BackgroundId::new();
        let ctx_id = ContextId::new();
        let principal = PrincipalId::new();
        registry.insert(BackgroundEntry {
            id,
            pid: 1,
            command: "true".to_string(),
            context_id: ctx_id,
            principal_id: principal,
            block_id: BlockId::new(ctx_id, principal, 1),
            started_at: SystemTime::now(),
            finished_at: None,
            status: BackgroundStatus::Running,
            cancel: CancellationToken::new(),
        });
        registry.mark_exited(id, 0);
        assert_eq!(registry.len(), 1, "entry present immediately after marking terminal");

        // No list_for_context/get_for_context call anywhere below — only the
        // periodic sweep can remove it.
        wait_for(StdDuration::from_secs(5), || (registry.len() == 0).then_some(())).await;
    }

    /// The dock's ambient-state ask: several processes running at once must
    /// report the right count AND the OLDEST one's start time (the elapsed
    /// anchor), not just "something is running".
    #[tokio::test]
    async fn summary_by_context_reports_running_count_and_oldest_started_at() {
        let (blocks, ctx_id, principal) = setup();
        let registry = Arc::new(BackgroundRegistry::new());

        let block_a = running_block(&blocks, ctx_id, principal);
        let id_a = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 5".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id: block_a,
            },
        )
        .unwrap();

        // Force a measurable (well above SystemTime's ms resolution) gap so
        // "oldest" has an unambiguous answer.
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        let block_b = running_block(&blocks, ctx_id, principal);
        let id_b = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 5".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id: block_b,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(2), || {
            registry.get_for_context(id_b, ctx_id).filter(|s| s.status == "running")
        })
        .await;

        let a_started_at = registry.get_for_context(id_a, ctx_id).unwrap().started_at_unix_ms;

        let summary = registry
            .summary_by_context()
            .get(&ctx_id)
            .cloned()
            .expect("ctx_id must have a summary while two processes are running");
        assert_eq!(summary.running_count, 2, "both processes are still Running");
        assert_eq!(
            summary.oldest_running_started_at_unix_ms,
            Some(a_started_at),
            "the OLDEST running process (spawned first) anchors the elapsed-time reading"
        );
        assert!(summary.last_finished.is_none(), "nothing has finished yet");

        // Clean up so the test doesn't leak two live `sleep 5`s.
        registry.cancel(id_a, ctx_id);
        registry.cancel(id_b, ctx_id);
    }

    /// A failed (nonzero exit) background process must surface as
    /// `last_finished` with the real exit code — "how did it end" for the
    /// dock's failure case.
    #[tokio::test]
    async fn summary_by_context_reports_nonzero_exit_as_last_finished() {
        let (blocks, ctx_id, principal) = setup();
        let registry = Arc::new(BackgroundRegistry::new());
        let block_id = running_block(&blocks, ctx_id, principal);

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "exit 9".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id, ctx_id).filter(|s| s.status == "exited")
        })
        .await;

        let summary = registry.summary_by_context().remove(&ctx_id).expect("ctx_id must have a summary");
        assert_eq!(summary.running_count, 0);
        let finished = summary.last_finished.expect("the exited process must be reported");
        assert_eq!(finished.status, "exited");
        assert_eq!(finished.exit_code, Some(9));
    }

    /// A killed process has no exit code — `last_finished.exit_code` must be
    /// `None`, never a fabricated `0` or the signal's raw number.
    #[tokio::test]
    async fn summary_by_context_reports_killed_with_no_exit_code() {
        let (blocks, ctx_id, principal) = setup();
        let registry = Arc::new(BackgroundRegistry::new());
        let block_id = running_block(&blocks, ctx_id, principal);

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "sleep 30".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(2), || {
            registry.get_for_context(id, ctx_id).filter(|s| s.status == "running")
        })
        .await;
        registry.cancel(id, ctx_id);

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id, ctx_id).filter(|s| s.status == "killed")
        })
        .await;

        let summary = registry.summary_by_context().remove(&ctx_id).expect("ctx_id must have a summary");
        let finished = summary.last_finished.expect("the killed process must be reported");
        assert_eq!(finished.status, "killed");
        assert_eq!(finished.exit_code, None, "a killed process has no exit code to report");
    }

    /// Of several finished processes, `last_finished` must be the one that
    /// finished MOST RECENTLY (by wall time), not the first one spawned or
    /// the first one inserted into the map.
    #[tokio::test]
    async fn summary_by_context_last_finished_picks_the_most_recent_by_finish_time() {
        let (blocks, ctx_id, principal) = setup();
        let registry = Arc::new(BackgroundRegistry::new());

        let block_fast = running_block(&blocks, ctx_id, principal);
        spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "exit 3".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id: block_fast,
            },
        )
        .unwrap();

        let block_slow = running_block(&blocks, ctx_id, principal);
        let id_slow = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                // Comfortably longer than the fast command, so it finishes
                // strictly later regardless of scheduler jitter.
                command: "sleep 0.3; exit 5".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id: block_slow,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(5), || {
            registry.get_for_context(id_slow, ctx_id).filter(|s| s.status == "exited")
        })
        .await;

        let summary = registry.summary_by_context().remove(&ctx_id).expect("ctx_id must have a summary");
        let finished = summary.last_finished.expect("both processes have finished");
        assert_eq!(
            finished.exit_code,
            Some(5),
            "the LATER-finishing process (exit 5) must win over the earlier one (exit 3)"
        );
    }

    /// A context with no background processes at all (past or present) is
    /// simply absent from the map — the caller treats a missing entry like
    /// `BackgroundSummary::default()`, and this must never leak another
    /// context's summary.
    #[tokio::test]
    async fn summary_by_context_omits_contexts_with_no_background_history() {
        let (blocks, ctx_a, principal) = setup();
        let ctx_b = ContextId::new();
        blocks.create_document(ctx_b, DocKind::Conversation, None).unwrap();
        let registry = Arc::new(BackgroundRegistry::new());

        let block_a = running_block(&blocks, ctx_a, principal);
        spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "true".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_a,
                principal_id: principal,
                block_id: block_a,
            },
        )
        .unwrap();

        wait_for(StdDuration::from_secs(5), || {
            registry.summary_by_context().get(&ctx_a).filter(|s| s.last_finished.is_some()).cloned()
        })
        .await;

        let by_ctx = registry.summary_by_context();
        assert!(by_ctx.contains_key(&ctx_a), "ctx_a spawned a process, must be present");
        assert!(
            !by_ctx.contains_key(&ctx_b),
            "ctx_b never spawned anything — must be absent, not a fabricated default entry"
        );
    }

    // --- Characterization tests for the kaish job-system swap ---------
    //
    // `docs/issues.md` ("Background exec → kaish's job system") lists five
    // behaviors of this module that must survive the swap unchanged. Three
    // are already pinned by tests above: Running→Done/Error tied to real
    // exit (`spawn_background_streams_output_and_records_exit_code`,
    // `spawn_background_nonzero_exit_marks_block_error_and_records_code`),
    // `kill_all_for_context` scoping
    // (`kill_all_for_context_only_touches_that_context`), and the output
    // cap's loud marker (`output_cap_truncates_with_a_loud_marker_and_still_records_exit`).
    // The two tests below close the remaining gap: live mid-run streaming
    // and process-group kill reaching children, neither of which any
    // existing test actually exercised (the existing kill test uses
    // `sleep 30`, a single-process command with no child to lose).
    //
    // PDEATHSIG (the orphan guard in `spawn_background`'s `pre_exec`,
    // module docs "Ownership and cleanup") is deliberately NOT tested
    // here — it requires the kernel process itself to die and a real PID
    // namespace to observe the reparenting, which no in-process
    // `#[tokio::test]` can honestly simulate. `contrib/isotest`
    // (docs/isotest.md) already covers it against the real binary in a
    // podman PID namespace, mutation-verified RED without PDEATHSIG
    // (docs/issues.md, "Functional gate exists (2026-08-09)") — that
    // suite is the actual pin for this behavior.

    /// Preserve-across-the-swap item: output must land in the kernel block
    /// WHILE the process is still running, not buffered until it exits.
    /// This is exactly the defect the module docs call out in kaish 0.13's
    /// own `execute_background` ("Why not kaish's own `&`/`JobManager`?",
    /// point 2) — the swap must not reintroduce it here.
    ///
    /// Uses a file sentinel rather than a sleep so the assertion window is
    /// deterministic: the child echoes a marker, then polls for the
    /// sentinel file before echoing a second marker and exiting. While that
    /// file is absent, the child CANNOT have progressed past its polling
    /// loop — so "the marker is already visible in the block" and "the
    /// process is still `Running`" are simultaneously guaranteed facts, not
    /// a timing guess racing against process exit.
    #[tokio::test]
    async fn spawn_background_streams_output_while_still_running() {
        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let continue_file = std::env::temp_dir().join(format!("bg-exec-test-continue-{}", Uuid::now_v7()));
        let continue_path = continue_file.display();

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: format!(
                    "echo mid-run-marker; while [ ! -f {continue_path} ]; do sleep 0.05; done; echo done-marker"
                ),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        // `continue_file` does not exist yet at this point — the child is
        // physically unable to have exited the moment we observe its
        // marker in the block.
        wait_for(StdDuration::from_secs(5), || {
            let content = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap().content;
            content.contains("mid-run-marker").then_some(())
        })
        .await;

        let mid_run_status = registry.get_for_context(id, ctx_id).unwrap().status;
        assert_eq!(
            mid_run_status, "running",
            "the continue-sentinel does not exist yet, so the process cannot have exited — \
             if status is anything but running here, output was not actually streamed live"
        );

        // Let it finish.
        std::fs::write(&continue_file, b"go").expect("write continue sentinel");

        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "exited").then_some(s)
        })
        .await;
        assert_eq!(snap.exit_code, Some(0));

        let final_content = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap().content;
        assert!(
            final_content.contains("done-marker"),
            "process should have completed after the sentinel appeared, got: {final_content:?}"
        );

        let _ = std::fs::remove_file(&continue_file);
    }

    /// Preserve-across-the-swap item: `spawn_background`'s process-group
    /// kill (`setpgid` at spawn + `kill_process_group` on cancel, module
    /// docs "Ownership and cleanup") must reach CHILDREN of the spawned
    /// command, not just the top-level `/bin/sh -c` process.
    /// `kill_background_process_stops_it_and_marks_killed` above only
    /// exercises `sleep 30` — a single simple command most shells `exec`
    /// into directly with no fork at all — so it never actually proves the
    /// "whole tree" half of the contract. This test forces a real fork by
    /// backgrounding a grandchild explicitly and waiting on it.
    ///
    /// Liveness is checked via `/proc/<pid>/stat` rather than `kill(pid,
    /// 0)`: a just-killed-but-not-yet-reaped process is briefly a zombie
    /// (`Z` state), and `kill(pid, 0)` reports success for a zombie — using
    /// it would make this test flaky right after the kill. Reading state
    /// out of `/proc` lets `Z` (or the pid disappearing entirely once
    /// init reaps the orphan) count as "no longer running", which is the
    /// actual claim under test.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn kill_background_process_reaches_process_group_children() {
        fn proc_state(pid: u32) -> Option<char> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // The command-name field (2nd) is parenthesized and may itself
            // contain spaces/parens — split on the LAST ')' rather than on
            // whitespace to stay correct regardless of argv[0].
            let after_paren = stat.rsplit_once(')')?.1;
            after_paren.trim_start().chars().next()
        }
        fn is_running(pid: u32) -> bool {
            matches!(proc_state(pid), Some('R') | Some('S') | Some('D'))
        }

        let (blocks, ctx_id, principal) = setup();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let pidfile = std::env::temp_dir().join(format!("bg-exec-test-childpid-{}", Uuid::now_v7()));
        let pidfile_path = pidfile.display();

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                // Force a real fork: `sleep 60` runs as a genuine
                // grandchild of the `/bin/sh -c` process (backgrounded
                // with `&`), whose pid we capture via `$!`; the parent
                // shell then blocks on `wait` so the whole tree stays
                // alive until cancelled.
                command: format!("sleep 60 & echo $! > {pidfile_path}; wait"),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        let child_pid: u32 = wait_for(StdDuration::from_secs(5), || {
            std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok())
        })
        .await;

        // Sanity: the grandchild really is running before we touch
        // anything — otherwise "not running" later would be meaningless.
        wait_for(StdDuration::from_secs(2), || is_running(child_pid).then_some(())).await;

        assert!(registry.cancel(id, ctx_id), "cancel should find the running entry");

        wait_for(StdDuration::from_secs(5), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "killed").then_some(())
        })
        .await;

        // The load-bearing assertion: the GRANDCHILD — never directly
        // targeted, only the top-level `/bin/sh -c` pid is passed to
        // `kill_process_group` — must also be dead. If process-group kill
        // regressed to a single-pid kill, this child would still be
        // sleeping and this wait would time out.
        wait_for(StdDuration::from_secs(5), || (!is_running(child_pid)).then_some(())).await;

        let _ = std::fs::remove_file(&pidfile);
    }

    // ---------------------------------------------------------------
    // ANSI ingest (docs/ansi-and-beyond.md)
    // ---------------------------------------------------------------

    /// The chunk-boundary property, at the level that actually owns it.
    ///
    /// `AnsiDrain` is the thing that has to survive a sequence split across two
    /// `read()`s: it appends only new text per chunk and has to end up with the
    /// same total the one-shot transform would produce. Splitting at *every*
    /// position proves it without depending on how a pipe happens to chunk.
    #[test]
    fn ansi_drain_is_split_invariant() {
        let input = b"plain \x1b[1;31mbold red\x1b[0m tail \x1b[38;5;42mindexed\x1b[0m\n";
        let (want_text, want_spans) = kaijutsu_ansi::strip(input);

        for split in 0..=input.len() {
            let mut drain = AnsiDrain::new();
            let mut appended = String::new();
            for chunk in [&input[..split], &input[split..]] {
                if chunk.is_empty() {
                    continue;
                }
                appended.push_str(&drain.feed(chunk));
                drain.commit();
            }
            assert_eq!(appended, want_text, "text differs when split at {split}");
            let (original, spans) = drain.finish().expect("input has escape bytes");
            assert_eq!(original, input, "provenance must be byte-exact at split {split}");
            assert_eq!(spans, want_spans, "spans differ when split at {split}");
            assert_eq!(
                kaijutsu_ansi::strip(&original),
                (appended, spans),
                "the standing invariant must hold at split {split}"
            );
        }
    }

    /// The fast path, at the same level: no escape byte anywhere means no
    /// projection at all — no tag, no row, no spans.
    #[test]
    fn ansi_drain_without_escapes_projects_nothing() {
        let mut drain = AnsiDrain::new();
        assert_eq!(drain.feed(b"ordinary output\n"), "ordinary output\n");
        drain.commit();
        assert!(drain.finish().is_none(), "escape-free output must not project");
    }

    /// A block-store variant whose provenance rows are real. `setup()` builds a
    /// db-less store (fine for output/status assertions), but a provenance row
    /// only exists where a `KernelDb` does.
    fn setup_with_db() -> (SharedBlockStore, ContextId, PrincipalId, crate::block_store::DbHandle) {
        use crate::block_store::shared_block_store_with_db;
        let principal = PrincipalId::new();
        let db = Arc::new(parking_lot::Mutex::new(
            crate::kernel_db::KernelDb::temporary().expect("temporary KernelDb"),
        ));
        let ws_id = db.lock().get_or_create_default_workspace(principal).expect("workspace");
        let blocks = shared_block_store_with_db(db.clone(), ws_id, principal);
        let ctx_id = ContextId::new();
        blocks.create_document(ctx_id, DocKind::Conversation, None).expect("create doc");
        (blocks, ctx_id, principal, db)
    }

    /// End-to-end through a real child process, with the SGR sequence
    /// deliberately straddling the 8 KiB read boundary.
    ///
    /// `printf '%8190s\033[31m…'` emits 8190 spaces before the ESC, so the
    /// first `read()` of the 8192-byte buffer ends *inside* `\x1b[31m` — the
    /// exact case a per-chunk `String::from_utf8_lossy` (what this path used to
    /// do) would smear into the block as visible garbage.
    #[tokio::test]
    async fn background_ansi_output_is_stripped_with_spans_and_provenance() {
        let (blocks, ctx_id, principal, db) = setup_with_db();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: r"printf '%8190s\033[31mRED\033[0m\n' ''".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        wait_for(StdDuration::from_secs(10), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "exited").then_some(())
        })
        .await;

        // `finalize_ansi` runs after the drains join, which is after the
        // registry flips terminal — poll for the spans rather than assuming.
        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
            (!s.style_spans.is_empty()).then_some(s)
        })
        .await;

        assert!(
            !snap.content.contains('\u{1b}'),
            "no escape byte may survive the split-sequence boundary into block content"
        );
        assert!(snap.content.ends_with("RED\n"), "content tail: {:?}", &snap.content[8180..]);
        let tag = snap.provenance.as_ref().expect("styled block must be tagged");
        assert_eq!(tag.transform, kaijutsu_ansi::TRANSFORM_NAME);

        let (_, original) = db
            .lock()
            .get_block_provenance(&block_id, kaijutsu_ansi::TRANSFORM_NAME)
            .expect("provenance query")
            .expect("a tagged block must have a row");
        assert_eq!(
            kaijutsu_ansi::strip(&original),
            (snap.content.clone(), snap.style_spans.clone()),
            "strip(original) must reproduce (content, style_spans) for a streamed block too"
        );
        // The span covers exactly "RED", after 8190 spaces.
        assert_eq!(snap.style_spans.len(), 1);
        assert_eq!(snap.style_spans[0].start, 8190);
        assert_eq!(snap.style_spans[0].end, 8193);
    }

    /// The fast path end-to-end: an escape-free background command must leave
    /// its block exactly as it was before this feature existed.
    #[tokio::test]
    async fn background_plain_output_stays_untagged() {
        let (blocks, ctx_id, principal, db) = setup_with_db();
        let block_id = running_block(&blocks, ctx_id, principal);
        let registry = Arc::new(BackgroundRegistry::new());

        let id = spawn_background(
            &registry,
            &blocks,
            SpawnBackgroundParams {
                command: "echo no-escapes-here".to_string(),
                cwd: std::env::temp_dir(),
                env: test_env(),
                context_id: ctx_id,
                principal_id: principal,
                block_id,
            },
        )
        .expect("spawn");

        wait_for(StdDuration::from_secs(10), || {
            let s = registry.get_for_context(id, ctx_id).unwrap();
            (s.status == "exited").then_some(())
        })
        .await;
        let snap = wait_for(StdDuration::from_secs(5), || {
            let s = blocks.get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
            (s.status == Status::Done).then_some(s)
        })
        .await;

        assert_eq!(snap.content, "no-escapes-here\n");
        assert!(snap.style_spans.is_empty());
        assert!(snap.provenance.is_none());
        assert_eq!(
            db.lock()
                .get_block_provenance(&block_id, kaijutsu_ansi::TRANSFORM_NAME)
                .expect("provenance query"),
            None,
        );
    }
}

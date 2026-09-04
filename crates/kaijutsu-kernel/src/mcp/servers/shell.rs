//! `ShellServer` — the in-kernel projection of the `shell` / `shell_write`
//! facades as broker MCP tools (`builtin.shell` / `shell` and
//! `builtin.shell_write` / `shell_write`).
//!
//! **2026-08-17 flag day** (`docs/gate-and-shell-split.md`, "Slice 3", Amy's
//! 2026-08-16 ruling): `shell` is now the unmarked, SAFE name
//! (`ExternalExec::Deny`) — the tool a model reaches for by accident must be
//! the one that cannot hurt anything. `shell_write` is the hot, mutating name
//! (`ExternalExec::Allow`, same behavior `builtin.shell`/`shell` had before
//! the flag day), granted not default. `read_only_shell` retires as a name
//! entirely — no dual-name transition period. A stale caller that still asks
//! for `"shell"` after the flag day lands on the SAFE tool now, never the
//! mutating one — wrong-but-safe, the only acceptable direction for a
//! breaking rename.
//!
//! The `shell`/`shell_write` facades were historically reachable only over the
//! RPC seam: the human shell box and the external MCP `context_shell` (both
//! cross `Broker::check_facade`). The in-kernel LLM agent's tool roster is
//! built from broker tools (`list_visible_tools`), which never included
//! facades — so a native agent in any context "had no shell" no matter what
//! its binding said.
//!
//! This server closes that gap. It exposes tools that materialize the SAME
//! per-context kaish (`KjDispatcher::materialize_context_kaish`) the RPC seam
//! and the rc lifecycle use, so durable env/cwd stay coherent across every
//! surface — there is one shell (per flavor), reached three ways.
//!
//! Gating stays single-axis per flavor: `builtin.shell` and `builtin.
//! shell_write` are each *facade-projected* instances (see
//! [`crate::mcp::binding::FACADE_PROJECTED_INSTANCES`]), so a context sees and
//! can call `shell` exactly when its binding grants `facade:shell`, and
//! `shell_write` exactly when it grants `facade:shell_write` — the same bits
//! that gate the RPC seam. There is no second capability to keep in sync.
//! Default grants stay per-rc, decided per context type at the flag day:
//! `default`/`coder`/`mcp` via `facade:*`; `director` explicitly holds both
//! (operator's console — wants the safe tool AND the hot one available);
//! `toolie` holds `facade:shell` only (never `facade:shell_write`), so it gets
//! exactly the safe tool; `musician` holds neither and is excluded by design —
//! its binding grants only `drive`, because a small local model plays best
//! with an empty tool palette (see
//! `assets/defaults/rc/musician/create/S10-binding.kai`).

use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::super::broker::Broker;
use super::super::context::CallContext;
use super::super::error::{McpError, McpResult};
use kaijutsu_types::RefusalKind;
use kaijutsu_types::shell_envelope::{ShellEnvelope, ShellStatus};
use super::super::server_like::{McpServerLike, ServerNotification};
use super::super::types::{InstanceId, KernelCallParams, KernelTool, KernelToolResult, ToolContent};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellParams {
    /// kaish command to run in your current kernel context.
    pub command: String,
    /// Optional standard input fed to the first stdin-reading command in
    /// `command` (e.g. `jq '.name'`, `grep foo`, `patch`). Lets you pipe a
    /// payload you already have — a generated document, a block's text — into a
    /// pipeline without first writing a temp file. A command that reads no
    /// stdin ignores it.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Run `command` in the background instead of waiting for it to finish.
    /// Returns immediately with a `background_id` + the `block_id` its
    /// output streams into — never the full output. Poll with
    /// `read_background_output`, list with `list_background_processes`, stop
    /// with `kill_background_process` (same server, `builtin.background`).
    ///
    /// A backgrounded command runs as `/bin/sh -c <command>` directly on the
    /// host — NOT through kaish — so shell syntax (`|`, `&&`, `>`) works but
    /// `kj` verbs and kaish variables do not; use the foreground `shell` for
    /// those. Requires the `exec` authority (same as any external command in
    /// the foreground shell) and is never available on `read_only_shell`.
    #[serde(default)]
    pub background: bool,
}

// The kaish-language guidance (word-splitting, globs, `case`/`esac`,
// pre-validation, …) is composed from `kaish-help` at process start instead of
// hand-maintained here — that crate exists so a kaish release updates this
// text everywhere (kaijutsu, kaibo) instead of every embedder re-drifting its
// own prose (kaish's `docs/composable-help.md` step 4). `without_overlay()`
// drops the copy-on-write-overlay paragraph: kaijutsu materializes a fresh
// context kaish per call and never turns overlay on, so that guidance would
// be an active mixed signal ("run `kaish-vfs commit`" for a mode that isn't
// enabled). `LazyLock`, not `const`, because composition is a runtime call
// (`compose()`), not a `&'static str` kaish-help can hand us at compile time.
static COMPOSED_TOOL_DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    kaish_help::compose(
        &kaish_help::Recipe::tool_description().without_overlay(),
        &kaish_help::SchemaContent::new(&[]),
    )
});

/// The composed (kaish-sourced) half of the shell tool description, for the
/// cross-slot duplication guard in `kj::kaish` — the primer must not repeat
/// what already rides here. Exposed rather than duplicated so the guard reads
/// the real bytes, not a second composition that could drift from this one.
///
/// `cfg(test)` rather than `allow(dead_code)`: it exists for the guard, and a
/// production build has no caller.
#[cfg(test)]
pub(crate) fn composed_tool_description() -> &'static str {
    &COMPOSED_TOOL_DESCRIPTION
}

// The kaijutsu-specific half kaish-help can't know: what this tool IS here
// (runs in the caller's current kernel context), that `kj` is in scope for
// context/drift/fork management, and the return contract (one JSON envelope,
// every key always present). Kept as an intro paragraph, separated from the
// composed kaish-language rules by a blank line, so the two sources stay
// visibly distinct rather than blurring into one hand-tuned paragraph the way
// the old static file did.
//
// RETURN_CONTRACT is the one statement of the envelope, shared by both
// flavours — two copies of a shape description drift, and this one is read by
// every model that calls the tool.
const RETURN_CONTRACT: &str = "Returns one JSON object, always the same \
     keys: {stdout, stderr, exit_code, status, did_spill, data, latch, \
     block_id, background_id, content_type, ephemeral, elapsed_ms, error}. \
     `stdout` and `stderr` are separate and are empty strings when the \
     command wrote none. Detect failure with `exit_code != 0` rather than \
     matching text. `status` is done, error, rejected, running, timeout or \
     stream_closed — `rejected` means kaish refused the program and nothing \
     ran, so fix the command text and retry. `exit_code` is null exactly \
     when there is no code to report; null is never evidence of success. \
     `did_spill` true means output was capped and the tail dropped. `data` \
     is the kj structured payload when present.";

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a command in your current kernel context using kaish (会sh). \
         `kj` is in scope for context/drift/fork management. {}\n\n{}",
        RETURN_CONTRACT, &*COMPOSED_TOOL_DESCRIPTION
    )
});

// Read-only variant's kaijutsu-specific half: same return contract, plus what
// makes it read-only (no mutation, no external commands) and the document views
// it can still read (`/v/docs`, `/v/input`) that a host-only read-only shell
// wouldn't have.
//
// Names `shell_write` as where external commands live. The refusal a model
// meets at runtime states its condition and stops there, deliberately — the
// remedy belongs to the layer that configured the condition, which is this
// description. Without it a model reads "command not found" and concludes the
// binary is missing.
static DESCRIPTION_READ_ONLY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Run a READ-ONLY command in your current kernel context using kaish \
         (会sh). This shell cannot mutate anything: every file write/delete/\
         move and every external command is refused — the binary is still \
         installed and on PATH, so reach for `shell_write` when you need to \
         run one, rather than concluding it is missing. Use this tool to \
         inspect — read files, `grep`, `find`, walk the tree, and read the \
         kernel document/input views under `/v/docs` and `/v/input`; `kj` is \
         in scope for read-only context introspection. {}\n\n{}",
        RETURN_CONTRACT, &*COMPOSED_TOOL_DESCRIPTION
    )
});

/// In-kernel broker server backing the `shell` / `shell_write` tools. Holds
/// `Weak<Broker>` (the broker owns this instance's `Arc`) and reaches the
/// shared `KjDispatcher` through the broker, materializing a throwaway context
/// kaish per call. One struct, two flavours selected at construction: the
/// hot, mutating `shell_write` (`facade:shell_write`) and the safe, unmarked
/// `shell` (`facade:shell`) — the name a caller reaches for by default, and
/// what `read_only_shell`/`facade:shell_readonly` used to be before the
/// 2026-08-17 flag day. The constraint lives in the *tool name* so the model
/// never wastes a turn attempting a write it can't do.
pub struct ShellServer {
    instance_id: InstanceId,
    /// The model-facing tool name (`shell`, safe, or `shell_write`, hot).
    tool: &'static str,
    /// When true, materialize a read-only context kaish (no writes, no external
    /// commands; reads — incl. document views — still work).
    read_only: bool,
    broker: Weak<Broker>,
    notif_tx: broadcast::Sender<ServerNotification>,
}

impl ShellServer {
    /// The safe, unmarked tool — `ExternalExec::Deny`. This is what
    /// `builtin.shell_readonly`/`read_only_shell` was before the 2026-08-17
    /// flag day (`docs/gate-and-shell-split.md`, "Slice 3"): the name a caller
    /// reaches for by accident must be the one that cannot hurt anything.
    pub const INSTANCE: &'static str = "builtin.shell";
    pub const TOOL: &'static str = "shell";
    /// The hot, mutating tool — `ExternalExec::Allow`, granted not default.
    /// This is what `builtin.shell`/`shell` was before the flag day; a stale
    /// caller that still asks for the bare name `"shell"` now lands on the
    /// SAFE tool above instead, never here.
    pub const INSTANCE_WRITE: &'static str = "builtin.shell_write";
    pub const TOOL_WRITE: &'static str = "shell_write";

    /// The hot `shell_write` tool (gated by `facade:shell_write`).
    pub fn new(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE_WRITE),
            tool: Self::TOOL_WRITE,
            read_only: false,
            broker,
            notif_tx,
        }
    }

    /// The safe, unmarked `shell` tool (gated by `facade:shell`).
    pub fn new_read_only(broker: Weak<Broker>) -> Self {
        let (notif_tx, _) = broadcast::channel(16);
        Self {
            instance_id: InstanceId::new(Self::INSTANCE),
            tool: Self::TOOL,
            read_only: true,
            broker,
            notif_tx,
        }
    }

    fn description(&self) -> &'static str {
        if self.read_only {
            &DESCRIPTION_READ_ONLY
        } else {
            &DESCRIPTION
        }
    }

    fn broker(&self) -> McpResult<Arc<Broker>> {
        self.broker.upgrade().ok_or_else(|| McpError::InstanceDown {
            instance: self.instance_id.clone(),
            reason: "broker dropped".to_string(),
        })
    }

    /// `background: true` path — bypasses the kaish materialization entirely
    /// (see `background_exec.rs` module docs for why: a per-call kaish
    /// instance can't host a registry that outlives the call, and kaish's own
    /// external-command capture isn't live). Spawns `command` as a direct
    /// host process, streaming into a fresh `Running` block, and returns as
    /// soon as it's registered — never the command's output.
    async fn start_background(
        &self,
        command: &str,
        dispatcher: &crate::kj::KjDispatcher,
        ctx: &CallContext,
    ) -> McpResult<KernelToolResult> {
        // `read_only_shell` is structurally read-only (its materialized kaish
        // pins `ExternalExec::Deny`) — background execution must be refused
        // the same way, by construction, not left to the exec-authority
        // check below (which a read-only role never holds anyway, but this
        // keeps the refusal reason specific to the tool rather than a
        // generic capability-denied).
        if self.read_only {
            return Err(McpError::Protocol(
                "the safe `shell` tool cannot start background processes (it never spawns host subprocesses; use `shell_write`)"
                    .to_string(),
            ));
        }

        // Same authority a synchronous external command requires — `exec` —
        // never a weaker gate. `facade:shell` alone (which `builtin.background`
        // rides too, see FACADE_PROJECTED_INSTANCES) only grants kj/builtins;
        // spawning a real host process needs the dedicated `exec` authority on
        // top, exactly like `ExternalExec::Allow` vs `Deny` in
        // `kj/context_shell.rs`.
        let broker = self.broker()?;
        let exec_granted = broker
            .binding(&ctx.context_id)
            .await
            .is_some_and(|b| b.allows(&crate::mcp::Capability::Exec));
        if !exec_granted {
            return Err(McpError::Protocol(
                "background execution requires the `exec` authority (deny-by-default — see `kj binding allow exec`)"
                    .to_string(),
            ));
        }

        let kernel = dispatcher.kernel();
        let kernel_db = dispatcher.kernel_db();

        // cwd: mirror the synchronous shell's persisted `context_shell.cwd`,
        // but validated as a REAL host directory. A background spawn goes
        // straight to the host (bypassing kaish's VFS), so a virtual-only cwd
        // like `/v/docs` can't be honored — surfaced as an error rather than
        // silently landing somewhere else.
        let persisted_cwd = {
            let db = kernel_db.lock();
            db.get_context_shell(ctx.context_id)
                .ok()
                .flatten()
                .and_then(|row| row.cwd)
        };
        let cwd = match persisted_cwd {
            Some(p) => {
                let path = std::path::PathBuf::from(&p);
                if path.is_dir() {
                    path
                } else {
                    return Err(McpError::Protocol(format!(
                        "background execution needs a host-real cwd; this context's cwd ({p}) doesn't resolve on the host filesystem"
                    )));
                }
            }
            None => kaish_kernel::home_dir(),
        };

        // env: hermetic like the synchronous shell — PATH is the kernel's
        // startup capture, HOME is seeded, and the context's durable env vars
        // are exported (mirrors `EmbeddedKaish::apply_context_config`).
        let mut env = vec![(
            "HOME".to_string(),
            kaish_kernel::home_dir().to_string_lossy().into_owned(),
        )];
        if let Some(path) = kernel.host_path() {
            env.push(("PATH".to_string(), path.to_string()));
        }
        {
            let db = kernel_db.lock();
            if let Ok(vars) = db.get_context_env(ctx.context_id) {
                for v in vars {
                    env.push((v.key, v.value));
                }
            }
        }

        let blocks = dispatcher.block_store();
        let block_id = blocks
            .insert_block_as(
                ctx.context_id,
                None,
                None,
                kaijutsu_types::Role::Tool,
                kaijutsu_types::BlockKind::ToolResult,
                String::new(),
                kaijutsu_types::Status::Running,
                kaijutsu_types::ContentType::Plain,
                Some(ctx.principal_id),
            )
            .map_err(|e| McpError::Protocol(format!("failed to create background output block: {e}")))?;

        let registry = kernel.background_processes();
        let bg_id = crate::background_exec::spawn_background(
            registry,
            blocks,
            crate::background_exec::SpawnBackgroundParams {
                command: command.to_string(),
                cwd,
                env,
                context_id: ctx.context_id,
                principal_id: ctx.principal_id,
                block_id,
            },
        )
        .map_err(|e| McpError::Protocol(format!("failed to start background process: {e}")))?;

        // A started background command reports through the same envelope as
        // every other `shell` return. `stdout` is empty because the output has
        // not happened yet — it streams into `block_id`, which is why the
        // instruction for reaching it rides `stderr` rather than being the
        // whole body.
        let mut env = ShellEnvelope::new(ShellStatus::Running);
        env.background_id = Some(bg_id.to_string());
        env.block_id = Some(block_id.to_key());
        env.stderr = format!(
            "started background process {bg_id}; output streams into block {}. \
             Poll with read_background_output, list with list_background_processes, \
             stop with kill_background_process (builtin.background).",
            block_id.to_key()
        );
        Ok(envelope_result(env))
    }
}

#[async_trait]
impl McpServerLike for ShellServer {
    fn instance_id(&self) -> &InstanceId {
        &self.instance_id
    }

    async fn list_tools(&self, _ctx: &CallContext) -> McpResult<Vec<KernelTool>> {
        let schema = schemars::schema_for!(ShellParams);
        Ok(vec![KernelTool {
            instance: self.instance_id.clone(),
            name: self.tool.to_string(),
            description: Some(self.description().to_string()),
            input_schema: serde_json::to_value(schema).map_err(McpError::InvalidParams)?,
        }])
    }

    async fn call_tool(
        &self,
        params: KernelCallParams,
        ctx: &CallContext,
        _cancel: CancellationToken,
    ) -> McpResult<KernelToolResult> {
        if params.tool != self.tool {
            return Err(McpError::ToolNotFound {
                instance: self.instance_id.clone(),
                tool: params.tool,
            });
        }
        let parsed: ShellParams =
            serde_json::from_value(params.arguments).map_err(McpError::InvalidParams)?;

        // Reach the shared dispatcher (wired at bootstrap via
        // `Broker::set_kj_dispatcher`) and materialize the SAME per-context
        // kaish the RPC seam and rc lifecycle use. Kernel-side callers pass no
        // semantic index + a no-op block source, so `kj`'s synthesis/search
        // tools are degraded here (matching rc/hooks); the core `kj` verbs and
        // shell work. Wiring the real index is a follow-up.
        let broker = self.broker()?;
        let dispatcher = broker
            .kj_dispatcher()
            .await
            .ok_or_else(|| McpError::InstanceDown {
                instance: self.instance_id.clone(),
                reason: "kj dispatcher not wired (Broker::set_kj_dispatcher)".to_string(),
            })?;

        // Pair the kernel's semantic index with a block-backed source so the
        // model's `kj search`/synthesis tools work inside the shell. Both come
        // from the dispatcher (the server installs the index at bootstrap);
        // when embeddings aren't configured the index is `None` and `kj` falls
        // back to non-semantic search rather than failing.
        if parsed.background {
            // NOT gated (`docs/gate-and-shell-split.md`, "Slice 4"): a
            // backgrounded command runs as a direct host subprocess
            // (`start_background`, below), not through kaish, so
            // `plan_program` cannot describe it — there is no kaish source
            // here to plan. Gating this path is separate, unbuilt work; see
            // `kj::shell_gate`'s module docs for the full list of what this
            // gate does and does not cover.
            return self.start_background(&parsed.command, &dispatcher, ctx).await;
        }

        // The hot, mutating tool is gated (`docs/gate-and-shell-split.md`,
        // "Slice 4") — every foreground `shell_write` submission goes
        // through the approval ledger before it runs. The safe `shell` tool
        // (`self.read_only`) is never gated: it cannot mutate anything
        // (`ExternalExec::Deny`), so a gate on it would be pure friction
        // with nothing to protect against.
        //
        // The gate is all-or-nothing per submission (kaish has no
        // per-command interception hook — `kj::shell_gate`'s module docs
        // explain why) and covers exactly what `plan_program` can see: the
        // kaish source text of `parsed.command`. It does NOT cover a
        // program handed to an interpreter as a string argument or over
        // `parsed.stdin`, and it does not cover `start_background` above —
        // see `kj::shell_gate`'s module docs for the full, honest list.
        // Populated only when `run_gate` redeems an escalated ask: the cwd
        // the human's approval was actually asked about. `None` on every
        // other path (the safe read-only tool, a rule-matched auto-allow,
        // or a synthetic caller the gate never pinned) — those all keep
        // running in the context's current cwd, unchanged from before this
        // pin existed.
        let mut pinned_cwd: Option<std::path::PathBuf> = None;
        if !self.read_only {
            // A submission that does not parse is refused here, before the
            // gate — a human is never asked to approve text that cannot be
            // rendered. `ShellGateBuildError` has exactly one variant and it
            // is `Parse`, so this is always the model's mistake to fix and
            // never a fault: it takes the same D-28 `is_error` channel a
            // post-gate rejection takes, not `McpError::Protocol`.
            let spec = match crate::kj::shell_gate::build_shell_gate_spec(&parsed.command) {
                Ok(spec) => spec,
                Err(e) => {
                    let mut env = ShellEnvelope::new(ShellStatus::Rejected);
                    env.error = Some(format!("{e} — nothing was run"));
                    return Ok(envelope_result(env));
                }
            };
            let caller = crate::kj::KjCaller {
                principal_id: ctx.principal_id,
                context_id: Some(ctx.context_id),
                session_id: ctx.session_id,
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            // Nothing here waits. `run_gate` records a durable ask and
            // returns; a human answers from `kj ledger` whenever they
            // answer, and the next attempt at this same command redeems
            // that answer once. See `docs/gate-resume.md`.
            //
            // The four-hop timeout ladder this call used to sit inside
            // still exists (`kaijutsu_types::timeout::gate`) and is now
            // load-bearing for nothing here — it comes out with slice 4,
            // after the kernel can resume an approved action on its own.
            let outcome = crate::kj::gate::run_gate(
                dispatcher.kernel_db(),
                &caller,
                spec,
                dispatcher.kernel().ledger_flows(),
            )
            .await;
            if !outcome.allowed() {
                // Every non-allowed outcome fails closed, and each carries
                // its own kind: a pending ask is waiting, a ledger fault is
                // broken, and a denial is somebody's decision. This gate has
                // no hook behind it, so the tool it guards is the subject.
                let kind = match outcome.verdict {
                    crate::kj::gate::GateVerdict::Pending => RefusalKind::Pending,
                    crate::kj::gate::GateVerdict::Unavailable => RefusalKind::GateUnavailable,
                    crate::kj::gate::GateVerdict::Denied => RefusalKind::Denied,
                    // Guarded by `!outcome.allowed()` directly above. Reaching
                    // here means `allowed()` and this match disagree about
                    // which verdict lets a call through.
                    crate::kj::gate::GateVerdict::Allowed => unreachable!(
                        "an allowed gate outcome reached the refusal path"
                    ),
                };
                return Err(McpError::refused_gate(
                    kind,
                    Self::TOOL_WRITE,
                    outcome.ask.clone(),
                    &outcome.reason,
                ));
            }
            // An approval authorizes THAT operation, not a similar one run
            // wherever the context's cwd has drifted to since the ask was
            // asked (`docs/gate-resume.md`, "Staleness"). `outcome.cwd` is
            // the pin `run_gate` captured at escalation time; carry it
            // through so the command below runs there, not in whatever
            // directory `materialize_context_kaish` would otherwise land in.
            pinned_cwd = outcome.cwd;
        }

        let semantic_index = dispatcher.semantic_index();
        let block_source = dispatcher.block_source();
        let kaish = if self.read_only {
            dispatcher
                .materialize_context_kaish_read_only(
                    "model-shell-ro",
                    ctx.principal_id,
                    ctx.context_id,
                    ctx.session_id,
                    semantic_index,
                    block_source,
                )
                .await
        } else {
            dispatcher
                .materialize_context_kaish(
                    "model-shell",
                    ctx.principal_id,
                    ctx.context_id,
                    ctx.session_id,
                    semantic_index,
                    block_source,
                )
                .await
        }
        .map_err(|e| McpError::Protocol(format!("materialize context shell: {e}")))?;

        // The pinned directory is validated against the shell's own backend
        // (host paths and VFS-only paths like `/v/docs` alike — the same
        // namespace `cd` resolves against, exactly what
        // `EmbeddedKaish::try_set_cwd` checks). A pin that no longer
        // resolves — the directory was removed, or was VFS-only and this
        // context's mounts changed — fails closed and loud here: it never
        // falls back to the context's current cwd, because that fallback is
        // the exact bug this pin exists to close (approve in directory A,
        // run in directory B).
        if let Some(cwd) = &pinned_cwd {
            if !kaish.try_set_cwd(cwd.clone()).await {
                return Err(McpError::Protocol(format!(
                    "shell_write: the approved directory ({}) no longer resolves — \
                     refusing rather than running elsewhere — nothing was run",
                    cwd.display()
                )));
            }
        }

        let mut opts = kaish_kernel::ExecuteOptions::default();
        if let Some(stdin) = parsed.stdin {
            opts = opts.with_stdin(stdin);
        }
        if let Some(cwd) = pinned_cwd {
            opts = opts.with_cwd(cwd);
        }
        // A REJECTED program is not a plumbing fault. kaish refuses parse and
        // validation failures before anything runs, and that is the model's
        // own mistake to read and fix — so it travels the D-28 `is_error`
        // channel with kaish's text verbatim, the same way a nonzero exit
        // does. Wrapping it in `McpError::Protocol` prepended `mcp protocol
        // error:`, which is broker-internal vocabulary its own doc comment
        // says must be converted at the LLM boundary, and it read like
        // kaijutsu was broken rather than like the command was rejected.
        //
        // `Execution` keeps the fault channel: a statement started and
        // something under it broke, which is not a rejection the model can
        // fix by rewriting the command. `is_rejected()` is kaish's own
        // predicate for the split; do not re-derive it from the message text.
        let started = std::time::Instant::now();
        let result = match kaish.execute_with_options(&parsed.command, opts).await {
            Ok(result) => result,
            Err(e) if e.is_rejected() => {
                let mut env = ShellEnvelope::new(ShellStatus::Rejected);
                env.error = Some(e.to_string());
                env.elapsed_ms = Some(started.elapsed().as_millis() as u64);
                return Ok(envelope_result(env));
            }
            Err(e) => {
                return Err(McpError::Protocol(format!("shell execution failed: {e}")));
            }
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(envelope_result(shell_result_to_envelope(result, elapsed_ms)))
    }

    fn notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notif_tx.subscribe()
    }
}

/// Wrap a `ShellEnvelope` as the tool result. The envelope is the
/// model-facing body — `ToolContent::Json`, not text, so the broker's
/// oversize truncation shrinks the strings inside it and re-serializes
/// instead of cutting the serialization in half.
///
/// `is_error` comes from the envelope's status, so the flag and the `status`
/// field can never disagree.
fn envelope_result(env: kaijutsu_types::shell_envelope::ShellEnvelope) -> KernelToolResult {
    let value = env.to_value();
    KernelToolResult {
        is_error: env.is_error(),
        content: vec![ToolContent::Json(value.clone())],
        structured: Some(value),
    }
}

/// Collapse a kaish `ExecResult` into the shared `ShellEnvelope`.
///
/// Every `shell` return travels this shape — `docs/shell-envelope.md` is
/// canonical. The body used to be prose (stdout, with
/// stderr and `[exit N]` appended) and the envelope a side channel, which made
/// the model-facing shape depend on whether the body came out empty: a command
/// with no output fell through to the pretty-printed envelope while every
/// other command produced text.
///
/// A capped result (kaish `did_spill`: exit remapped to 3, real exit stashed
/// in `original_code`) is judged by the command's REAL exit — truncation is
/// not failure, and an error here tempts a model into re-running a command
/// that already succeeded. The truncation stays unmissable as `did_spill`.
fn shell_result_to_envelope(
    result: kaish_kernel::interpreter::ExecResult,
    elapsed_ms: u64,
) -> kaijutsu_types::shell_envelope::ShellEnvelope {
    use kaijutsu_types::shell_envelope::ShellEnvelope;

    let exit_code = if result.did_spill {
        result.original_code.unwrap_or(result.code)
    } else {
        result.code
    };
    let mut env = ShellEnvelope::new(ShellEnvelope::status_for_exit(exit_code));
    env.stdout = result.text_out().into_owned();
    env.stderr = result.err.clone();
    env.exit_code = Some(exit_code);
    env.did_spill = Some(result.did_spill);
    env.elapsed_ms = Some(elapsed_ms);
    // kj verbs (and any builtin that opts in) attach a structured `.data`
    // payload — context-id arrays for list commands, records for inspect. Carry
    // it so consumers don't scrape stdout. `null` when the command set no data
    // (external commands, echo, …).
    env.data = result
        .data
        .as_ref()
        .map(kaish_kernel::interpreter::value_to_json);
    // A `kj` confirmation gate (exit 2, e.g. `kj context remove`) rides
    // kaish's opaque `baggage` channel, distinct from the data-plane `.data`.
    // Surface it so a batch loop reads `latch.hint` and re-runs with
    // `--confirm`, instead of scraping the confirmation prose out of stdout.
    // `null` when the command didn't latch. kaish's own latch — the one that
    // used to hold `rm` here on a typed `.latch` field — is gone as of 0.14,
    // and was never enabled in kaijutsu anyway (`KaishConfig::named` defaults
    // `latch_enabled` off, and we never called `with_latch`).
    env.latch = crate::runtime::kj_builtin::latch_from_result(&result).map(|l| {
        serde_json::json!({
            "command": l.command,
            "target": l.target,
            "hint": l.hint,
        })
    });
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::ContextShellRow;
    use crate::kj::test_helpers::{register_context, test_caller, test_dispatcher};
    use crate::mcp::binding::{Capability, ContextToolBinding};
    use crate::mcp::{InstancePolicy, KernelCallParams};
    use kaijutsu_types::{PrincipalId, SessionId};

    /// The composed half must carry real kaish-help content (a known rule)
    /// and must NOT carry the overlay paragraph — the assertion that would
    /// have caught shipping published kaish-help 0.13 (which forces overlay
    /// guidance into every recipe) instead of the opt-in-overlay rev this
    /// dependency is pinned to.
    #[test]
    fn composed_tool_description_has_a_known_rule_and_excludes_overlay() {
        let text = DESCRIPTION.as_str();
        assert!(
            text.to_lowercase().contains("word splitting"),
            "composed description should carry the no-word-splitting rule: {text}"
        );
        assert!(
            !text.contains("Overlay mode") && !text.contains("kaish-vfs commit"),
            "kaijutsu never enables overlay mode; the description must not tell \
             the model to run `kaish-vfs commit`: {text}"
        );

        let ro_text = DESCRIPTION_READ_ONLY.as_str();
        assert!(
            ro_text.to_lowercase().contains("word splitting"),
            "read-only description should carry the same composed rules: {ro_text}"
        );
        assert!(
            !ro_text.contains("Overlay mode") && !ro_text.contains("kaish-vfs commit"),
            "read-only description must not carry overlay guidance either: {ro_text}"
        );
    }

    /// The kaijutsu-specific wrapper — what kaish-help can't know — must
    /// survive composition: what the tool IS here (current kernel context),
    /// `kj` in scope, and the return contract. The read-only variant also
    /// names its mutation refusal and the document views it can still read.
    #[test]
    fn kaijutsu_wrapper_survives_composition() {
        let text = DESCRIPTION.as_str();
        assert!(text.contains("current kernel context"), "{text}");
        assert!(text.contains("`kj` is in scope"), "{text}");
        assert!(
            text.contains("one JSON object, always the same keys")
                && text.contains("exit_code != 0"),
            "return contract must survive: {text}"
        );

        let ro_text = DESCRIPTION_READ_ONLY.as_str();
        assert!(
            ro_text.contains("cannot mutate anything"),
            "read-only contract must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("/v/docs") && ro_text.contains("/v/input"),
            "read-only document views must survive: {ro_text}"
        );
        assert!(
            ro_text.contains("one JSON object, always the same keys")
                && ro_text.contains("exit_code != 0"),
            "return contract must survive on the read-only variant too: {ro_text}"
        );
        // The runtime refusal names its condition and stops, by design, so
        // this description is the only place a model learns where external
        // commands live. Without it, `command not found` reads as a missing
        // binary and a model abandons a viable path.
        assert!(
            ro_text.contains("`shell_write`") && ro_text.contains("on PATH"),
            "the read-only description must name `shell_write` as where \
             external commands run, and say the binary is still installed: {ro_text}"
        );
    }

    /// Prints the read-only tool description as the model receives it —
    /// `cargo test -p kaijutsu-kernel read_only_description -- --nocapture`.
    /// Published text is read, not grepped: the asserts above pin phrases,
    /// this shows the whole thing.
    #[test]
    fn read_only_description_as_the_model_reads_it() {
        println!("\n--- shell (read-only) description ---\n{}\n--- end ---", DESCRIPTION_READ_ONLY.as_str());
    }

    /// An `Arc<KjDispatcher>` wired into a fresh broker with BOTH the writable
    /// and read-only `ShellServer`s registered — the runtime shape
    /// (`set_self_arc` + `set_kj_dispatcher`), so facade gating across the two
    /// can be exercised together.
    async fn wired() -> (Arc<Broker>, Arc<crate::kj::KjDispatcher>) {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let broker = Arc::new(Broker::new());
        broker.set_kj_dispatcher(&d).await;
        broker
            .register(
                Arc::new(ShellServer::new(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        broker
            .register(
                Arc::new(ShellServer::new_read_only(Arc::downgrade(&broker))),
                InstancePolicy::default(),
            )
            .await
            .unwrap();
        (broker, d)
    }

    /// Params targeting the SAFE, unmarked `shell` tool (`ExternalExec::Deny`)
    /// — the name a stale/default caller reaches for after the flag day.
    fn call(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// Params targeting the HOT, mutating `shell_write` tool
    /// (`ExternalExec::Allow`) — granted not default.
    fn call_write(command: &str) -> KernelCallParams {
        KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    /// `shell_write` is gated (`docs/gate-and-shell-split.md`, "Slice 4"),
    /// and `run_gate` never waits (`docs/gate-resume.md`): a test that calls
    /// it synchronously gets `Pending`/`GatePending` back immediately, with
    /// nothing run, and a durable ask already sitting in the ledger. Answer
    /// that ask directly — no poll loop, no spawned task — the way a human
    /// running `kj ledger allow` would from another shell, then retry the
    /// same call to redeem it.
    fn answer_pending_ask(
        db: Arc<parking_lot::Mutex<crate::kernel_db::KernelDb>>,
        allow: bool,
    ) -> String {
        let db = db.lock();
        let conn = db.conn_for_ledger();
        let row = approval_ledger::ask::list_pending(conn)
            .unwrap()
            .into_iter()
            .next()
            .expect("the gate must have left exactly one pending ask");
        approval_ledger::claim::claim(conn, &row.request_id, b"test-approver").unwrap();
        approval_ledger::decide::decide(
            conn,
            &row.request_id,
            approval_ledger::decide::DecideInput {
                allow,
                decided_by: Some(approval_ledger::decide::Answerer {
                    principal: b"test-approver",
                    context: Some(b"another-seat"),
                }),
                decided_option: Some(if allow { "allow_once" } else { "deny" }),
                remember_scope: None,
                auto_reason: None,
            },
        )
        .unwrap();
        row.request_id
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` alone (no `*`, no
    /// instance grant) must let the model run a command through the SAFE tool.
    /// Post-2026-08-17-flag-day, `facade:shell` is the unmarked/safe grant —
    /// this is the tool a caller reaches for by default.
    #[tokio::test]
    async fn facade_shell_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("sh"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hello-shell"), &cc, CancellationToken::new())
            .await
            .expect("shell call should succeed");

        assert!(!result.is_error, "echo should not be an error");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-shell"), "stdout missing, got: {out:?}");
    }

    /// The mirror on the hot side: `facade:shell_write` alone must let the
    /// model run a command through the mutating tool, under its new name.
    /// Director explicitly holds both facades post-flag-day (the operator's
    /// console wants the safe tool by default and the hot one available).
    #[tokio::test]
    async fn facade_shell_write_runs_a_command_through_the_broker() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("shw"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let pending = broker
            .call_tool(call_write("echo hello-shell-write"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered shell_write submission must escalate and return \
                         immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an open ask is a pending verdict, not a fault: {pending:?}"
        );
        assert!(
            pending.as_refusal().and_then(|r| r.ask_id().map(str::to_owned)).is_some(),
            "the caller gets the ask id as a handle, not as prose: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write("echo hello-shell-write"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed once the pending ask is answered");

        assert!(!result.is_error, "echo should not be an error");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-shell-write"), "stdout missing, got: {out:?}");
    }

    /// The gate's deadline must be ordered under the broker's, so an
    /// unanswered gate returns the gate's honest reason rather than a generic
    /// MCP timeout. This USED to be a clamp computed by hand right here
    /// (`min(gate_wait, mcp_call_timeout_default - 5s)`); it is now enforced
    /// by the shared `kaijutsu_types::timeout::gate` ladder —
    /// `effective_gate_wait()` on the kernel side, `gate::BROKER_CALL` as
    /// this instance's `InstancePolicy` cap (`for_kernel_gated`, NOT the
    /// generic `mcp_call_timeout_default` every other instance uses).
    ///
    /// Asserts the ordering rather than a literal duration, so retuning
    /// either bound cannot quietly invert it. `gate_ladder_fires_caller_first`
    /// (`kaijutsu-types::timeout`) pins the ladder's constants in isolation;
    /// this is the integration check that the call site here actually reads
    /// through it rather than reintroducing a local clamp.
    #[test]
    fn the_gate_deadline_is_ordered_under_the_broker_call_timeout() {
        let t = kaijutsu_types::TimeoutPolicy::default();
        let effective = t.effective_gate_wait();
        assert!(
            effective < kaijutsu_types::timeout::gate::BROKER_CALL,
            "the gate must resolve BEFORE the broker cancels the call, or its reason is lost \
             (effective gate wait {:?}, broker cap {:?})",
            effective,
            kaijutsu_types::timeout::gate::BROKER_CALL
        );
        assert!(
            kaijutsu_types::timeout::gate::MAX_KERNEL_WAIT
                < kaijutsu_types::timeout::gate::BROKER_CALL,
            "if this fails the ladder became a no-op and this test stopped testing anything — \
             the gate ceiling caught up with (or passed) the broker cap it's supposed to clear"
        );
    }

    /// **Slice 1 spec test.** An uncovered `shell_write` submission escalates
    /// and returns IMMEDIATELY, refusing — proven through the real
    /// `ShellServer` → `build_shell_gate_spec` → `run_gate` wiring, not just
    /// `run_gate` in isolation (`kj::gate`'s own
    /// `an_uncovered_multi_statement_submission_escalates` covers the
    /// underlying mechanism). Nothing here times out (`docs/gate-resume.md`
    /// — `run_gate` never waits): a rename of the old
    /// `shell_write_with_no_answer_escalates_and_times_out_refusing_the_call`,
    /// which pinned a wait that no longer exists.
    /// The POST-gate half of the same rule: a program that parses, gets
    /// approved, and is then refused by kaish's validator.
    ///
    /// The pre-gate test below cannot reach this branch — an unparseable
    /// program never builds a gate spec, so it never gets as far as
    /// `execute_with_options`. `break` outside a loop is the opposite shape:
    /// it parses cleanly (a human is shown it and approves it) and the
    /// validator rejects it before any statement runs. That is the case
    /// `KernelError::is_rejected()` exists to separate from a real fault.
    #[tokio::test]
    async fn a_validator_rejection_after_approval_is_also_a_tool_failure() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("validrej"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let bad = "break";

        let _ = broker
            .call_tool(call_write(bad), &cc, CancellationToken::new())
            .await;
        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write(bad), &cc, CancellationToken::new())
            .await
            .expect("a validator rejection must come back as a result, not an Err");

        assert!(result.is_error, "a rejected program must set is_error");
        let envelope = body_of(&result);
        assert_eq!(
            envelope["status"],
            serde_json::json!("rejected"),
            "a refused program is `rejected`, distinct from a command that ran and failed"
        );
        let body = envelope["error"]
            .as_str()
            .expect("a rejection names why in `error`")
            .to_string();
        assert!(
            !body.contains("mcp protocol error"),
            "broker-internal vocabulary must not reach the model; got: {body}"
        );
    }

    /// A program kaish REFUSES reaches the model as a tool failure it can
    /// read, not as a broker protocol fault.
    ///
    /// Before the 0.16 error split, every kaish failure — parse, validation,
    /// genuine IO fault alike — was wrapped in `McpError::Protocol`, whose
    /// Display prepends `mcp protocol error:`. That is broker-internal
    /// vocabulary its own doc says must be converted at the LLM boundary, and
    /// it made "your command was rejected, fix it and retry" read like
    /// kaijutsu was broken. This pins the two halves that matter: `is_error`
    /// is set, and the internal prefix is absent.
    #[tokio::test]
    async fn a_rejected_program_is_a_tool_failure_not_a_protocol_fault() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("reject"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        // An unterminated quote. This is refused BEFORE the gate — a human
        // is never asked to approve text that cannot be rendered — so there
        // is no ask to answer, and the refusal must still not wear protocol
        // clothes.
        let result = broker
            .call_tool(call_write("echo 'unclosed"), &cc, CancellationToken::new())
            .await
            .expect("a refused program must come back as a result, not an Err");

        assert!(result.is_error, "a rejected program must set is_error");
        let envelope = body_of(&result);
        assert_eq!(
            envelope["status"],
            serde_json::json!("rejected"),
            "a refused program is `rejected`, distinct from a command that ran and failed"
        );
        let body = envelope["error"]
            .as_str()
            .expect("a rejection names why in `error`")
            .to_string();
        assert!(
            !body.contains("mcp protocol error"),
            "broker-internal vocabulary must not reach the model; got: {body}"
        );
        assert!(
            !body.is_empty(),
            "kaish's own rejection text must survive; got an empty body"
        );
    }

    #[tokio::test]
    async fn shell_write_with_no_answer_escalates_and_returns_pending_refusing_the_call() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("pending-shw"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let err = broker
            .call_tool(call_write("echo nobody-answers"), &cc, CancellationToken::new())
            .await
            .expect_err("an unanswered gate must refuse immediately, never hang or silently run");
        assert!(
            err.is_refusal(RefusalKind::Pending),
            "an unanswered gate is waiting, distinguishably from a hard denial: {err:?}"
        );
    }

    /// **Slice 1 spec test.** A multi-statement `shell_write` submission a
    /// human denies refuses the WHOLE call — the gate runs entirely before
    /// `execute_with_options` is ever called for this submission, so there
    /// is no partial-execution window to prove separately; this asserts the
    /// refusal itself reaches the caller through the real MCP wiring. The
    /// RULE-covered "one denied statement denies the whole ask" composition
    /// is unit-tested directly against `run_gate` in `kj::gate`
    /// (`a_denied_statement_among_several_refuses_the_whole_submission_and_names_it`).
    /// First call escalates and returns immediately (nothing run); the
    /// retry after a human denies is the one that carries the verdict.
    #[tokio::test]
    async fn shell_write_deny_refuses_the_whole_multi_statement_submission() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("deny-multi"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let pending = broker
            .call_tool(
                call_write("echo one\necho two"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "expected a pending verdict, got {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), false);

        let err = broker
            .call_tool(
                call_write("echo one\necho two"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("a denied gate must refuse the whole submission, not run any of it");
        assert!(
            err.is_refusal(RefusalKind::Denied),
            "a human's no is a denial, never merely unavailable: {err:?}"
        );
    }

    /// **Slice 1 spec test — required.** `kj ledger` (the SAME verb `kj cc
    /// send`'s ask is answered through) must see and answer a `shell_write`
    /// ask with no special-casing by origin: one answering surface for
    /// every gated producer. No concurrency needed any more: the first call
    /// escalates and returns with the ask already durable, `kj ledger`
    /// answers it directly, and the retry redeems the answer.
    #[tokio::test]
    async fn kj_ledger_answers_a_shell_write_ask_like_a_cc_send_ask() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ledger-shw"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let pending = broker
            .call_tool(
                call_write("echo answered-like-cc-send"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        // The id comes back on the refusal itself. Recovering it by
        // splitting a `kj ledger list` line — which this test used to do —
        // is the symptom the refusal shape exists to remove.
        let request_id = pending
            .as_refusal()
            .and_then(|r| r.ask_id().map(str::to_owned))
            .expect("a pending gate hands the caller its ask id");

        let ledger_caller = test_caller();
        let listing = d
            .dispatch(&["ledger".to_string(), "list".to_string()], &ledger_caller)
            .await;
        assert!(
            listing.message().contains("shell_gate"),
            "kj ledger list must show the shell_write ask: {}",
            listing.message()
        );
        assert!(
            listing.message().contains(&request_id),
            "the id the caller was handed must be the id the ledger lists: {}",
            listing.message()
        );

        let allow = d
            .dispatch(
                &["ledger".to_string(), "allow".to_string(), request_id.clone()],
                &ledger_caller,
            )
            .await;
        assert!(allow.is_ok(), "kj ledger allow must answer a shell_write ask: {allow:?}");

        let result = broker
            .call_tool(
                call_write("echo answered-like-cc-send"),
                &cc,
                CancellationToken::new(),
            )
            .await
            .expect("shell_write call should succeed once kj ledger allows it");
        assert!(!result.is_error);
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("answered-like-cc-send"), "stdout missing, got: {out:?}");
    }

    /// **The unresolvable-pin refusal.** The context's cwd at ask time is a
    /// directory that does not exist. The ask escalates and is approved
    /// exactly as normal, but redemption must refuse LOUDLY rather than
    /// fall back to running in the context's current cwd — an approval
    /// authorizes the operation it was asked about, and a pin that no
    /// longer resolves means that operation cannot honestly be run at all.
    ///
    /// Falsified by deleting the `kaish.try_set_cwd` check in `call_tool`
    /// (letting `opts.with_cwd` carry the dead pin straight into
    /// `execute_with_options` unchecked): the call succeeded instead of
    /// refusing — `kaish` accepted the nonexistent cwd silently and ran
    /// `echo` anyway, which is precisely the silent-fallback failure this
    /// refusal exists to prevent. Reverted.
    #[tokio::test]
    async fn a_pin_that_no_longer_resolves_refuses_loudly_not_a_fallback() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("dead-pin"), None, principal);
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&ContextShellRow {
                context_id: ctx_id,
                cwd: Some("/this/directory/does/not/exist/kaijutsu-gate-pin-test".to_string()),
                updated_at: 0,
            })
            .unwrap();
        }
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let pending = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an uncovered submission escalates to an open ask: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let err = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("a pin that no longer resolves must refuse, never fall back to the \
                         context's current cwd");
        match err {
            McpError::Protocol(msg) => {
                assert!(
                    msg.contains("no longer resolves"),
                    "refusal must name the unresolvable-pin condition: {msg}"
                );
                assert!(
                    msg.contains("nothing was run"),
                    "refusal must say nothing ran: {msg}"
                );
            }
            other => panic!("expected a Protocol refusal, got {other:?}"),
        }
    }

    /// A `kj` verb's structured `.data` must survive into the tool result's
    /// `structured` envelope — consumers read full context handles from `data`
    /// instead of scraping stdout (which renders short ids in a table).
    #[tokio::test]
    async fn kj_data_payload_reaches_structured_envelope() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("alpha"), None, principal);
        register_context(&d, Some("beta"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("kj context list"), &cc, CancellationToken::new())
            .await
            .expect("kj context list should succeed");

        assert!(!result.is_error, "kj context list errored: {result:?}");
        let structured = result.structured.expect("structured envelope present");
        let data = structured
            .get("data")
            .and_then(|d| d.as_array())
            .unwrap_or_else(|| panic!("data must be a JSON array, got: {structured}"));
        let labels: Vec<&str> = data.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            labels.contains(&"alpha") && labels.contains(&"beta"),
            "structured data must carry context handles: {labels:?}"
        );
    }

    /// An `echo` (no structured data) leaves `data` null — the field is present
    /// but empty, never fabricated.
    #[tokio::test]
    async fn plain_command_leaves_data_null() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("sh"), None, principal);
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hi"), &cc, CancellationToken::new())
            .await
            .expect("echo should succeed");
        let structured = result.structured.expect("structured envelope present");
        assert!(
            structured.get("data").is_some_and(|d| d.is_null()),
            "echo must leave data null, got: {structured}"
        );
    }

    /// Deny-by-default: a context WITHOUT `facade:shell` (here a read-only-ish
    /// loadout) must be refused at the broker capability gate — the projection
    /// is the only path to the tool, so no facade means no shell.
    #[tokio::test]
    async fn no_facade_is_denied_at_the_gate() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("noshell"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Tool {
            instance: InstanceId::new("builtin.file"),
            tool: "read".to_string(),
        });
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let err = broker
            .call_tool(call("echo nope"), &cc, CancellationToken::new())
            .await
            .expect_err("must be denied without facade:shell");
        assert!(
            matches!(err, McpError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {err:?}"
        );
    }

    /// The tool must be advertised to a `facade:shell` context (so it lands in
    /// the model's roster + `<tools>` system-prompt line) and hidden otherwise.
    #[tokio::test]
    async fn tool_is_listed_only_with_the_facade() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();

        let with = register_context(&d, Some("with"), None, principal);
        let mut b = ContextToolBinding::new();
        b.grant(Capability::Facade("shell".into()));
        broker.set_binding(with, b).await;
        let cc = CallContext::new(principal, with, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(with, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell context should see the shell tool: {visible:?}"
        );

        let without = register_context(&d, Some("without"), None, principal);
        broker.set_binding(without, ContextToolBinding::new()).await;
        let cc2 = CallContext::new(principal, without, SessionId::new(), d.kernel_id());
        let visible2 = broker.list_visible_tools(without, &cc2).await.unwrap();
        assert!(
            !visible2.iter().any(|(name, _)| name == "shell"),
            "no-facade context must not see the shell tool: {visible2:?}"
        );
    }

    /// Helper: the envelope a completed `ExecResult` produces.
    fn envelope_of(r: kaish_kernel::interpreter::ExecResult) -> serde_json::Value {
        shell_result_to_envelope(r, 0).to_value()
    }

    /// Helper: the envelope a `shell` call returned. Every return is one
    /// JSON object, so a test that used to match on a text body reads fields
    /// here instead.
    fn body_of(result: &KernelToolResult) -> serde_json::Value {
        match result.content.as_slice() {
            [ToolContent::Json(v)] => v.clone(),
            other => panic!("a shell result must be one JSON value, got {other:?}"),
        }
    }

    /// Helper: everything the command wrote, both streams. For assertions
    /// about whether some text appeared at all.
    fn streams_of(result: &KernelToolResult) -> String {
        let body = body_of(result);
        format!(
            "{}{}",
            body["stdout"].as_str().unwrap_or_default(),
            body["stderr"].as_str().unwrap_or_default()
        )
    }

    /// Every `shell` return is one JSON object with every key present. This
    /// is the fix for the shape flip a worknote reported: the body used to be
    /// prose, and a command with no output fell through to a pretty-printed
    /// envelope while every other command produced text.
    #[test]
    fn every_return_is_one_json_object_with_every_key() {
        let cases = vec![
            (
                "silent success",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::success(""),
                    0,
                )),
            ),
            (
                "output",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::success("hi"),
                    0,
                )),
            ),
            (
                "failure",
                envelope_result(shell_result_to_envelope(
                    kaish_kernel::interpreter::ExecResult::failure(1, "boom"),
                    0,
                )),
            ),
            ("rejection", {
                let mut env = ShellEnvelope::new(ShellStatus::Rejected);
                env.error = Some("parse error".into());
                envelope_result(env)
            }),
            ("background", {
                let mut env = ShellEnvelope::new(ShellStatus::Running);
                env.background_id = Some("bg-1".into());
                envelope_result(env)
            }),
        ];
        for (label, kr) in cases {
            let body = match kr.content.as_slice() {
                [ToolContent::Json(v)] => v.clone(),
                other => panic!("{label}: body must be one JSON value, got {other:?}"),
            };
            let obj = body
                .as_object()
                .unwrap_or_else(|| panic!("{label}: body must be an object, got {body}"));
            for key in ShellEnvelope::KEYS {
                assert!(obj.contains_key(*key), "{label}: key {key} missing");
            }
            assert_eq!(
                kr.structured.as_ref().expect("structured present"),
                &body,
                "{label}: structured and the model-facing body are one value"
            );
        }
    }

    /// A silent success and a refused program both have empty stdout. Before
    /// the shared envelope they were told apart only by whether the body
    /// happened to be empty, which is what made the shape flip.
    #[test]
    fn silent_success_and_rejection_are_told_apart_by_status() {
        let quiet = envelope_of(kaish_kernel::interpreter::ExecResult::success(""));
        assert_eq!(quiet["stdout"], serde_json::json!(""));
        assert_eq!(quiet["status"], serde_json::json!("done"));
        assert_eq!(quiet["exit_code"], serde_json::json!(0));
        assert_eq!(quiet["error"], serde_json::Value::Null);

        let mut env = ShellEnvelope::new(ShellStatus::Rejected);
        env.error = Some("parse error at 1:6".into());
        let refused = envelope_result(env);
        assert!(refused.is_error, "a refused program is an error");
        let body = refused.structured.unwrap();
        assert_eq!(body["stdout"], serde_json::json!(""));
        assert_eq!(body["status"], serde_json::json!("rejected"));
        assert_eq!(
            body["exit_code"],
            serde_json::Value::Null,
            "nothing ran, so there is no exit code to report"
        );
        assert!(body["error"].as_str().unwrap().contains("parse error"));
    }

    #[test]
    fn conversion_success_with_warnings_keeps_exit_zero_and_surfaces_stderr() {
        let mut r = kaish_kernel::interpreter::ExecResult::success("the-output");
        r.err = "a-warning".to_string();
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(!kr.is_error, "exit 0 stays non-error even with stderr");
        let body = kr.structured.unwrap();
        assert_eq!(body["stdout"], serde_json::json!("the-output"));
        assert_eq!(
            body["stderr"],
            serde_json::json!("a-warning"),
            "stderr is its own field, never folded into stdout"
        );
    }

    #[test]
    fn conversion_surfaces_latch_request_structurally() {
        // A latched destructive op (exit 2) carries its gate on kaish's opaque
        // `baggage` channel (kaish 0.14 deleted the typed `.latch` field). The
        // envelope must surface it so a batch loop reads the gate structurally
        // instead of scraping the confirmation prose out of stdout. Resolves
        // the on-hold docs/issues.md "latch nonce on stderr" entry.
        let r = crate::runtime::kj_builtin::latch_result(
            "kj context remove",
            "doomed",
            "removing a context is destructive",
            "kj context remove doomed --confirm".to_string(),
        );
        let structured = envelope_of(r);
        assert_eq!(
            structured["latch"]["command"],
            serde_json::json!("kj context remove")
        );
        assert_eq!(structured["latch"]["target"], serde_json::json!("doomed"));
        assert_eq!(
            structured["latch"]["hint"],
            serde_json::json!("kj context remove doomed --confirm"),
            "the ready-to-run confirmation command must ride the envelope"
        );

        // A non-latched result carries an explicit null — present as a key so a
        // consumer can test `latch == null` rather than guess at omission.
        let plain = envelope_of(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain["latch"],
            serde_json::Value::Null,
            "a non-latched result leaves `latch` explicitly null"
        );
    }

    #[test]
    fn conversion_nonzero_exit_is_error_and_status_agrees() {
        let r = kaish_kernel::interpreter::ExecResult::failure(3, "boom");
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(kr.is_error, "nonzero exit must be an error");
        let body = kr.structured.unwrap();
        assert_eq!(body["exit_code"], serde_json::json!(3));
        assert_eq!(
            body["status"],
            serde_json::json!("error"),
            "the flag and the status field must never disagree"
        );
    }

    #[test]
    fn conversion_spilled_success_is_not_error_but_signals_truncation() {
        // kaish remaps a capped/spilled result to exit 3, stashing the real
        // exit in `original_code`. Truncation is not failure: flagging it an
        // error tempts a model into re-running a command that succeeded.
        // The truncation must still be unmissable — a model reasoning over a
        // head+tail excerpt as if it were complete output hallucinates.
        let mut r = kaish_kernel::interpreter::ExecResult::success(
            "head…\n[output truncated: spilled to /v/spill/abc]",
        );
        r.did_spill = true;
        r.original_code = Some(r.code);
        r.code = 3;
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(!kr.is_error, "a spilled successful command is not an error");
        let structured = kr.structured.unwrap();
        assert_eq!(
            structured["exit_code"],
            serde_json::json!(0),
            "envelope carries the command's real exit, not kaish's spill marker"
        );
        assert_eq!(structured["status"], serde_json::json!("done"));
        assert_eq!(
            structured["did_spill"],
            serde_json::json!(true),
            "truncation stays unmissable as a field"
        );
    }

    #[test]
    fn conversion_spilled_failure_stays_error_with_original_code() {
        let mut r = kaish_kernel::interpreter::ExecResult::failure(3, "tail of a real failure");
        r.did_spill = true;
        r.original_code = Some(1);
        let kr = envelope_result(shell_result_to_envelope(r, 0));
        assert!(kr.is_error, "a spilled FAILING command is still an error");
        let structured = kr.structured.unwrap();
        assert_eq!(
            structured["exit_code"],
            serde_json::json!(1),
            "the real code, not the spill marker"
        );
        assert_eq!(structured["status"], serde_json::json!("error"));
        assert_eq!(structured["did_spill"], serde_json::json!(true));
    }

    #[test]
    fn conversion_unspilled_results_report_did_spill_false() {
        let plain = envelope_of(kaish_kernel::interpreter::ExecResult::success("ok"));
        assert_eq!(
            plain["did_spill"],
            serde_json::json!(false),
            "did_spill is always present so consumers can test it directly"
        );
    }

    /// The toolie's post-flag-day loadout: `facade:shell` (and NOT
    /// `facade:shell_write`). It must see exactly the safe `shell` tool and
    /// NOT the hot `shell_write` — one shell or the other, never both, for a
    /// narrow role. `read_only_shell`/`facade:shell_readonly` are retired
    /// names as of the 2026-08-17 flag day.
    #[tokio::test]
    async fn safe_role_sees_only_the_shell_tool() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell must expose the safe shell tool: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "shell_write"),
            "facade:shell must NOT expose the hot shell_write tool: {visible:?}"
        );
    }

    /// The mirror: a `facade:shell_write` (hot) role sees `shell_write` and
    /// NOT `shell`. Together with the test above, this is the "one shell or
    /// the other" invariant for the narrow roles.
    #[tokio::test]
    async fn shell_write_role_does_not_see_the_safe_shell() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("rw"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            visible.iter().any(|(name, _)| name == "shell_write"),
            "facade:shell_write must expose the hot shell tool: {visible:?}"
        );
        assert!(
            !visible.iter().any(|(name, _)| name == "shell"),
            "facade:shell_write must NOT expose the safe shell tool: {visible:?}"
        );
    }

    /// End-to-end through `broker.call_tool`: `facade:shell` lets the model
    /// run a *read* command and get its output through the safe, unmarked
    /// tool. Refusal of writes / external commands is enforced structurally
    /// and unit-tested at the `MountBackend` / `ReadOnlyFs` layers; here we
    /// prove the gate opens for a read and the command actually runs in the
    /// read-only materialization.
    #[tokio::test]
    async fn safe_shell_runs_a_read_command() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("roexec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let result = broker
            .call_tool(call("echo hello-ro"), &cc, CancellationToken::new())
            .await
            .expect("safe shell call should succeed");

        assert!(!result.is_error, "echo should not be an error: {result:?}");
        let out = body_of(&result)["stdout"].as_str().unwrap_or_default().to_string();
        assert!(out.contains("hello-ro"), "stdout missing, got: {out:?}");
    }

    /// **Slice 3 spec test 1** (`docs/gate-and-shell-split.md`): a context
    /// bound to the OLD `facade:shell` grant (a stale rc script, a cached
    /// binding, a model's habit — nobody updated it for the flag day) must
    /// see the mutating tool disappear and the safe one take over under the
    /// name `shell` — capability LOSS, never a capability leak. Pins the
    /// "wrong-but-safe, never wrong-but-dangerous" direction the rename must
    /// fail in.
    #[tokio::test]
    async fn stale_facade_shell_grant_loses_write_keeps_the_name() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("stale"), None, principal);

        // The stale grant: whatever an old rc script or cached binding still
        // says, unaware anything changed.
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        // The mutating tool is gone: neither visible...
        let visible = broker.list_visible_tools(ctx_id, &cc).await.unwrap();
        assert!(
            !visible.iter().any(|(name, _)| name == "shell_write"),
            "a stale facade:shell grant must not expose shell_write: {visible:?}"
        );
        // ...nor callable.
        let err = broker
            .call_tool(call_write("echo should-not-run"), &cc, CancellationToken::new())
            .await
            .expect_err("a stale facade:shell grant must not reach the mutating tool");
        assert!(
            matches!(err, McpError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {err:?}"
        );

        // The name "shell" still works — routed to the safe tool now.
        let result = broker
            .call_tool(call("echo still-works"), &cc, CancellationToken::new())
            .await
            .expect("the stale grant must still reach the safe tool under the name `shell`");
        assert!(!result.is_error);
        match result.content.first().expect("content") {
            _ => {
                let out = streams_of(&result);
                assert!(out.contains("still-works"), "got: {out:?}");
            }
        }
    }

    /// **Slice 3 spec test 2**: a context newly granted `facade:shell_write`
    /// (plus the `exec` authority — external spawning is gated on that
    /// authority independent of which facade is granted, see
    /// `kj/context_shell.rs::materialize_context_kaish_inner`) gets exactly
    /// what `builtin.shell` provided before the flag day, under the new name
    /// — proven with a real external binary (`id`), not just a kaish
    /// builtin, so the assertion actually exercises `ExternalExec::Allow`,
    /// not merely that a command ran.
    ///
    /// The `exec` grant must land on TWO brokers: `wired()`'s standalone
    /// broker (what `call_tool` gates against) AND `d.kernel().broker()`
    /// (what `materialize_context_kaish_inner`'s exec-authority check reads,
    /// via `self.kernel().broker()` — a different `Arc<Broker>` than the one
    /// `ShellServer` was registered on for the *synchronous* path; the
    /// `background: true` path checks the server's own `self.broker`
    /// instead, so background tests elsewhere in this file don't need this).
    #[tokio::test]
    async fn shell_write_grant_gets_full_external_exec_under_the_new_name() {
        let (broker, d) = wired().await;
        // Real host root so the shell's default cwd resolves to a real
        // directory and `id` can actually spawn (mirrors
        // `kj/context_shell.rs`'s `unknown_command_fails_fast_exec_granted_shell`).
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("write-exec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding.clone()).await;
        d.kernel().broker().set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let pending = broker
            .call_tool(call_write("id"), &cc, CancellationToken::new())
            .await
            .expect_err("an uncovered submission must escalate and return immediately, nothing run");
        assert!(
            pending.is_refusal(RefusalKind::Pending),
            "an uncovered submission escalates to an open ask: {pending:?}"
        );

        answer_pending_ask(d.kernel_db().clone(), true);

        let result = broker
            .call_tool(call_write("id"), &cc, CancellationToken::new())
            .await
            .expect("shell_write call should succeed once the pending ask is answered");
        assert!(!result.is_error, "`id` should run and exit 0: {result:?}");
        let out = streams_of(&result);
        assert!(
            out.contains("uid="),
            "`id` must have actually spawned as a real external process, got: {out:?}"
        );
    }

    /// **Slice 3 spec test 3 — the fail-safe pin.** A stale `"shell"` request
    /// must NEVER reach `ExternalExec::Allow`, full stop, regardless of what
    /// the caller intended. Same real-binary probe as the test above (`id`),
    /// same real host mount, but through the safe tool under a bare
    /// `facade:shell` grant — the output must NOT show a real spawn. `exec`
    /// is granted too (which no real safe-only role would have) to prove the
    /// refusal is structural on the tool identity, not merely a missing
    /// authority that a different rc grant could paper over.
    #[tokio::test]
    async fn stale_shell_name_never_reaches_external_exec_allow() {
        let (broker, d) = wired().await;
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("stale-exec"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;
        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());

        let result = broker
            .call_tool(call("id"), &cc, CancellationToken::new())
            .await
            .expect("the call itself succeeds structurally — the shell runs, `id` just can't spawn");
        let out = streams_of(&result);
        assert!(
            !out.contains("uid="),
            "a stale `shell` request must NEVER reach ExternalExec::Allow \
             and spawn a real process, got: {out:?}"
        );
    }

    /// `background: true` requires the `exec` authority on top of
    /// `facade:shell_write` — the same gate a synchronous external command
    /// hits, never a weaker one. A context with `facade:shell_write` alone
    /// (no `exec`) must be refused, not silently degrade to a foreground run.
    #[tokio::test]
    async fn background_true_is_denied_without_exec_authority() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-noexec"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("background execution must be denied without the exec authority");
        assert!(
            matches!(err, McpError::Protocol(_)),
            "expected a Protocol denial explaining the missing exec authority, got {err:?}"
        );
    }

    /// End-to-end: `shell_write(background: true)` with `facade:shell_write`
    /// + `exec` returns IMMEDIATELY (a handle + block id, never the command's
    /// output), and the command actually runs — its output shows up in the
    /// returned block a moment later, proving the async path is really
    /// wired, not just accepting the flag and doing nothing.
    #[tokio::test]
    async fn background_true_returns_immediately_and_streams_into_its_block() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-ok"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "echo streamed-bg-output", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");

        assert!(!result.is_error, "starting a background process is not itself an error");
        let structured = result.structured.clone().expect("structured envelope");
        assert_eq!(structured["status"], serde_json::json!("running"));
        let block_key = structured["block_id"].as_str().expect("block_id present").to_string();
        assert!(structured["background_id"].as_str().is_some(), "background_id present");
        // The response body must be a short confirmation, never the command's
        // full output — that's the whole point of backgrounding.
        let out = streams_of(&result);
        assert!(
            !out.contains("streamed-bg-output"),
            "the immediate response must not carry the command's output, got: {out:?}"
        );

        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");
        let start = std::time::Instant::now();
        loop {
            let snap = d
                .block_store()
                .get_block_snapshot(ctx_id, &block_id)
                .unwrap()
                .expect("block exists");
            if snap.content.contains("streamed-bg-output") {
                break;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for background output to stream in");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The safe `shell` tool must refuse `background: true` outright — it
    /// never spawns host subprocesses by construction (its materialized
    /// kaish pins `ExternalExec::Deny`), and background execution must not be
    /// a back door around that.
    #[tokio::test]
    async fn safe_shell_rejects_background_true() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("ro-bg"), None, principal);

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        // Even granting `exec` (which no real safe-shell-only role would
        // have) must not open the door — the refusal is structural on
        // `read_only`, checked before the capability gate.
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE),
            tool: ShellServer::TOOL.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("the safe shell tool must refuse background execution even with exec granted");
        assert!(matches!(err, McpError::Protocol(_)), "expected a Protocol refusal, got {err:?}");
    }

    /// CHARACTERIZATION: block lifecycle, "created up front" half. The
    /// output block's stored `Status` must already be `Running` the instant
    /// `shell(background: true)` returns — not flipped to `Running` by some
    /// later step. Distinct from the `"status": "running"` field in the tool
    /// response (that's the background JOB's status, a different value from
    /// the block's own status); this pins the block directly.
    ///
    /// Reaches into `d.block_store()` for the raw `Status` enum (the MCP tool
    /// surface never exposes it) — expected to remain the shared
    /// primitive across the background-engine swap, unlike
    /// `background_exec`'s own types.
    #[tokio::test]
    async fn background_true_creates_a_running_block_before_the_process_finishes() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-runblock"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            // Long enough that it cannot have exited by the time we check.
            arguments: serde_json::json!({"command": "sleep 2", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let structured = result.structured.unwrap();
        let block_key = structured["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");
        let bg_id = crate::background_exec::BackgroundId::parse(structured["background_id"].as_str().unwrap())
            .expect("valid background id");

        let snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(
            snap.status,
            kaijutsu_types::Status::Running,
            "the output block must be Running immediately after shell(background: true) returns"
        );

        // Clean up the still-running sleep.
        d.kernel().background_processes().cancel(bg_id, ctx_id);
    }

    /// CHARACTERIZATION: block lifecycle, nonzero-exit case. A Running block
    /// must transition to `Status::Error` (never silently `Done`, never
    /// stuck `Running`) when the backgrounded command exits nonzero, and the
    /// real exit code must survive into `list_background_processes`.
    /// Complements
    /// `background_exec::tests::spawn_background_nonzero_exit_marks_block_error_and_records_code`
    /// (same contract, pinned directly against the internal `spawn_background`
    /// API) with the MCP-tool-surface view expected to survive the engine
    /// swap. Reaches into `d.block_store()` for the stored `Status` only.
    #[tokio::test]
    async fn background_true_nonzero_exit_marks_the_block_error() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-nonzero"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "exit 5", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let bg_id = result.structured.as_ref().unwrap()["background_id"].as_str().unwrap().to_string();
        let block_key = result.structured.unwrap()["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");

        let registry = d.kernel().background_processes();
        let parsed_id = crate::background_exec::BackgroundId::parse(&bg_id).unwrap();
        let start = std::time::Instant::now();
        let snap = loop {
            if let Some(s) = registry.get_for_context(parsed_id, ctx_id).filter(|s| s.status == "exited") {
                break s;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for exit");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(snap.exit_code, Some(5), "exit status must never be lost");

        let block_snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
        assert_eq!(
            block_snap.status,
            kaijutsu_types::Status::Error,
            "a nonzero-exit background process must leave the block Error, not Done"
        );
        assert_eq!(block_snap.exit_code, Some(5));
    }

    /// CHARACTERIZATION: refusal guard. `background: true` must refuse a
    /// persisted context cwd that isn't a REAL host directory — a background
    /// spawn goes straight to the host (bypassing kaish's VFS), so a
    /// virtual-only cwd like `/v/docs` can't be honored. Assert both the
    /// refusal AND that it names the offending cwd, per Amy's "assert on the
    /// behavior and the reason, not exact prose" standard.
    #[tokio::test]
    async fn background_true_refuses_a_non_host_real_cwd() {
        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-badcwd"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx_id,
                cwd: Some("/v/docs".to_string()),
                updated_at: kaijutsu_types::now_millis() as i64,
            })
            .unwrap();
        }

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "echo nope", "background": true}),
        };
        let err = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect_err("a non-host-real cwd must refuse background execution");
        match err {
            McpError::Protocol(msg) => {
                assert!(msg.contains("/v/docs"), "refusal should name the offending cwd: {msg}");
                assert!(
                    msg.to_lowercase().contains("cwd") || msg.to_lowercase().contains("host"),
                    "refusal should explain it's a host-realness problem, not a generic error: {msg}"
                );
            }
            other => panic!("expected a Protocol refusal, got {other:?}"),
        }
    }

    /// CHARACTERIZATION: hermetic env. `background_exec.rs` docs promise the
    /// child's environment is the caller's EXPLICIT set (HOME, PATH from
    /// `Kernel::host_path`, and the context's durable env vars) — never this
    /// kernel process's own ambient OS environment. Proven two ways at once:
    /// a var real in this test process's OS env but never threaded through
    /// `start_background` must NOT reach the child, while a context-scoped
    /// env var explicitly set via `kernel_db::set_context_env` — which IS
    /// part of the documented hermetic set — must.
    #[tokio::test]
    async fn background_true_env_is_hermetic_not_inherited() {
        let leak_key = "KAIJUTSU_TEST_BG_ENV_LEAK_MARKER";
        // SAFETY: unique var name avoids cross-test collisions; this crate's
        // test suite already accepts this pattern (see llm/config.rs).
        unsafe {
            std::env::set_var(leak_key, "should-not-leak-into-the-child");
        }

        let (broker, d) = wired().await;
        let principal = PrincipalId::new();
        let ctx_id = register_context(&d, Some("bg-env"), None, principal);
        d.block_store()
            .create_document(ctx_id, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();

        {
            let db = d.kernel_db().lock();
            db.set_context_env(ctx_id, "KJ_CONTEXT_VAR", "context-value").unwrap();
        }

        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell_write".into()));
        binding.grant(Capability::Exec);
        broker.set_binding(ctx_id, binding).await;

        let cc = CallContext::new(principal, ctx_id, SessionId::new(), d.kernel_id());
        let params = KernelCallParams {
            instance: InstanceId::new(ShellServer::INSTANCE_WRITE),
            tool: ShellServer::TOOL_WRITE.to_string(),
            arguments: serde_json::json!({"command": "env", "background": true}),
        };
        let result = broker
            .call_tool(params, &cc, CancellationToken::new())
            .await
            .expect("background start should succeed");
        let block_key = result.structured.unwrap()["block_id"].as_str().unwrap().to_string();
        let block_id = kaijutsu_types::BlockId::from_key(&block_key).expect("valid block key");

        let start = std::time::Instant::now();
        let content = loop {
            let snap = d.block_store().get_block_snapshot(ctx_id, &block_id).unwrap().unwrap();
            if snap.status != kaijutsu_types::Status::Running {
                break snap.content;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for `env` to finish");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        // SAFETY: matches the set_var above.
        unsafe {
            std::env::remove_var(leak_key);
        }

        assert!(
            !content.contains(leak_key),
            "the child must not see this kernel process's own OS env, got: {content:?}"
        );
        assert!(
            content.contains("KJ_CONTEXT_VAR=context-value"),
            "the context's durable env var must reach the child, got: {content:?}"
        );
        assert!(
            content.contains(&format!("HOME={}", kaish_kernel::home_dir().to_string_lossy())),
            "HOME must be seeded from kaish_kernel::home_dir(), got: {content:?}"
        );
        if let Some(path) = d.kernel().host_path() {
            assert!(
                content.contains(&format!("PATH={path}")),
                "PATH must be the kernel's startup capture, got: {content:?}"
            );
        }
    }
}

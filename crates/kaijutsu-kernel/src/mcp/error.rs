//! Broker-internal error type (§4.5, D-26).
//!
//! Named variants only — no `Other(anyhow::Error)` catch-all. Adding a new
//! category requires a new variant AND a decision entry in
//! `docs/tool-system-redesign.md` §6.
//!
//! LLM-visible failures (D-28) are *not* routed through this type; they
//! arrive at the model as `KernelToolResult { is_error: true, … }`. `McpError`
//! is for broker-internal control flow only; conversion happens at the LLM
//! boundary.

use thiserror::Error;

use super::types::InstanceId;
use kaijutsu_types::{AskRef, ContextId, Refusal, RefusalKind, Status};

/// Hook identifier (opaque, stable across restarts of the process).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HookId(pub String);

impl std::fmt::Display for HookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reasons a policy rejection can fire (§5.5).
#[derive(Clone, Debug, Error)]
pub enum PolicyError {
    #[error("tool call exceeded timeout ({timeout_ms} ms) on {instance}")]
    Timeout { instance: InstanceId, timeout_ms: u64 },
    #[error("concurrency cap reached on {instance} (max {max})")]
    ConcurrencyCap { instance: InstanceId, max: usize },
    #[error("result size {size} bytes exceeded max {max} bytes on {instance}")]
    ResultTooLarge {
        instance: InstanceId,
        size: usize,
        max: usize,
    },
}

/// Coalescer-side errors (§5.3). Seat only in Phase 1 — no emitters yet.
#[derive(Clone, Debug, Error)]
pub enum CoalescerError {
    #[error("coalescer window closed while flushing")]
    WindowClosed,
}

/// Cap on how many visible tool names a resolution error lists inline.
/// Above this, the error names only the first `TOOL_LIST_CAP` (sorted) and
/// reports how many more exist instead of enumerating them — a typical
/// context sees on the order of 50 tools (well under the cap), but a
/// context bound to several external MCP servers could see many more.
pub const TOOL_LIST_CAP: usize = 60;

/// Build the `(shown, total)` payload for a tool-resolution error from the
/// full set of names visible to the calling context: sorts and dedups for
/// determinism, then truncates the list the error carries to
/// [`TOOL_LIST_CAP`]. `total` keeps the untruncated count so the message
/// can say how many names were left out.
pub fn tool_name_list(mut names: Vec<String>) -> (Vec<String>, usize) {
    names.sort();
    names.dedup();
    let total = names.len();
    names.truncate(TOOL_LIST_CAP);
    (names, total)
}

/// Render the "here's what you could call instead" clause shared by
/// [`McpError::UnknownToolName`] and [`McpError::LoadoutDenied`] — same
/// underlying data (a context's visible-tool list), so the two messages
/// can't drift apart on how they describe it.
fn available_tools_clause(available: &[String], total: usize) -> String {
    if available.is_empty() {
        return "no tools are visible to this context".to_string();
    }
    let joined = available.join(", ");
    let tool_word = if total == 1 { "tool is" } else { "tools are" };
    if total > available.len() {
        let omitted = total - available.len();
        format!(
            "{total} {tool_word} visible to this context; showing the first {} and \
             omitting {omitted} more (call tool_search to find the rest): {joined}",
            available.len()
        )
    } else {
        format!("{total} {tool_word} visible to this context: {joined}")
    }
}

/// Broker-internal errors (§4.5, D-26).
#[derive(Debug, Error)]
pub enum McpError {
    #[error("server does not support this operation")]
    Unsupported,

    #[error("tool `{tool}` not found on instance {instance}")]
    ToolNotFound { instance: InstanceId, tool: String },

    /// `tool_name` never resolved to anything in the broker's unfiltered
    /// registry — a typo or a hallucinated name, as distinct from
    /// [`McpError::LoadoutDenied`] (the name is real, this context's
    /// loadout just doesn't grant it). Carries the names actually visible
    /// to the calling context so the caller has something to retry with
    /// instead of a bare "not found". The one construction site (central
    /// broker dispatch, `Kernel::dispatch_tool_via_broker_with_cancel`)
    /// already has the visible-tool list in hand; this variant retires
    /// that call site's former fallback of building a `ToolNotFound` with
    /// an empty `InstanceId`, which named no server because there wasn't
    /// one to name.
    #[error("tool `{tool}` does not exist — check for a typo. {}", available_tools_clause(available, *total))]
    UnknownToolName {
        tool: String,
        available: Vec<String>,
        total: usize,
    },

    #[error("instance {0} not registered with broker")]
    InstanceNotFound(InstanceId),

    #[error("instance {instance} is down: {reason}")]
    InstanceDown {
        instance: InstanceId,
        reason: String,
    },

    #[error("invalid params: {0}")]
    InvalidParams(#[from] serde_json::Error),

    #[error("mcp protocol error: {0}")]
    Protocol(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("call cancelled")]
    Cancelled,

    /// A refusal about the caller's standing — the gate's three states,
    /// which are one refusal with three kinds. Branch on
    /// [`Refusal::kind`]; the three teach opposite lessons and collapsing
    /// any two of them is the defect `docs/error-chain.md` exists to name.
    ///
    /// [`RefusalKind::Denied`] means a human or a rule decided no.
    /// [`RefusalKind::GateUnavailable`] means the control broke — still
    /// fail-closed, but honest, so a model can retry or escalate instead of
    /// learning "that action is refused" from a gate that was simply absent.
    /// [`RefusalKind::Pending`] means a durable ask is open and nothing ran;
    /// the action runs when the answer lands, not when this call returned.
    ///
    /// `Refusal::subject` is a hook id here, and it is allowed to be empty:
    /// the direct `shell_write` gate has no hook behind it, and requiring
    /// one is what forced that path — the one a model actually takes — to
    /// report verdicts as `Protocol` faults instead.
    #[error("{0}")]
    Refused(Refusal),

    #[error("tool `{tool}` on instance {instance} is not in this context's capability allow-set")]
    CapabilityDenied { instance: InstanceId, tool: String },

    #[error("facade `{facade}` is not in this context's capability allow-set")]
    FacadeDenied { facade: String },

    /// A tool call named something that exists somewhere in the broker's
    /// registry, but this context's loadout/binding doesn't grant it — as
    /// distinct from [`McpError::UnknownToolName`], which means the name
    /// never resolved to anything at all (a typo or a hallucinated tool).
    /// Both look identical to `binding.resolve()` (deny-by-default hides the
    /// tool the same way either way), so the dispatch path checks the
    /// unfiltered registry to tell them apart before reporting which one
    /// happened. Carries the same visible-tool-list data as
    /// `UnknownToolName` — this is arguably where it helps most: the tool
    /// exists, this context may not call it, here is what it may call
    /// instead.
    #[error(
        "tool `{tool}` denied to context {context}: not granted by its loadout/binding \
         (deny-by-default — see `kj binding show`/`kj binding allow`). {}",
        available_tools_clause(available, *total)
    )]
    LoadoutDenied {
        context: ContextId,
        tool: String,
        available: Vec<String>,
        total: usize,
    },

    /// The context's tool binding could not be read from the kernel DB (a
    /// real storage/IO failure, not "never bound" — that case returns an
    /// empty binding, not an error). Distinct from `CapabilityDenied` so a
    /// storage fault doesn't masquerade as a deliberate capability decision.
    #[error("could not load context {context}'s tool binding: {reason}")]
    BindingUnavailable { context: ContextId, reason: String },

    #[error("hook recursion depth exceeded ({depth})")]
    HookRecursionLimit { depth: u32 },

    #[error("coalescer error: {reason}")]
    Coalescer { reason: CoalescerError },

    #[error("policy violation: {0}")]
    Policy(#[from] PolicyError),
}

impl McpError {
    /// The status the blocks of a call that this error stopped should settle
    /// to.
    ///
    /// `GatePending` settles to `Status::Waiting`, everything else to
    /// `Status::Error`. The distinction is the same one the variants' own
    /// `Display` text makes and exists for the same reason: an unanswered
    /// question is not a refusal, and a block left `Error` says it was.
    ///
    /// One mapping, so the shell paths that settle blocks themselves cannot
    /// each decide differently.
    pub fn settled_block_status(&self) -> Status {
        match self {
            McpError::Refused(r) if r.kind.is_pending() => Status::Waiting,
            _ => Status::Error,
        }
    }

    /// The refusal behind this error, when it is one.
    ///
    /// The gate's own variant answers directly; the three capability
    /// variants are translated, because they are the same idea told to the
    /// caller — *you may not do this, here is what to change* — and
    /// `docs/error-chain.md` gives gate and capability one shared shape on
    /// the wire. Every other variant is a fault and answers `None`.
    pub fn as_refusal(&self) -> Option<Refusal> {
        match self {
            McpError::Refused(r) => Some(r.clone()),
            McpError::CapabilityDenied { tool, .. } => Some(Refusal {
                kind: RefusalKind::CapabilityDenied,
                reason: self.to_string(),
                subject: tool.clone(),
                ask: None,
                remedy: None,
            }),
            McpError::FacadeDenied { facade } => Some(Refusal {
                kind: RefusalKind::FacadeDenied,
                reason: self.to_string(),
                subject: facade.clone(),
                ask: None,
                remedy: None,
            }),
            McpError::LoadoutDenied { tool, .. } => Some(Refusal {
                kind: RefusalKind::LoadoutDenied,
                reason: self.to_string(),
                subject: tool.clone(),
                ask: None,
                remedy: Some(format!("kj binding allow {tool}")),
            }),
            _ => None,
        }
    }

    /// The gate's refusals, composed one way so the three cannot drift
    /// apart in how they read.
    ///
    /// `subject` names what the gate guards — a hook id when a hook is
    /// behind it, the tool's name when none is. It may be empty; a gate
    /// with nothing to name says "the approval gate" rather than leaving a
    /// hole where a name belongs.
    ///
    /// `reason` is the gate's own sentence and is not restated here: each
    /// layer adds what the one outside it lacks.
    pub fn refused_gate(
        kind: RefusalKind,
        subject: &str,
        ask: Option<AskRef>,
        reason: &str,
    ) -> Self {
        let who = if subject.is_empty() {
            "the approval gate".to_string()
        } else {
            format!("gate for {subject}")
        };
        let headline = match kind {
            RefusalKind::Pending => format!("{who} is waiting on a human"),
            RefusalKind::GateUnavailable => format!("{who} had nothing to answer it"),
            _ if subject.is_empty() => "the approval gate refused this".to_string(),
            _ => format!("denied by hook {subject}"),
        };
        // A refusal with nothing more to say is the headline alone. A hook
        // that gave a reason keeps it: a verdict delivered without one is
        // indistinguishable from a broken control, and the reason was
        // already being written to the journal one layer away.
        let reason = if reason.is_empty() {
            headline
        } else {
            format!("{headline}: {reason}")
        };
        // Only an open question has something to answer. A denial and a
        // broken control both name an ask the caller can read, but neither
        // gets better by answering it again.
        let remedy = ask
            .as_ref()
            .filter(|_| kind.is_pending())
            .map(|a| format!("kj ledger allow {} — or deny it", a.request_id));
        McpError::Refused(Refusal {
            kind,
            reason,
            subject: subject.to_string(),
            ask,
            remedy,
        })
    }

    /// A hook decided no with nothing further to say — it exited non-zero
    /// and asked nobody. Use [`Self::refused_gate`] with
    /// [`RefusalKind::Denied`] when a reason or an ask exists: a human's
    /// "no" on an ask has both, and dropping them leaves a verdict that
    /// reads exactly like a broken control.
    pub fn denied_by_hook(by_hook: HookId) -> Self {
        Self::refused_gate(RefusalKind::Denied, &by_hook.0, None, "")
    }

    /// A gate fired and never reached a verdict.
    pub fn gate_unavailable(subject: Option<HookId>, ask: Option<AskRef>, reason: String) -> Self {
        Self::refused_gate(
            RefusalKind::GateUnavailable,
            &subject.map(|h| h.0).unwrap_or_default(),
            ask,
            &reason,
        )
    }

    /// A durable ask is open and nothing ran.
    pub fn gate_pending(subject: Option<HookId>, ask: Option<AskRef>, reason: String) -> Self {
        Self::refused_gate(
            RefusalKind::Pending,
            &subject.map(|h| h.0).unwrap_or_default(),
            ask,
            &reason,
        )
    }

    /// Whether this is a refusal of `kind`. The check a caller makes when it
    /// cares which of the three gate states it got.
    pub fn is_refusal(&self, kind: RefusalKind) -> bool {
        matches!(self, McpError::Refused(r) if r.kind == kind)
    }

    /// Whether this is a refusal of `kind` raised by `subject` — the hook
    /// id, tool or facade named in the refusal.
    pub fn is_refusal_from(&self, kind: RefusalKind, subject: &str) -> bool {
        matches!(self, McpError::Refused(r) if r.kind == kind && r.subject == subject)
    }
}

pub type McpResult<T> = Result<T, McpError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::gate::{GateOutcome, GateVerdict, PENDING_REASON_RETRY, ask_ref};
    use approval_ledger::types::ApprovalStatus;
    use kaijutsu_types::AskStatus;

    const ASK: &str = "01a03e66-fa87-7680-a578-305f15202d4e";

    fn pending_outcome() -> GateOutcome {
        GateOutcome {
            verdict: GateVerdict::Pending,
            ask: Some(ask_ref(ASK.to_string(), ApprovalStatus::Pending)),
            cwd: None,
            // The REAL text `run_gate` uses for a hook ask (`exec_source:
            // None`), not a stand-in — a stand-in makes this test unable to
            // fail when that text regresses.
            reason: PENDING_REASON_RETRY.to_string(),
        }
    }

    /// The gate messages nest — `McpError` wraps the broker's summary, which
    /// wraps `GateOutcome::reason` — so each layer must add what the one
    /// outside it lacks. Composed, this said "waiting for a human" three
    /// times before it reached a model, which is prose we ship.
    ///
    /// Falsified by putting "waiting for a human" back into either inner
    /// layer.
    #[test]
    fn a_pending_gate_says_it_is_waiting_exactly_once() {
        let outcome = pending_outcome();
        let rendered = McpError::gate_pending(
            Some(HookId("lfm2d-advisory".to_string())),
            outcome.ask.clone(),
            outcome.ask_summary(),
        )
        .to_string();

        assert_eq!(
            rendered.matches("waiting").count(),
            1,
            "each layer must add new information, not restate the last: {rendered}"
        );
        // The three facts a reader needs, each present once.
        assert!(rendered.contains("lfm2d-advisory"), "names the hook: {rendered}");
        assert!(rendered.contains("01a03e66"), "names the ask: {rendered}");
        assert!(rendered.contains("kj ledger allow"), "says what to do: {rendered}");
        assert!(
            !rendered.contains("denied"),
            "a pending gate is not a denial: {rendered}"
        );
    }

    /// The id is the point of the refusal shape: a caller gets the handle
    /// without parsing the sentence it also appears in. Before this, an ask
    /// id existed only inside that sentence.
    ///
    /// Falsified by dropping the `ask` argument on the way into `Refusal`.
    #[test]
    fn a_pending_gate_hands_back_the_ask_id_structurally() {
        let outcome = pending_outcome();
        let err = McpError::gate_pending(
            Some(HookId("lfm2d-advisory".to_string())),
            outcome.ask.clone(),
            outcome.ask_summary(),
        );

        let refusal = err.as_refusal().expect("a gate refusal is a refusal");
        assert_eq!(refusal.ask_id(), Some(ASK), "the handle, not the prose");
        assert_eq!(refusal.kind, RefusalKind::Pending);
        assert_eq!(refusal.subject, "lfm2d-advisory");
        assert_eq!(
            refusal.ask.as_ref().map(|a| a.status),
            Some(AskStatus::Pending),
        );
        assert!(
            refusal.remedy.as_deref().is_some_and(|r| r.contains(ASK)),
            "the remedy names the ask to answer: {:?}",
            refusal.remedy
        );
    }

    /// A gate with no hook behind it still produces a real verdict. The
    /// direct `shell_write` gate is exactly this case, and a mandatory hook
    /// id is what used to force it onto `McpError::Protocol` — a fault
    /// variant carrying a verdict.
    ///
    /// Falsified by making `subject` required again.
    #[test]
    fn a_hookless_gate_still_refuses_with_a_verdict() {
        let outcome = pending_outcome();
        let err = McpError::gate_pending(None, outcome.ask.clone(), outcome.ask_summary());

        assert!(err.is_refusal(RefusalKind::Pending), "a verdict, not a fault");
        assert_eq!(err.settled_block_status(), Status::Waiting);
        let refusal = err.as_refusal().expect("still a refusal");
        assert_eq!(refusal.subject, "", "there is no hook to name");
        assert_eq!(refusal.ask_id(), Some(ASK), "the ask survives without a hook");
        let rendered = err.to_string();
        assert!(
            !rendered.contains("gate for  "),
            "an absent hook leaves no hole in the message: {rendered}"
        );
    }

    /// The broken-control message nests the same way and had the same
    /// doubling ("gate unavailable" inside "had nothing to answer it").
    #[test]
    fn an_unavailable_gate_does_not_restate_itself() {
        let outcome = GateOutcome {
            verdict: GateVerdict::Unavailable,
            ask: None,
            cwd: None,
            reason: "the ledger could not be reached".to_string(),
        };
        let rendered = McpError::gate_unavailable(
            Some(HookId("lfm2d-advisory".to_string())),
            None,
            outcome.ask_summary(),
        )
        .to_string();

        assert_eq!(
            rendered.matches("nothing to answer").count() + rendered.matches("unavailable").count(),
            1,
            "the broken-control fact belongs to one layer: {rendered}"
        );
        assert!(rendered.contains("no ask was recorded"), "{rendered}");
    }

    /// The three gate states share one variant now, so `settled_block_status`
    /// reads the KIND rather than the variant — and only `Pending` settles
    /// to `Waiting`. `GateUnavailable` is the trap: it is also not a "no",
    /// but nothing will ever come back to move its blocks, and calling it
    /// `Waiting` would leave a block waiting on an answer no one is
    /// composing.
    #[test]
    fn only_a_pending_gate_settles_blocks_to_waiting() {
        let hook = || Some(HookId("lfm2d-advisory".to_string()));

        assert_eq!(
            McpError::gate_pending(hook(), None, "r".into()).settled_block_status(),
            Status::Waiting
        );
        assert_eq!(
            McpError::denied_by_hook(HookId("lfm2d-advisory".to_string()))
                .settled_block_status(),
            Status::Error,
            "someone said no"
        );
        assert_eq!(
            McpError::gate_unavailable(hook(), None, "r".into()).settled_block_status(),
            Status::Error,
            "a broken control has no answer coming"
        );
        assert_eq!(
            McpError::Cancelled.settled_block_status(),
            Status::Error,
            "a non-gate error is not a question"
        );
    }

    /// A capability refusal is the same idea told to the caller — you may
    /// not do this, here is what to change — so it reaches the wire through
    /// the same shape, carrying the name to grant. A fault does not.
    ///
    /// Falsified by returning `Some` for any fault variant.
    #[test]
    fn capability_denials_are_refusals_and_faults_are_not() {
        let denied = McpError::FacadeDenied { facade: "editor".to_string() };
        let refusal = denied.as_refusal().expect("a capability decision is a refusal");
        assert_eq!(refusal.kind, RefusalKind::FacadeDenied);
        assert_eq!(refusal.subject, "editor");
        assert!(refusal.ask.is_none(), "a capability asks nobody");

        let loadout = McpError::LoadoutDenied {
            context: ContextId::new(),
            tool: "block_edit".to_string(),
            available: vec![],
            total: 0,
        };
        let refusal = loadout.as_refusal().expect("loadout denial is a refusal");
        assert_eq!(
            refusal.remedy.as_deref(),
            Some("kj binding allow block_edit"),
            "names the grant that would fix it",
        );

        for fault in [McpError::Cancelled, McpError::Unsupported] {
            assert!(
                fault.as_refusal().is_none(),
                "{fault} is a fault; the caller learns nothing about its standing",
            );
        }
    }
}

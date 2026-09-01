//! A refusal about the caller's standing.
//!
//! One shape, shared by the gate and capability families: *this principal,
//! in this context, may not do this right now — and here is the thing to
//! present or change so it can.*
//!
//! A refusal is a **result**, never a transport fault. The machinery worked
//! and produced a considered answer, so the caller learns something true and
//! must not retry blindly. `docs/error-chain.md` carries the rule and the
//! boundary test for what must not be folded in here: a duplicate label is
//! about the name, not the caller; a read-only mount is a property of the
//! mount; a peer's verdict belongs to the peer.

use serde::{Deserialize, Serialize};

/// Which refusal this is. The three gate variants teach opposite lessons and
/// must never be collapsed: `Denied` means someone said no, `GateUnavailable`
/// means the control was broken, and `Pending` means the question is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// A human or a rule decided no. Retrying the same call gets the same
    /// answer.
    Denied,
    /// A durable ask is recorded and **nothing ran**. The action runs when
    /// the answer lands, not when this call returned. Do other work and come
    /// back; do not retry in a loop.
    Pending,
    /// An ask hook fired and never reached a verdict — no dispatcher was
    /// wired, or the ledger could not be reached. Fails closed like a
    /// denial, but it is a broken control rather than an answer, so a
    /// caller may retry or escalate through another channel.
    GateUnavailable,
    /// The named tool is not in this context's capability allow-set.
    CapabilityDenied,
    /// The named facade is not in this context's capability allow-set.
    FacadeDenied,
    /// The tool exists in the broker's registry; this context's
    /// loadout/binding does not grant it. Distinct from a tool that does not
    /// exist at all, which is a plain not-found.
    LoadoutDenied,
}

impl RefusalKind {
    /// Whether an answer is still coming. True only for [`Self::Pending`] —
    /// the one state where waiting is the right move.
    pub fn is_pending(self) -> bool {
        matches!(self, RefusalKind::Pending)
    }

    /// Whether a decision was actually reached. False for
    /// [`Self::GateUnavailable`] (nothing decided; the control broke) and
    /// for [`Self::Pending`] (nothing decided yet).
    pub fn is_decided(self) -> bool {
        !matches!(self, RefusalKind::Pending | RefusalKind::GateUnavailable)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RefusalKind::Denied => "denied",
            RefusalKind::Pending => "pending",
            RefusalKind::GateUnavailable => "gate_unavailable",
            RefusalKind::CapabilityDenied => "capability_denied",
            RefusalKind::FacadeDenied => "facade_denied",
            RefusalKind::LoadoutDenied => "loadout_denied",
        }
    }
}

impl std::fmt::Display for RefusalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an ask stands. Mirrors `approval_ledger::types::ApprovalStatus` for
/// the wire.
///
/// The two enums are separate because `approval-ledger` and this crate are
/// independent leaves — neither depends on the other, and making one depend
/// on the other to share six variants would be the more expensive mistake.
/// The correspondence is pinned by a test in the kernel, where both types
/// are in scope; that test is what keeps a new ledger variant from silently
/// arriving on the wire as something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskStatus {
    Pending,
    Claimed,
    Allowed,
    Denied,
    Expired,
    Abandoned,
}

impl AskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AskStatus::Pending => "pending",
            AskStatus::Claimed => "claimed",
            AskStatus::Allowed => "allowed",
            AskStatus::Denied => "denied",
            AskStatus::Expired => "expired",
            AskStatus::Abandoned => "abandoned",
        }
    }

    /// Whether this ask can still be answered. A caller polling an ask stops
    /// when this goes false.
    pub fn is_open(self) -> bool {
        matches!(self, AskStatus::Pending | AskStatus::Claimed)
    }
}

impl std::fmt::Display for AskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The durable ask a refusal belongs to.
///
/// A pair rather than two optional fields because the id and the status are
/// the same fact: a row either exists with both, or does not exist at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskRef {
    /// The ledger row's id. This is what a caller shows a human, polls with,
    /// and later presents to redeem — it is the handle, not a display
    /// string, so it is never rendered into prose and re-parsed out.
    pub request_id: String,
    pub status: AskStatus,
}

/// A refusal that reached the caller as a result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub kind: RefusalKind,
    /// Why, in one line, for whoever reads it. Always populated. Branch on
    /// [`Self::kind`] rather than matching on this text.
    pub reason: String,
    /// What refused, or what is missing: a hook id for a gate, a tool or
    /// facade name for a capability. Empty when there is nothing to name.
    pub subject: String,
    /// The durable ask, when the gate got far enough to record one. `None`
    /// on a capability refusal, which asks nobody, and on the two gate
    /// faults that happen before anything durable exists.
    pub ask: Option<AskRef>,
    /// The command that changes the answer, when there is one — answering
    /// the ask, or granting the capability. `None` when nothing the caller
    /// can run would help.
    pub remedy: Option<String>,
}

impl Refusal {
    /// The ask id, when there is one. The whole point of the structured
    /// shape: a caller gets the handle without parsing a message.
    pub fn ask_id(&self) -> Option<&str> {
        self.ask.as_ref().map(|a| a.request_id.as_str())
    }

    /// Whether an answer is still coming.
    pub fn is_pending(&self) -> bool {
        self.kind.is_pending()
    }
}

impl std::fmt::Display for Refusal {
    /// One line, for a log or a block's stderr. Each fact appears once: the
    /// kind, what refused, which ask, and what to do about it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)?;
        if let Some(ask) = &self.ask {
            write!(f, " (ask {}, {})", ask.request_id, ask.status)?;
        }
        if let Some(remedy) = &self.remedy {
            write!(f, " — {remedy}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three gate kinds answer "is an answer coming?" and "was anything
    /// decided?" differently, and `GateUnavailable` is the one that looks
    /// like both neighbours and is neither.
    ///
    /// Falsified by folding `GateUnavailable` into either arm.
    #[test]
    fn gate_unavailable_is_neither_pending_nor_decided() {
        assert!(!RefusalKind::GateUnavailable.is_pending());
        assert!(!RefusalKind::GateUnavailable.is_decided());

        assert!(RefusalKind::Pending.is_pending());
        assert!(!RefusalKind::Pending.is_decided());

        assert!(!RefusalKind::Denied.is_pending());
        assert!(RefusalKind::Denied.is_decided());
    }

    /// A capability refusal reached a real decision with nobody to ask.
    #[test]
    fn capability_refusals_are_decided_and_askless() {
        for kind in [
            RefusalKind::CapabilityDenied,
            RefusalKind::FacadeDenied,
            RefusalKind::LoadoutDenied,
        ] {
            assert!(kind.is_decided(), "{kind} decided nothing");
            assert!(!kind.is_pending(), "{kind} claims an answer is coming");
        }
    }

    /// The rendered line states each fact once and never invents an id.
    #[test]
    fn display_names_the_ask_once_and_omits_it_when_absent() {
        let pending = Refusal {
            kind: RefusalKind::Pending,
            reason: "gate for lfm2d-advisory is waiting on a human".to_string(),
            subject: "lfm2d-advisory".to_string(),
            ask: Some(AskRef {
                request_id: "01a05d19-0000-7000-8000-000000000000".to_string(),
                status: AskStatus::Pending,
            }),
            remedy: Some("kj ledger allow 01a05d19-0000-7000-8000-000000000000".to_string()),
        };
        let rendered = pending.to_string();
        assert_eq!(rendered.matches("01a05d19-0000-7000-8000-000000000000").count(), 2,
            "the id belongs once in the ask clause and once in the remedy: {rendered}");
        assert_eq!(pending.ask_id(), Some("01a05d19-0000-7000-8000-000000000000"));

        let unavailable = Refusal {
            kind: RefusalKind::GateUnavailable,
            reason: "gate for lfm2d-advisory had nothing to answer it".to_string(),
            subject: "lfm2d-advisory".to_string(),
            ask: None,
            remedy: None,
        };
        let rendered = unavailable.to_string();
        assert!(!rendered.contains("ask "), "no ask exists to name: {rendered}");
        assert_eq!(unavailable.ask_id(), None);
    }

    /// An open ask is one a poll should keep watching; every terminal state
    /// stops it. `Claimed` is the trap — it is mid-decision, not decided.
    #[test]
    fn only_pending_and_claimed_are_open() {
        assert!(AskStatus::Pending.is_open());
        assert!(AskStatus::Claimed.is_open());
        for s in [
            AskStatus::Allowed,
            AskStatus::Denied,
            AskStatus::Expired,
            AskStatus::Abandoned,
        ] {
            assert!(!s.is_open(), "{s} would poll forever");
        }
    }
}

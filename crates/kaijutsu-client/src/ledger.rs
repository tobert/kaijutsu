//! The approval ledger as a client sees it: the open asks, kept current by
//! the kernel's push, and the typed calls that read and answer it.
//!
//! The ledger is kernel-wide, so nothing here takes a context. On every
//! (re)connect the actor lists the queue, subscribes from the generation
//! that listing was read at, and applies each push to one [`LedgerState`].
//! A surface watches that state ([`crate::ActorHandle::ledger`]) and diffs
//! it against what it shows: an ask that appears is offered, and an ask
//! that leaves was answered, cancelled, or expired, from any surface.
//!
//! Answers go through [`crate::ActorHandle::decide_ask`]. The ledger is the
//! record; an answer authors no block in any transcript. Two players may
//! answer the same ask; the ledger's claim makes exactly one win, and the
//! other gets [`kaijutsu_types::AskAnswerFailureKind::AlreadyAnswered`].

use std::collections::BTreeMap;

use kaijutsu_types::AskSummary;

/// One push from the kernel: every ask that changed after the subscriber's
/// cursor, each in its current state, and the generation they bring it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerPush {
    pub generation: i64,
    pub asks: Vec<AskSummary>,
}

/// The open asks, keyed by request id, and the ledger generation they
/// reflect. `synced` is false until the first listing after a connect has
/// landed; until then `open` may be empty because nothing was read yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerState {
    pub generation: i64,
    pub open: BTreeMap<String, AskSummary>,
    pub synced: bool,
}

impl LedgerState {
    /// The state a listing of the queue describes.
    pub fn from_listing(generation: i64, asks: Vec<AskSummary>) -> Self {
        let open = asks.into_iter().filter(|ask| ask.status.is_open())
            .map(|ask| (ask.request_id.clone(), ask)).collect();
        Self { generation, open, synced: true }
    }

    /// Fold one push in: an open ask is added or replaced, a closed one is
    /// removed. A push older than the state is ignored whole.
    pub fn apply(&mut self, push: &LedgerPush) {
        if push.generation < self.generation {
            return;
        }
        for ask in &push.asks {
            if ask.status.is_open() {
                self.open.insert(ask.request_id.clone(), ask.clone());
            } else {
                self.open.remove(&ask.request_id);
            }
        }
        self.generation = push.generation;
    }
}

#[cfg(test)]
mod tests {
    use kaijutsu_types::{AskOrigin, AskStatus};

    use super::*;

    fn ask(id: &str, status: AskStatus) -> AskSummary {
        AskSummary {
            request_id: id.into(), status, origin: AskOrigin::ShellGate, context_id: None,
            description: String::new(), statements: Vec::new(), requester: None, performer: None,
            reviewer: None, created_at_ms: 0, decided_at_ms: None,
        }
    }

    #[test]
    fn a_push_adds_open_asks_and_removes_closed_ones() {
        let mut state = LedgerState::from_listing(5, vec![ask("a", AskStatus::Pending)]);
        state.apply(&LedgerPush { generation: 7, asks: vec![ask("a", AskStatus::Allowed), ask("b", AskStatus::Pending)] });
        assert_eq!(state.open.keys().collect::<Vec<_>>(), vec!["b"]);
        assert_eq!(state.generation, 7);
    }

    #[test]
    fn a_stale_push_changes_nothing() {
        let mut state = LedgerState::from_listing(9, vec![ask("a", AskStatus::Pending)]);
        state.apply(&LedgerPush { generation: 8, asks: vec![ask("a", AskStatus::Denied)] });
        assert!(state.open.contains_key("a"));
        assert_eq!(state.generation, 9);
    }
}

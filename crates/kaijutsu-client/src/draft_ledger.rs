//! The draft ledger: when a compose surface may take the kernel's copy of
//! its draft (`docs/tui.md`, "Compose").
//!
//! A client types into its own buffer and sends each edit through the draft
//! outbox ([`crate::draft_outbox`]). The kernel's copy comes back two ways:
//! as acks for our own edits ([`crate::draft_outbox::Landed`]), and as the
//! draft block on the change feed, which also carries a sibling's typing.
//! The two travel separately, so a mirror can arrive that predates our own
//! keystrokes. Applying it would delete them. The ledger counts our edits
//! and their acks and says when a mirror is safe to apply; the client's own
//! editor does the applying.
//!
//! Pure: no editor, no RPC, no clock.

/// Our edits to one draft and the acks that came back for them.
#[derive(Debug, Default)]
pub struct DraftLedger {
    /// The highest context version our own edits have acknowledged. A mirror
    /// older than this does not yet carry our keystrokes.
    acked: u64,
    /// Which draft this is. [`Self::restart`] starts a new one, so an edit
    /// sent for the last draft cannot settle this one.
    generation: u64,
    /// Our edits sent for this draft and not yet landed.
    in_flight: usize,
    /// The newest mirror refused while edits were in flight, as text and
    /// context version. Checked again when the last one lands.
    held: Option<(String, u64)>,
}

impl DraftLedger {
    /// Start a new draft: a submit, a context switch, or a reload. Acks for
    /// the old draft no longer count.
    pub fn restart(&mut self) {
        self.acked = 0;
        self.generation += 1;
        self.in_flight = 0;
        self.held = None;
    }

    /// The current draft generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Edits sent for this draft and not yet landed.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Count one edit as sent for this draft. Returns the generation to send
    /// with it and hand back to [`Self::landed`].
    pub fn sent(&mut self) -> u64 {
        self.in_flight += 1;
        self.generation
    }

    /// One of our edits came back: `Some(version)` when the kernel
    /// acknowledged it, `None` when it failed. An edit for an earlier draft
    /// is ignored. When the last edit in flight lands, a mirror held for it
    /// is offered again; returns its text when the caller should apply it.
    ///
    /// # Panics
    ///
    /// When more edits land for this draft than were sent: the counting is
    /// broken, and every later decision would be wrong.
    pub fn landed(&mut self, generation: u64, version: Option<u64>) -> Option<String> {
        if generation != self.generation {
            return None;
        }
        self.in_flight = self.in_flight.checked_sub(1).expect("an edit landed that was never sent");
        if let Some(version) = version {
            self.acked = self.acked.max(version);
        }
        if self.in_flight > 0 {
            return None;
        }
        let (text, version) = self.held.take()?;
        self.offer(&text, version).then_some(text)
    }

    /// A mirror of the draft at context `version`. Returns whether the
    /// caller should apply it now. While our edits are in flight no mirror
    /// can be known to carry them, so the newest one is held for
    /// [`Self::landed`]. A mirror older than our last ack is refused.
    pub fn offer(&mut self, text: &str, version: u64) -> bool {
        if self.in_flight > 0 {
            if self.held.as_ref().is_none_or(|(_, held)| version >= *held) {
                self.held = Some((text.to_string(), version));
            }
            return false;
        }
        version >= self.acked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mirror older than our last ack is the echo race: refused.
    #[test]
    fn a_mirror_behind_our_ack_is_refused() {
        let mut ledger = DraftLedger::default();
        let generation = ledger.sent();
        assert_eq!(ledger.landed(generation, Some(9)), None);
        assert!(!ledger.offer("a", 8));
        assert!(ledger.offer("ab", 9));
    }

    /// A mirror that arrives while an edit is in flight is held, and offered
    /// again once the last edit lands.
    #[test]
    fn a_mirror_held_for_our_acks_comes_back_when_they_land() {
        let mut ledger = DraftLedger::default();
        let generation = ledger.sent();
        ledger.sent();
        assert!(!ledger.offer("Xab", 7), "held while two edits are in flight");
        assert_eq!(ledger.landed(generation, Some(6)), None, "one edit is still in flight");
        assert_eq!(ledger.landed(generation, Some(7)).as_deref(), Some("Xab"));
        assert_eq!(ledger.in_flight(), 0);
    }

    /// A held mirror older than the last ack is dropped, not applied late.
    #[test]
    fn a_held_mirror_older_than_the_last_ack_is_dropped() {
        let mut ledger = DraftLedger::default();
        let generation = ledger.sent();
        assert!(!ledger.offer("a", 6));
        assert_eq!(ledger.landed(generation, Some(7)), None);
    }

    /// An ack for the last draft settles nothing on this one.
    #[test]
    fn an_ack_for_an_earlier_draft_is_ignored() {
        let mut ledger = DraftLedger::default();
        let old = ledger.sent();
        ledger.restart();
        let generation = ledger.sent();
        assert_ne!(old, generation);
        assert_eq!(ledger.landed(old, Some(40)), None);
        assert_eq!(ledger.in_flight(), 1, "the old ack did not count against this draft");
        assert_eq!(ledger.landed(generation, Some(25)), None);
        assert!(ledger.offer("n sibling", 30), "the old ack did not raise the floor to 40");
    }

    /// A failed edit is no longer in flight and raises no floor.
    #[test]
    fn a_failed_edit_stops_holding_the_draft() {
        let mut ledger = DraftLedger::default();
        let generation = ledger.sent();
        assert_eq!(ledger.landed(generation, None), None);
        assert!(ledger.offer("a sibling typed this", 3));
    }
}

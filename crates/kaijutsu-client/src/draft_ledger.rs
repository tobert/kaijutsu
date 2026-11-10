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
//! editor does the applying. [`draft_of`] picks which block in a mirror is
//! the draft at all.
//!
//! Pure: no editor, no RPC, no clock.

use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId, Status};

/// `principal`'s draft among a mirror's `blocks`: their `Status::Draft`
/// block, skipping any draft a submit already took.
///
/// `submitted` is the block the client's last submit in this context
/// answered with: the promoted draft itself for a prompt, the command block
/// for a draft run as shell. The kernel numbers each principal's blocks in a
/// context in the order it makes them and keeps one draft per (context,
/// principal). The draft a submit takes was made before the block the
/// submit answers with, and the next draft is made after it, so a draft
/// numbered at or below `submitted` is one already sent, however late its
/// echo arrives, and one numbered above it is new.
pub fn draft_of(blocks: &[BlockSnapshot], principal: PrincipalId, submitted: Option<BlockId>) -> Option<&BlockSnapshot> {
    blocks.iter().find(|b| {
        b.status == Status::Draft
            && b.id.principal_id == principal
            && !submitted.is_some_and(|sent| taken_by(b.id, sent))
    })
}

/// Whether the submit that answered with `sent` took `draft`: the same
/// context and principal, numbered no later.
fn taken_by(draft: BlockId, sent: BlockId) -> bool {
    draft.context_id == sent.context_id && draft.principal_id == sent.principal_id && draft.seq <= sent.seq
}

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

    fn draft(context: kaijutsu_types::ContextId, principal: PrincipalId, seq: u64, text: &str) -> BlockSnapshot {
        let mut block = BlockSnapshot::text(BlockId::new(context, principal, seq), None, kaijutsu_types::Role::User, text);
        block.status = Status::Draft;
        block
    }

    /// A prompt submit answers with the draft it promoted; a shell run
    /// answers with its command block, numbered after the draft. Either way
    /// the submitted draft is skipped and the next one is read.
    #[test]
    fn a_draft_a_submit_took_is_skipped_and_the_next_is_read() {
        let (context, me) = (kaijutsu_types::ContextId::new(), PrincipalId::new());
        let sent = draft(context, me, 4, "sent");
        let next = draft(context, me, 7, "next");
        let promoted = Some(sent.id);
        let command = Some(BlockId::new(context, me, 6));
        for submitted in [promoted, command] {
            assert_eq!(draft_of(std::slice::from_ref(&sent), me, submitted), None, "{submitted:?}");
            assert_eq!(draft_of(&[sent.clone(), next.clone()], me, submitted).map(|b| b.id), Some(next.id));
        }
        assert_eq!(draft_of(std::slice::from_ref(&sent), me, None).map(|b| b.id), Some(sent.id), "no submit yet");
    }

    /// The numbering is per context and per principal: a submit in another
    /// context, or another principal's draft, says nothing about this one.
    #[test]
    fn a_submit_only_skips_drafts_in_its_own_lane() {
        let (here, elsewhere, me) = (kaijutsu_types::ContextId::new(), kaijutsu_types::ContextId::new(), PrincipalId::new());
        let mine = draft(here, me, 2, "mine");
        assert_eq!(draft_of(std::slice::from_ref(&mine), me, Some(BlockId::new(elsewhere, me, 9))).map(|b| b.id), Some(mine.id));
        let theirs = draft(here, PrincipalId::new(), 1, "theirs");
        assert_eq!(draft_of(std::slice::from_ref(&theirs), me, None), None, "another principal's draft is never ours");
    }
}

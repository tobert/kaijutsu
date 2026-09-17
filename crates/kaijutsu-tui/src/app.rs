//! Client state: which contexts are watched and how each is shown, the
//! rank, the pending-ask count, and what the status line says.
//!
//! Pure. Nothing here opens a connection, reads a clock, or draws a cell —
//! every value a decision needs is handed in, so the whole module is
//! unit-testable without a kernel and without a terminal.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use kaijutsu_client::{
    ConnectionStatus, ContextChange, ContextInfo, ContextMirror, RankedSeat, ranked_seats,
};
use kaijutsu_types::{
    BlockId, BlockKind, BlockSnapshot, ContextId, InputEdge, PrincipalId, Role, Status,
};
use kaijutsu_viz::layout::Band;

use crate::compose::Compose;
use crate::present::{BlockView, Palette, WrapCache, collapses_by_default};
use crate::status::{CacheHealth, SeatCell, StatusModel, cache_health};

/// The double-tap window, shared by `Ctrl+C Ctrl+C` and the prefix's
/// `Ctrl+A Ctrl+A` (`docs/input.md`, the app's `Esc Esc` pattern).
pub const DOUBLE_TAP: Duration = Duration::from_millis(500);

/// Where one context's transcript view sits over its blocks.
///
/// One per context, held on its [`ContextView`], so switching seats switches
/// buffers: a context left scrolled is still scrolled on return and a context
/// at its tail is still at its tail (`docs/tui.md`, "The buffer").
///
/// Following is the live tail: new blocks and streaming text move the view.
/// Scrolling stops it, and scrolling *is* copy mode — off the tail the
/// transcript owns vi motions, `v`, `y` and a search, and every other key
/// snaps it back (`docs/tui.md`, "Scrolling is copy mode"). Off the tail the
/// place is kept by block and line rather than by row, so a block streaming
/// above or below the reader does not move what they are reading.
#[derive(Default)]
pub struct TranscriptView {
    /// Where the reader is, while they are off the tail.
    pub scrolled: Option<crate::copy::Scrolled>,
}

impl TranscriptView {
    /// Whether the view is on the live tail, where new blocks move it.
    pub fn follow(&self) -> bool {
        self.scrolled.is_none()
    }

    /// Back to the live tail — `q`, `Esc`, `G`, a yank, or any key the
    /// scrolled view does not claim. Not a context switch: the place is this
    /// context's own and waits here for the reader's return.
    pub fn snap(&mut self) {
        self.scrolled = None;
    }
}

/// One watched context: its mirror, and how this client is showing it.
pub struct ContextView {
    pub mirror: ContextMirror,
    /// Per-block collapse, seeded from [`collapses_by_default`] on first
    /// arrival and then carried forward, so a later expand is not wiped by
    /// the next redraw.
    pub collapsed: HashMap<BlockId, bool>,
    /// Activity since this context was last on screen — screen's `@` flag.
    pub activity: bool,
    /// The block that ended the last transcript frame, and how many of its
    /// characters that frame drew — the inner `None` when the window cropped
    /// its tail (`render::transcript_window`). It is the player's edge
    /// on submit (`docs/prompts.md`, "The submit verb"); the outer `None`
    /// means this context has shown nothing at all.
    pub shown_tail: Option<(BlockId, Option<u64>)>,
    /// Where this context's transcript is: its live tail, or the place the
    /// reader scrolled to. It is the context's, not the screen's, so it
    /// survives a switch away and back for as long as the context stays
    /// resident (`App::hot_set`).
    pub transcript: TranscriptView,
}

impl ContextView {
    pub fn new(mirror: ContextMirror) -> Self {
        let mut view = Self {
            mirror,
            collapsed: HashMap::new(),
            activity: false,
            shown_tail: None,
            transcript: TranscriptView::default(),
        };
        view.seed_collapse();
        view
    }

    /// Apply the first-arrival collapse default to every block that has not
    /// been seen before.
    pub fn seed_collapse(&mut self) {
        for block in self.mirror.blocks() {
            self.collapsed
                .entry(block.id)
                .or_insert_with(|| block.collapsed || collapses_by_default(block.kind));
        }
    }

    /// Whether a block renders collapsed right now.
    pub fn is_collapsed(&self, block: &BlockSnapshot) -> bool {
        self.collapsed
            .get(&block.id)
            .copied()
            .unwrap_or_else(|| block.collapsed || collapses_by_default(block.kind))
    }

    /// The player's edge of context, as of the last frame this view drew:
    /// the newest block it had shown, and how much of it. `None` when this
    /// context has shown nothing at all, so the caller sends no edge rather
    /// than guess (`docs/prompts.md`, "The submit verb").
    pub fn edge(&self) -> Option<InputEdge> {
        self.shown_tail.map(|(block, shown)| InputEdge { block, shown })
    }
}

/// The whole client's state.
pub struct App {
    /// The local principal's display name, for the role divider.
    pub identity: String,
    /// The last `list_contexts` answer.
    pub contexts: Vec<ContextInfo>,
    /// The rank, recomputed from `contexts` — the same seats the app and an
    /// ACP session list, in the same order.
    pub seats: Vec<RankedSeat>,
    pub current: Option<ContextId>,
    /// Where `Ctrl+A Ctrl+A` goes.
    pub previous: Option<ContextId>,
    pub views: HashMap<ContextId, ContextView>,
    pub connection: Option<ConnectionStatus>,
    /// Pending asks across every context (`asks.rs` answers them).
    pub pending_asks: usize,
    /// The compose surface — a modalkit `VimMachine` over the context's
    /// kernel-owned draft block, and the `:` bar (`compose.rs`).
    pub compose: Compose,
    pub palette: Palette,
    /// The terminal's row count, from the last size the event loop read.
    /// Sizes what may grow with the screen (the compose cap).
    pub screen_rows: u16,
    pub wrap: WrapCache,
    pub quit: bool,
    /// This client's own principal, from `whoami`. It selects which draft
    /// block in the mirror is ours — there is at most one per
    /// (context, principal).
    pub principal: Option<PrincipalId>,
    notice: Option<String>,
    /// Contexts this client currently believes have a turn running: set on
    /// this client's own submit (`compose_key`) and on
    /// `ServerEvent::TurnStarted`, cleared on `TurnCompleted`/`TurnFailed`
    /// for that context, and forgotten wholesale when events are known lost
    /// (`forget_turn_liveness`). **Partial**: an interactive submit from
    /// another client or peer names no start this client observes unless it
    /// is already watching that context (`docs/tui.md`, "Ctrl+C reclaimed")
    /// — it is what `Ctrl+C`'s ladder and `:q`'s warning read, not an
    /// authoritative turn registry.
    pub turns_running: HashSet<ContextId>,
    /// Contexts whose running turn has shown a `Thinking` block — the
    /// thinking pane's latch (`docs/tui.md`, "The thinking pane"). Set by
    /// [`Self::observe_thinking`] as the feed lands, cleared with the turn.
    thinking_turns: HashSet<ContextId>,
    /// The draft block this client last submitted. A submit promotes the
    /// draft in place — the kernel flips its status to `Done` rather than
    /// deleting it — and a keystroke echo still on the feed can arrive before
    /// that flip, still `Draft` and still holding the sent text. Read as a
    /// draft again it would refill the compose line the reset just cleared,
    /// so [`Self::current_draft`] skips this one block by id.
    submitted_draft: Option<BlockId>,
    /// The last yank from the scrolled transcript, for `Ctrl+A ]` — tmux's
    /// paste buffer. The
    /// tui's own, so it never needs aligning with vim's registers or the OS
    /// clipboard (the yank also goes to the clipboard over OSC 52, but that
    /// is a one-way emission the tui cannot read back).
    pub paste_buffer: Option<String>,
    /// What the owned screen is showing. A full-screen surface — the
    /// editor, the diff viewer — takes the whole screen
    /// (`docs/tui.md`, "The owned screen"), and the key path early-returns
    /// on it, which is what makes
    /// the editor the sanctioned raw key reader.
    pub screen: crate::editor::ScreenMode,
    /// Which context each pending ask belongs to
    /// (`kaijutsu_client::AskInfo::context_id`), kept current by
    /// [`Self::note_ask`]/[`Self::forget_asks_not_in`] from the same poll
    /// loop that maintains `seen_asks` (`run.rs`) — what
    /// [`Self::status_model`] reads to mark a seat `!` without re-deriving
    /// it from the ledger on every frame.
    pub ask_owners: HashMap<String, ContextId>,
    /// The ask card showing in the live region, when one is — never more
    /// than one at a time (`docs/tui.md`, "Asks": it draws as an overlay, it
    /// is not a queue of modals).
    pub ask_card: Option<crate::asks::AskCardState>,
    /// The ledger view (`Ctrl+A l`), when open.
    pub ledger_view: Option<crate::asks::LedgerViewState>,
    /// The `kj` command catalog (`get_kj_command_catalog`), fetched once at
    /// connect and cached for `:kj ` completion (`completion.rs`).
    pub kj_catalog: Vec<kaijutsu_client::rpc::KjCommandInfo>,
    /// The `:kj ` completion popup, when `Tab` has one open.
    pub completion: Option<crate::completion::KjCompletion>,
    /// The last `listTracks` answer, refreshed on [`crate::run`]'s
    /// [`REFRESH`](crate::run) cadence — feeds the picker's TRACKS section
    /// and the status line's `bar.beat` figure (`docs/tui.md`, "TRACKS +
    /// beat"). Kept as [`crate::picker::TrackRow`] so both readers agree
    /// on `bar`/`beat`.
    pub tracks: Vec<crate::picker::TrackRow>,
    /// Per-track beat phasors, fed from `ServerEvent::BeatSync` on the
    /// kernel-wide event stream (`docs/tui.md`, "Timing to music").
    pub beats: kaijutsu_present::beats::WellBeats,
    /// The picker's single-line tail buffer, fed from the same kernel-wide
    /// stream, independent of which context is watched.
    pub tails: crate::picker::PickerTails,
    /// The picker, when `Ctrl+A "` has it open — `render::overlay_lines`
    /// draws it and the key path routes to it.
    pub picker: Option<crate::picker::PickerModel>,
    /// A kj verb this client ran may have changed the roster: a placement
    /// from the picker or any `:kj` line. The loop takes it and starts a
    /// refresh round now.
    pub roster_changed: bool,
    /// Whether the terminal reports itself focused (DECSET 1004,
    /// `Event::FocusGained`/`FocusLost`). `true` until told otherwise: a
    /// terminal that never reports focus never sends `FocusLost`, and a
    /// client that assumed the worst would stop animating everywhere it is
    /// not supported (`docs/tui.md`, "What owning the screen lets us use").
    pub focused: bool,
    /// The playing track's beat envelope, sampled against `Instant::now()`
    /// once per redraw tick in [`crate::run`] — the only place this module
    /// reads a live clock. [`Self::track_figure`] projects it; nothing here
    /// samples `beats` directly.
    pub track_pulse: bool,
}

impl App {
    pub fn new(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            contexts: Vec::new(),
            seats: Vec::new(),
            current: None,
            previous: None,
            views: HashMap::new(),
            connection: None,
            pending_asks: 0,
            compose: Compose::new(),
            palette: Palette::builtin(),
            screen_rows: 24,
            wrap: WrapCache::new(),
            quit: false,
            principal: None,
            notice: None,
            turns_running: HashSet::new(),
            thinking_turns: HashSet::new(),
            submitted_draft: None,
            paste_buffer: None,
            screen: crate::editor::ScreenMode::Conversation,
            ask_owners: HashMap::new(),
            ask_card: None,
            ledger_view: None,
            kj_catalog: Vec::new(),
            completion: None,
            tracks: Vec::new(),
            beats: kaijutsu_present::beats::WellBeats::default(),
            tails: crate::picker::PickerTails::new(),
            picker: None,
            track_pulse: false,
            focused: true,
            roster_changed: false,
        }
    }

    /// Take a fresh `list_contexts` answer and recompute the rank.
    pub fn set_contexts(&mut self, contexts: Vec<ContextInfo>) {
        self.seats = ranked_seats(&contexts);
        self.contexts = contexts;
    }

    /// The contexts carrying the `@` activity flag — the set the picker's
    /// rows and the status line's rank both read.
    pub fn activity_set(&self) -> std::collections::HashSet<ContextId> {
        self.views.iter().filter(|(_, v)| v.activity).map(|(id, _)| *id).collect()
    }

    /// Open the picker over what the app holds now.
    pub fn open_picker(&mut self, now_millis: u64) {
        self.picker = Some(crate::picker::PickerModel::build(
            &self.contexts,
            &self.tracks,
            &self.activity_set(),
            &self.tails,
            now_millis,
        ));
    }

    /// Rebuild an open picker over what the app holds now, keeping its
    /// cursor and filter (`PickerModel::refreshed`). No-op when it is
    /// closed. The refresh calls this after every round so a promote,
    /// a demote, or an archive from any seat shows without reopening.
    pub fn refresh_picker(&mut self, now_millis: u64) {
        if let Some(picker) = self.picker.as_ref() {
            self.picker = Some(picker.refreshed(&self.contexts, &self.tracks, &self.activity_set(), &self.tails, now_millis));
        }
    }

    pub fn info(&self, id: ContextId) -> Option<&ContextInfo> {
        self.contexts.iter().find(|c| c.id == id)
    }

    pub fn current_info(&self) -> Option<&ContextInfo> {
        self.current.and_then(|id| self.info(id))
    }

    pub fn current_view(&self) -> Option<&ContextView> {
        self.current.and_then(|id| self.views.get(&id))
    }

    /// What a status-line notice calls a context: its label, or its short id
    /// when it has none — the same name the status line's seat cells use.
    pub fn label_for(&self, id: ContextId) -> String {
        self.info(id)
            .map(|c| c.label.clone())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| id.short())
    }

    /// The context sitting on seat `digit`, or `None` when the rank is
    /// shorter than that.
    pub fn seat_context(&self, digit: usize) -> Option<ContextId> {
        self.seats.get(digit).map(|s| s.context_id)
    }

    /// The seat one step from the context on screen, wrapping at either end
    /// — `Ctrl+A n` (`+1`) and `Ctrl+A p` (`-1`), screen's next/previous
    /// window. A context not on the rank (nothing on screen, or an
    /// unranked one) steps from the top for `n` and the bottom for `p`.
    /// `None` only when there are no seats at all.
    pub fn seat_neighbor(&self, step: isize) -> Option<ContextId> {
        let len = self.seats.len();
        if len == 0 {
            return None;
        }
        let here = self
            .current
            .and_then(|id| self.seats.iter().position(|s| s.context_id == id));
        let next = match here {
            Some(i) => (i as isize + step).rem_euclid(len as isize) as usize,
            None if step > 0 => 0,
            None => len - 1,
        };
        self.seat_context(next)
    }

    /// Switch to a context, remembering where we came from. Switching to the
    /// context already on screen is a no-op, so `Ctrl+A Ctrl+A` never
    /// collapses onto itself.
    ///
    /// The transcript is not touched: each context carries its own
    /// ([`ContextView::transcript`]), so a context left scrolled comes back
    /// scrolled and one at its tail comes back at its tail.
    pub fn switch_to(&mut self, id: ContextId) {
        if self.current == Some(id) {
            return;
        }
        self.previous = self.current;
        self.current = Some(id);
        if let Some(view) = self.views.get_mut(&id) {
            view.activity = false;
        }
        // The card is the current context's ask; leaving that context sets
        // it aside. The ask stays pending and the refresh raises the card
        // again on return.
        self.ask_card = None;
    }

    /// The transcript view of the context on screen. `None` when nothing is
    /// on screen or its feed ended — a context with no [`ContextView`]
    /// renders nothing at all, so there is no transcript to place a reader
    /// in.
    pub fn transcript(&self) -> Option<&TranscriptView> {
        self.current.and_then(|id| self.views.get(&id)).map(|v| &v.transcript)
    }

    pub fn transcript_mut(&mut self) -> Option<&mut TranscriptView> {
        let id = self.current?;
        self.views.get_mut(&id).map(|v| &mut v.transcript)
    }

    /// Where the reader is in the transcript on screen, while they are off
    /// its live tail.
    pub fn scrolled(&self) -> Option<&crate::copy::Scrolled> {
        self.transcript().and_then(|t| t.scrolled.as_ref())
    }

    pub fn scrolled_mut(&mut self) -> Option<&mut crate::copy::Scrolled> {
        self.transcript_mut().and_then(|t| t.scrolled.as_mut())
    }

    /// Whether the transcript on screen is on its live tail. A context with
    /// no view draws nothing, which follows the tail trivially.
    pub fn following(&self) -> bool {
        self.transcript().is_none_or(TranscriptView::follow)
    }

    /// Take the transcript on screen off its live tail. `false` when there is
    /// no transcript to scroll, so the caller says so rather than dropping
    /// the reader's place on the floor.
    pub fn set_scrolled(&mut self, scrolled: crate::copy::Scrolled) -> bool {
        match self.transcript_mut() {
            Some(view) => {
                view.scrolled = Some(scrolled);
                true
            }
            None => false,
        }
    }

    /// Return the transcript on screen to its live tail — `q`, `Esc`, `G`, a
    /// yank, or any key the scrolled view does not claim.
    pub fn snap_transcript(&mut self) {
        if let Some(view) = self.transcript_mut() {
            view.snap();
        }
    }

    /// The hot set: the contexts that stay resident — watched, hydrated, and
    /// holding their wrapped lines — so that reaching one is a redraw rather
    /// than a round trip. It is the context on screen, the one `Ctrl+A
    /// Ctrl+A` goes back to, and every seat on the ACTIVE ring
    /// (`docs/tui.md`, "The buffer").
    ///
    /// Everything else is hydrated on switch and released once it leaves
    /// both the ring and the screen. The current and previous contexts are
    /// members by construction, so a release can never take the transcript
    /// out from under the reader or break `Ctrl+A Ctrl+A`.
    pub fn hot_set(&self) -> HashSet<ContextId> {
        let mut hot: HashSet<ContextId> = self
            .seats
            .iter()
            .filter(|seat| seat.band == Band::Active)
            .map(|seat| seat.context_id)
            .collect();
        hot.extend(self.current);
        hot.extend(self.previous);
        hot
    }

    /// Hot contexts this client is not watching yet, current first and then
    /// in rank order — what the caller hydrates through the same
    /// `watch_context` path a switch takes.
    pub fn unwatched_hot(&self) -> Vec<ContextId> {
        let hot = self.hot_set();
        let mut out: Vec<ContextId> = Vec::new();
        let ordered = self
            .current
            .into_iter()
            .chain(self.previous)
            .chain(self.seats.iter().map(|seat| seat.context_id));
        for id in ordered {
            if hot.contains(&id) && !self.views.contains_key(&id) && !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// The watched contexts that have left the hot set — what a reconcile
    /// releases. Asking and releasing are separate so the caller can stop a
    /// context's feed before dropping the view it delivers into
    /// (`run::release_cold`).
    pub fn cold_contexts(&self) -> Vec<ContextId> {
        let hot = self.hot_set();
        self.views.keys().copied().filter(|id| !hot.contains(id)).collect()
    }

    /// Drop one context's view — the mirror, the collapse map and the
    /// transcript's own place — and its wrapped lines. The feed that
    /// delivers into it belongs to the caller that started it
    /// (`run::Feeds`), and is stopped first.
    pub fn release(&mut self, id: ContextId) {
        self.views.remove(&id);
        self.wrap.forget_context(id);
    }

    /// Who a block's divider names: the local user for their own text, the
    /// context's model for a reply, otherwise the role.
    pub fn speaker_for(&self, block: &BlockSnapshot, info: Option<&ContextInfo>) -> String {
        match block.role {
            Role::User => self.identity.clone(),
            Role::Model => info
                .and_then(|c| c.cast_label.clone())
                .or_else(|| info.map(|c| model_leaf(&c.model).to_string()))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "model".to_string()),
            Role::System => "system".to_string(),
            Role::Tool => block.tool_name.clone().unwrap_or_else(|| "tool".to_string()),
            Role::Asset => "asset".to_string(),
        }
    }

    /// The view a block renders under. `stamp` is the block's wallclock,
    /// formatted at the edge (see [`crate::render::wallclock`]).
    pub fn block_view<'a>(
        &'a self,
        block: &'a BlockSnapshot,
        speaker: &'a str,
        stamp: &'a str,
        show_divider: bool,
    ) -> BlockView<'a> {
        let ctx = block.id.context_id;
        let (tool, arg) = tool_header(block);
        BlockView {
            speaker,
            context_type: self
                .info(ctx)
                .map(|c| c.context_type.as_str())
                .unwrap_or("default"),
            stamp,
            show_divider,
            tool,
            arg,
            lineage: self.lineage_for(block),
            collapsed: self
                .views
                .get(&ctx)
                .map(|v| v.is_collapsed(block))
                .unwrap_or_else(|| block.collapsed || collapses_by_default(block.kind)),
            local_ctx: Some(ctx),
        }
    }

    /// The block's parent and grandparent from its context's mirror — the
    /// two hops an error's provenance line walks (`present::BlockView::
    /// lineage`). Stops at the first hop the mirror does not hold.
    pub fn lineage_for(&self, block: &BlockSnapshot) -> Vec<BlockSnapshot> {
        let Some(view) = self.views.get(&block.id.context_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut next = block.parent_id;
        while let Some(id) = next
            && out.len() < 2
            && let Some(parent) = view.mirror.block(&id)
        {
            next = parent.parent_id;
            out.push(parent.clone());
        }
        out
    }

    /// Post a status-line notice. It stands until something replaces it or
    /// [`Self::clear_notice`] runs.
    pub fn note(&mut self, message: impl Into<String>) {
        self.notice = Some(message.into());
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// Mark `context_id` as having a turn running. Returns whether the set
    /// changed, so a caller only redraws when it must.
    pub fn mark_turn_running(&mut self, context_id: ContextId) -> bool {
        self.turns_running.insert(context_id)
    }

    /// Mark `context_id`'s turn as ended. Returns whether the set changed.
    /// The thinking pane's latch ends with the turn.
    pub fn mark_turn_ended(&mut self, context_id: ContextId) -> bool {
        self.thinking_turns.remove(&context_id);
        self.turns_running.remove(&context_id)
    }

    /// Latch the thinking pane for `context_id` when its turn is running and
    /// one of the blocks `touched` by this delivery is `Thinking` — a block
    /// that completed inside one delivery counts, so a fast model's
    /// reasoning opens the pane as surely as a slow one's. Reasoning left in
    /// the mirror by an earlier turn does not: the pane is the turn's.
    /// Returns whether it latched now.
    pub fn observe_thinking(&mut self, context_id: ContextId, touched: &[BlockId]) -> bool {
        if !self.turn_running(context_id) || self.thinking_turns.contains(&context_id) {
            return false;
        }
        let seen = self.views.get(&context_id).is_some_and(|view| {
            view.mirror
                .blocks()
                .iter()
                .any(|b| b.kind == BlockKind::Thinking && touched.contains(&b.id))
        });
        if seen {
            self.thinking_turns.insert(context_id);
        }
        seen
    }

    /// Whether the thinking pane is latched open for `context_id`: its
    /// turn is running and has shown reasoning.
    pub fn thinking_pane_latched(&self, context_id: ContextId) -> bool {
        self.turn_running(context_id) && self.thinking_turns.contains(&context_id)
    }

    /// Forget every turn this client believed was running. The event stream
    /// is the only thing that clears the set, so once events are known to
    /// be lost (a broadcast lag, a dropped connection) a flag left set would
    /// never clear and `:q` would refuse forever. Returns whether anything
    /// was forgotten.
    pub fn forget_turn_liveness(&mut self) -> bool {
        let had = !self.turns_running.is_empty();
        self.turns_running.clear();
        self.thinking_turns.clear();
        had
    }

    /// Whether this client believes `context_id` has a turn running right
    /// now — the ladder's own gate for whether a fresh `Ctrl+C` press starts
    /// interrupting or just says there is nothing to interrupt.
    pub fn turn_running(&self, context_id: ContextId) -> bool {
        self.turns_running.contains(&context_id)
    }

    /// Whether any context this client knows about has a turn running —
    /// `:q`'s warning ("still running") is scoped to the whole client, not
    /// just the context on screen, because quitting abandons every one of
    /// them.
    pub fn any_turn_running(&self) -> bool {
        !self.turns_running.is_empty()
    }

    /// The current context's draft block for this client's principal, and the
    /// mirror version it was read at.
    ///
    /// The draft is an ordinary `Status::Draft` block riding the same change
    /// feed as everything else, so a sibling's typing arrives here with no
    /// separate fetch. There is at most one per (context, principal), so the
    /// first match is the only one.
    pub fn current_draft(&self) -> Option<(String, u64)> {
        let context_id = self.current?;
        let principal = self.principal?;
        let view = self.views.get(&context_id)?;
        let text = view
            .mirror
            .blocks()
            .iter()
            .find(|b| {
                b.status == Status::Draft
                    && b.id.principal_id == principal
                    && Some(b.id) != self.submitted_draft
            })
            .map(|b| b.content.clone())?;
        Some((text, view.mirror.version()))
    }

    /// Record that `block_id` was just submitted, so a stale echo of it is
    /// never read back as the draft.
    pub fn mark_submitted(&mut self, block_id: BlockId) {
        self.submitted_draft = Some(block_id);
    }

    /// Cache health for the context on screen.
    pub fn cache_health(&self, now_millis: u64) -> CacheHealth {
        self.current_info()
            .map(|info| cache_health(info, now_millis))
            .unwrap_or_default()
    }

    /// Everything the status line draws.
    pub fn status_model(&self, now_millis: u64) -> StatusModel {
        let seats = self
            .seats
            .iter()
            .enumerate()
            .take(10)
            .map(|(digit, seat)| SeatCell {
                digit,
                label: self
                    .info(seat.context_id)
                    .map(|c| c.label.clone())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| seat.context_id.short()),
                current: self.current == Some(seat.context_id),
                activity: self
                    .views
                    .get(&seat.context_id)
                    .is_some_and(|v| v.activity),
                ask: self.has_pending_ask(seat.context_id),
            })
            .collect();
        let info = self.current_info();
        StatusModel {
            seats,
            mode: self.compose.mode_banner(),
            tokens: info.and_then(|c| {
                c.context_used_tokens.map(|used| crate::status::TokenFigure { used, window: c.context_window })
            }),
            cache: self.cache_health(now_millis),
            pending_asks: self.pending_asks,
            connection: self.connection.clone(),
            notice: self.notice.clone(),
            track: self.track_figure(),
        }
    }

    /// A key or a paste arrived, so this terminal has focus whatever its
    /// last report said.
    ///
    /// Focus reporting is one-sided on some terminals — a `FocusLost` with
    /// no `FocusGained` after it, or a report at startup and never again —
    /// and a client that believed a stuck `false` would never animate
    /// again. Input is the ground truth nobody can fake: it only reaches a
    /// focused window (`docs/tui.md`, "What owning the screen lets us
    /// use").
    pub fn saw_input(&mut self) {
        self.focused = true;
    }

    /// Sync a still-live block's collapse state from the feed. Collapse is
    /// kernel state (`docs/tui.md`, "Conversation": "a sibling's expand is
    /// yours too") — this is what makes `ContextChange::CollapsedChanged`
    /// actually reach [`ContextView::collapsed`] for a sibling's manual
    /// toggle; the transcript redraws from the map on the next frame.
    pub fn apply_collapse_change(&mut self, context_id: ContextId, change: &ContextChange) {
        if let ContextChange::CollapsedChanged { block_id, collapsed } = change
            && let Some(view) = self.views.get_mut(&context_id)
        {
            view.collapsed.insert(*block_id, *collapsed);
        }
    }

    /// Record that an ask is pending for `context_id` — called from the
    /// poll loop (`run.rs`) for every ask [`kaijutsu_client::poll_new_asks`]
    /// reports.
    pub fn note_ask(&mut self, request_id: String, context_id: ContextId) {
        self.ask_owners.insert(request_id, context_id);
    }

    /// Drop every tracked ask whose id is not in `still_pending` — an ask
    /// leaves `still_pending` the moment it is decided, by any answerer,
    /// through any surface, so this is how a seat's `!` clears without this
    /// client having answered it itself. Same contract as the ledger's own
    /// `seen` pruning (`kaijutsu_client::ledger`'s `diff_new`).
    pub fn forget_asks_not_pending(&mut self, still_pending: &HashSet<String>) {
        self.ask_owners.retain(|id, _| still_pending.contains(id));
    }

    /// Take the ask card down when its ask is no longer pending — answered
    /// from another surface, expired, or abandoned — and hand it back so
    /// the caller can say what became of it. `None` while no card is up or
    /// its ask is still in `still_pending`. The card's own keys never come
    /// through here: they take the card before the answer round trip.
    pub fn take_answered_card(&mut self, still_pending: &HashSet<String>) -> Option<crate::asks::AskCardState> {
        if self.ask_card.as_ref().is_some_and(|card| !still_pending.contains(&card.request_id)) {
            return self.ask_card.take();
        }
        None
    }

    /// Whether any tracked ask belongs to `context_id` — [`SeatCell::ask`]'s
    /// derivation.
    pub fn has_pending_ask(&self, context_id: ContextId) -> bool {
        self.ask_owners.values().any(|c| *c == context_id)
    }

    /// `bar.beat` + pulse for the playing track: the current context's own
    /// attached track when it is playing, else the first playing track —
    /// "the playing track" the status line names (`docs/tui.md`, "Status
    /// line"). Reads [`Self::track_pulse`] rather than the phasor directly —
    /// [`crate::run`]'s event loop is the one place that reads
    /// `Instant::now()` against `beats`, on every redraw tick, the same way
    /// it already stamps [`Self::connection`]; this stays a pure projection
    /// of state already on `self`, like every other `status_model` field.
    pub fn track_figure(&self) -> Option<crate::status::TrackFigure> {
        let playing = self.playing_track()?;
        Some(crate::status::TrackFigure { bar: playing.bar, beat: playing.beat, pulse: self.track_pulse })
    }

    /// "The playing track" — the current context's own attached track when
    /// it is playing, else the first playing track in the roster. Shared by
    /// [`Self::track_figure`] (the status line) and `run.rs`'s beat-timer
    /// re-arm, so both name the same track.
    pub fn playing_track(&self) -> Option<&crate::picker::TrackRow> {
        self.current_info()
            .and_then(|c| c.track_id.as_deref())
            .and_then(|tid| self.tracks.iter().find(|t| t.id == tid && t.playing))
            .or_else(|| self.tracks.iter().find(|t| t.playing))
    }
}

/// The block a change touches, whether it adds, moves, edits or removes it.
pub fn touched_block(change: &ContextChange) -> BlockId {
    match change {
        ContextChange::BlockInserted { block, .. } => block.id,
        ContextChange::BlockDeleted { block_id }
        | ContextChange::BlockMoved { block_id, .. }
        | ContextChange::TextAppended { block_id, .. }
        | ContextChange::TextReplaced { block_id, .. }
        | ContextChange::StatusChanged { block_id, .. }
        | ContextChange::CollapsedChanged { block_id, .. }
        | ContextChange::ExcludedChanged { block_id, .. }
        | ContextChange::MetadataChanged { block_id, .. }
        | ContextChange::OutputChanged { block_id, .. }
        | ContextChange::SpansChanged { block_id, .. } => *block_id,
    }
}

/// `"deepseek/deepseek-v4"` → `"deepseek-v4"`.
/// A tool call's header parts — its tool name and its one-line argument —
/// or `(None, None)` for any other block (`present::BlockView::tool`).
pub fn tool_header(block: &BlockSnapshot) -> (Option<&str>, Option<String>) {
    if block.kind != BlockKind::ToolCall {
        return (None, None);
    }
    let tool = block.tool_name.as_deref().filter(|t| !t.is_empty()).or(Some("call"));
    let input = block.tool_input.as_deref().unwrap_or(block.content.as_str());
    (tool, crate::inflight::one_line_arg(input))
}

fn model_leaf(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, PrincipalId, Status};

    fn ctx(label: &str) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from: None,
            provider: String::new(),
            model: "deepseek/deepseek-v4".to_string(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: "coder".to_string(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            cast_label: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
        }
    }

    fn block(context: ContextId, seq: u64, kind: BlockKind, role: Role) -> BlockSnapshot {
        let id = BlockId::new(context, PrincipalId::new(), seq);
        BlockSnapshotBuilder::new(id, kind)
            .role(role)
            .content("body")
            .build()
    }

    fn app_with_two() -> (App, ContextId, ContextId) {
        let mut a = ctx("kaijutsu");
        a.promoted_at = Some(1_000);
        let mut b = ctx("kaish");
        b.promoted_at = Some(2_000);
        let (aid, bid) = (a.id, b.id);
        let mut app = App::new("amy");
        app.set_contexts(vec![a, b]);
        (app, aid, bid)
    }

    /// Watch a context with an empty mirror, the way `run::watch_context`
    /// leaves one behind.
    fn watch(app: &mut App, id: ContextId) {
        app.views.insert(id, ContextView::new(ContextMirror::new(id)));
    }

    /// One wrapped block in the cache, belonging to `context`.
    fn wrap_one(app: &mut App, context: ContextId) {
        let blk = block(context, 1, BlockKind::Text, Role::Model);
        let view = BlockView {
            speaker: "claude",
            context_type: "coder",
            stamp: "14:02:11",
            show_divider: false,
            tool: None,
            arg: None,
            lineage: Vec::new(),
            collapsed: false,
            local_ctx: Some(context),
        };
        let palette = app.palette;
        let _ = app.wrap.lines(&blk, &view, 40, &palette);
    }

    // ────────────────────────────────────────────────────────────────────
    // The hot set (docs/tui.md, "The buffer")
    // ────────────────────────────────────────────────────────────────────

    /// The hot set is the ACTIVE ring plus the two contexts a key can reach
    /// without the rank: the one on screen and the one `Ctrl+A Ctrl+A` goes
    /// back to. A RECENT seat nobody is looking at is not in it.
    #[test]
    fn the_hot_set_is_the_active_ring_plus_the_screen_and_the_toggle() {
        let mut active_a = ctx("kaijutsu");
        active_a.promoted_at = Some(1_000);
        let mut active_b = ctx("kaish");
        active_b.promoted_at = Some(2_000);
        let seen = ctx("onscreen");
        let toggle = ctx("previous");
        let cold = ctx("recent");
        let (a, b, seen_id, toggle_id, cold_id) =
            (active_a.id, active_b.id, seen.id, toggle.id, cold.id);

        let mut app = App::new("amy");
        app.set_contexts(vec![active_a, active_b, seen, toggle, cold]);
        app.switch_to(toggle_id);
        app.switch_to(seen_id);

        assert_eq!(
            app.hot_set(),
            HashSet::from([a, b, seen_id, toggle_id]),
            "the ring, the screen and the toggle — and nothing else"
        );
        assert!(!app.hot_set().contains(&cold_id), "a RECENT seat is not resident");
    }

    /// A context that leaves the ring without being on screen is released:
    /// its view goes and its wrapped lines go with it.
    #[test]
    fn a_context_leaving_the_ring_is_released_with_its_wraps() {
        let mut active = ctx("kaijutsu");
        active.promoted_at = Some(1_000);
        let mut leaving = ctx("demoted");
        leaving.promoted_at = Some(2_000);
        let (active_id, leaving_id) = (active.id, leaving.id);
        let mut app = App::new("amy");
        app.set_contexts(vec![active.clone(), leaving.clone()]);
        watch(&mut app, active_id);
        watch(&mut app, leaving_id);
        wrap_one(&mut app, active_id);
        wrap_one(&mut app, leaving_id);
        app.switch_to(active_id);
        assert!(app.cold_contexts().is_empty(), "both are on the ring");

        // The demote lands on the next round's rank.
        leaving.promoted_at = None;
        leaving.demoted_at = Some(3_000);
        app.set_contexts(vec![active, leaving]);
        let cold = app.cold_contexts();
        assert_eq!(cold, vec![leaving_id]);
        for id in cold {
            app.release(id);
        }
        assert!(!app.views.contains_key(&leaving_id), "its view went");
        assert_eq!(app.wrap.len(), 1, "its wrapped lines went with it");
        assert!(app.views.contains_key(&active_id), "the ring's own seat stayed");
    }

    /// Releasing never touches the context on screen or the one behind it,
    /// however the rank moves — neither is ever on a seat here.
    #[test]
    fn releasing_never_drops_the_screen_or_the_toggle() {
        let seen = ctx("onscreen");
        let toggle = ctx("previous");
        let (seen_id, toggle_id) = (seen.id, toggle.id);
        let mut app = App::new("amy");
        // An empty rank: nothing is on a seat at all.
        app.set_contexts(vec![]);
        watch(&mut app, seen_id);
        watch(&mut app, toggle_id);
        app.switch_to(toggle_id);
        app.switch_to(seen_id);

        assert!(app.cold_contexts().is_empty(), "neither is cold");
        assert!(app.views.contains_key(&seen_id));
        assert!(app.views.contains_key(&toggle_id));
    }

    /// A context promoted into the ring is named for the next watch, once
    /// and not again after it is watched.
    #[test]
    fn a_context_entering_the_ring_is_named_for_a_watch() {
        let mut entering = ctx("promoted");
        entering.promoted_at = Some(1_000);
        let seen = ctx("onscreen");
        let (entering_id, seen_id) = (entering.id, seen.id);
        let mut app = App::new("amy");
        app.set_contexts(vec![entering, seen]);
        watch(&mut app, seen_id);
        app.switch_to(seen_id);

        assert_eq!(app.unwatched_hot(), vec![entering_id], "the ring's new seat");
        watch(&mut app, entering_id);
        assert!(app.unwatched_hot().is_empty(), "watched once, not again");
    }

    #[test]
    fn the_rank_seats_contexts_in_promote_order() {
        let (app, aid, bid) = app_with_two();
        assert_eq!(app.seat_context(0), Some(aid));
        assert_eq!(app.seat_context(1), Some(bid));
        assert_eq!(app.seat_context(9), None);
    }

    #[test]
    fn switching_remembers_where_we_came_from() {
        let (mut app, aid, bid) = app_with_two();
        app.switch_to(aid);
        app.switch_to(bid);
        assert_eq!(app.current, Some(bid));
        // `Ctrl+A Ctrl+A` reads `previous` and switches to it (`run.rs`'s
        // `Intent::LastContext`), so the toggle is `previous` alternating.
        assert_eq!(app.previous, Some(aid));
        app.switch_to(aid);
        assert_eq!(app.current, Some(aid));
        assert_eq!(app.previous, Some(bid));
    }

    #[test]
    fn switching_to_the_context_already_on_screen_is_a_no_op() {
        let (mut app, aid, _) = app_with_two();
        app.switch_to(aid);
        app.switch_to(aid);
        assert_eq!(app.previous, None, "the toggle target was not overwritten");
    }

    #[test]
    fn there_is_nowhere_to_go_back_to_at_startup() {
        let (mut app, aid, _) = app_with_two();
        app.switch_to(aid);
        assert_eq!(app.previous, None);
    }

    #[test]
    fn a_context_with_no_label_takes_its_short_id_in_the_rank() {
        let mut a = ctx("");
        a.promoted_at = Some(1_000);
        let id = a.id;
        let mut app = App::new("amy");
        app.set_contexts(vec![a]);
        assert_eq!(app.status_model(0).seats[0].label, id.short());
    }

    #[test]
    fn a_context_that_has_never_called_a_model_shows_the_cast_alone() {
        let mut a = ctx("fresh");
        a.model = String::new();
        a.promoted_at = Some(1_000);
        let id = a.id;
        let mut app = App::new("amy");
        app.set_contexts(vec![a]);
        app.switch_to(id);
        assert_eq!(app.status_model(0).mode, "-- NORMAL --");
    }

    #[test]
    fn the_status_model_marks_the_current_seat() {
        let (mut app, _, bid) = app_with_two();
        app.switch_to(bid);
        let model = app.status_model(0);
        assert_eq!(model.seats[0].label, "kaijutsu");
        assert!(!model.seats[0].current);
        assert!(model.seats[1].current);
        assert_eq!(model.mode, "-- NORMAL --", "the vi mode is the status line's figure, not the model");
    }

    /// `Ctrl+C`'s own escalation is `interrupt::Ladder`'s job now
    /// (`docs/tui.md`, "Ctrl+C reclaimed"); `App` just tracks which contexts
    /// have a turn known running.
    #[test]
    fn turn_running_tracks_mark_and_clear() {
        let (mut app, aid, _) = app_with_two();
        assert!(!app.turn_running(aid));
        assert!(app.mark_turn_running(aid), "the set changed");
        assert!(!app.mark_turn_running(aid), "marking twice changes nothing");
        assert!(app.turn_running(aid));
        assert!(app.any_turn_running());
        assert!(app.mark_turn_ended(aid), "the set changed");
        assert!(!app.turn_running(aid));
        assert!(!app.any_turn_running());
    }

    /// `Ctrl+A n`/`p` walk the rank and wrap at both ends.
    #[test]
    fn seat_neighbor_steps_the_rank_and_wraps() {
        let (mut app, aid, bid) = app_with_two();
        let first = app.seat_context(0).expect("seat 0");
        let second = app.seat_context(1).expect("seat 1");
        assert!(first != second && [aid, bid].contains(&first));
        app.current = Some(first);
        assert_eq!(app.seat_neighbor(1), Some(second));
        assert_eq!(app.seat_neighbor(-1), Some(second), "wraps backward");
        app.current = Some(second);
        assert_eq!(app.seat_neighbor(1), Some(first), "wraps forward");
        app.current = None;
        assert_eq!(app.seat_neighbor(1), Some(first), "nothing on screen: n starts at the top");
        assert_eq!(app.seat_neighbor(-1), Some(second), "nothing on screen: p starts at the bottom");
    }

    #[test]
    fn any_turn_running_is_true_when_any_context_has_one() {
        let (mut app, aid, bid) = app_with_two();
        assert!(!app.any_turn_running());
        app.mark_turn_running(bid);
        assert!(app.any_turn_running(), "scoped to the whole client, not the context on screen");
        assert!(!app.turn_running(aid));
    }

    /// A lost event stream (a broadcast lag, a dropped connection) leaves
    /// this client with no idea which turns are still running, and a flag
    /// nothing will ever clear would make `:q` refuse forever — so the set
    /// is forgotten, not carried.
    #[test]
    fn forgetting_turn_liveness_clears_every_context() {
        let (mut app, aid, bid) = app_with_two();
        assert!(!app.forget_turn_liveness(), "nothing to forget yet");
        app.mark_turn_running(aid);
        app.mark_turn_running(bid);
        assert!(app.forget_turn_liveness());
        assert!(!app.any_turn_running());
        assert!(!app.turn_running(aid));
    }

    #[test]
    fn tool_blocks_arrive_whole_and_a_collapse_is_carried_forward() {
        let (_, aid, _) = app_with_two();
        let mut mirror = ContextMirror::new(aid);
        let call = block(aid, 1, BlockKind::ToolCall, Role::Model);
        let result = block(aid, 2, BlockKind::ToolResult, Role::Model);
        let error = block(aid, 3, BlockKind::Error, Role::Model);
        mirror
            .apply_snapshot(vec![call.clone(), result.clone(), error.clone()], 1)
            .expect("snapshot applies");
        let mut view = ContextView::new(mirror);
        assert!(!view.is_collapsed(&call), "tool output prints whole");
        assert!(!view.is_collapsed(&result), "tool output prints whole");
        assert!(view.is_collapsed(&error), "an error keeps its stub");

        view.collapsed.insert(call.id, true);
        view.seed_collapse();
        assert!(view.is_collapsed(&call), "a collapse is carried forward");
    }

    #[test]
    fn the_divider_names_the_local_user_for_their_own_text() {
        let (app, aid, _) = app_with_two();
        let b = block(aid, 1, BlockKind::Text, Role::User);
        assert_eq!(app.speaker_for(&b, app.info(aid)), "amy");
    }

    #[test]
    fn the_divider_names_the_model_for_a_reply() {
        let (app, aid, _) = app_with_two();
        let b = block(aid, 1, BlockKind::Text, Role::Model);
        assert_eq!(app.speaker_for(&b, app.info(aid)), "deepseek-v4");
    }

    #[test]
    fn the_divider_names_the_tool_for_a_tool_result() {
        let (app, aid, _) = app_with_two();
        let mut b = block(aid, 1, BlockKind::ToolResult, Role::Tool);
        b.tool_name = Some("shell".to_string());
        assert_eq!(app.speaker_for(&b, app.info(aid)), "shell");
    }

    /// `Thinking` never collapses on its own: `collapses_by_default`
    /// excludes it, and nothing collapses it at the turn's end, so reasoning
    /// prints whole (`docs/tui.md`, "Conversation").
    #[test]
    fn thinking_stays_expanded() {
        let (_, aid, _) = app_with_two();
        let mut mirror = ContextMirror::new(aid);
        let thinking = block(aid, 1, BlockKind::Thinking, Role::Model);
        mirror
            .apply_snapshot(vec![thinking.clone()], 1)
            .expect("snapshot applies");
        let view = ContextView::new(mirror);
        assert!(!view.is_collapsed(&thinking));
    }

    /// `ContextChange::CollapsedChanged` is what makes a sibling's collapse
    /// visible on a still-live block —
    /// `docs/tui.md`: "Collapse is kernel state ... so a sibling's expand is
    /// yours too."
    #[test]
    fn a_collapsed_change_updates_a_still_live_blocks_collapse_state() {
        let (mut app, aid, _) = app_with_two();
        let mut mirror = ContextMirror::new(aid);
        let call = block(aid, 1, BlockKind::ToolCall, Role::Model);
        mirror
            .apply_snapshot(vec![call.clone()], 1)
            .expect("snapshot applies");
        let view = ContextView::new(mirror);
        assert!(!view.is_collapsed(&call), "ToolCall arrives whole");
        app.views.insert(aid, view);

        app.apply_collapse_change(
            aid,
            &ContextChange::CollapsedChanged {
                block_id: call.id,
                collapsed: true,
            },
        );
        assert!(app.views[&aid].is_collapsed(&call), "the sibling's collapse landed");
    }

    /// A `CollapsedChanged` for a context with no watched view is not a bug
    /// — the ledger view, say, can see events for a context nobody has
    /// opened yet.
    #[test]
    fn a_collapsed_change_for_an_unwatched_context_is_a_no_op() {
        let (mut app, aid, _) = app_with_two();
        let block_id = BlockId::new(aid, PrincipalId::new(), 1);
        app.apply_collapse_change(
            aid,
            &ContextChange::CollapsedChanged {
                block_id,
                collapsed: true,
            },
        );
        assert!(!app.views.contains_key(&aid));
    }

    /// A submitted draft is promoted in place, and an echo of it that still
    /// reads `Draft` must not come back as the draft to type into.
    #[test]
    fn a_submitted_draft_is_no_longer_the_current_draft() {
        let (mut app, a, _b) = app_with_two();
        let principal = PrincipalId::new();
        app.principal = Some(principal);
        let id = BlockId::new(a, principal, 7);
        let draft = BlockSnapshotBuilder::new(id, BlockKind::Text)
            .role(Role::User)
            .status(Status::Draft)
            .content("why is the sky blue")
            .build();
        let mut mirror = ContextMirror::new(a);
        mirror.apply_snapshot(vec![draft], 3).expect("snapshot applies");
        app.views.insert(a, ContextView::new(mirror));
        app.switch_to(a);
        assert_eq!(app.current_draft().map(|(t, _)| t).as_deref(), Some("why is the sky blue"));

        app.mark_submitted(id);
        assert_eq!(app.current_draft(), None, "the promoted block is not a draft to refill from");
    }

    fn ask_card(request_id: &str, context_id: ContextId) -> crate::asks::AskCardState {
        crate::asks::AskCardState {
            request_id: request_id.to_string(),
            context_id,
            detail: kaijutsu_client::AskDetail {
                request_id: request_id.to_string(),
                context_id: Some(context_id),
                principal_id: None,
                principal_name: None,
                actor_id: None,
                actor_name: None,
                reviewer_id: None,
                reviewer_name: None,
                status: "pending".to_string(),
                origin: "shell_gate".to_string(),
                tool: Some("shell_write".to_string()),
                hook_id: None,
                instance: None,
                description: "kj cc send".to_string(),
                authorized_label: None,
                statements: vec!["kj cc send".to_string()],
                exec_source: None,
                cwd: None,
                env: Vec::new(),
                created_at: None,
                decided_at: None,
                decided_by: None,
                decided_by_name: None,
                decided_option: None,
                remember_scope: None,
                redeemed_at: None, publication_abandoned: None,
            },
        }
    }

    /// An ask answered from another surface leaves the pending set on the
    /// next poll; the card showing it must come down with it, or every key
    /// stays swallowed by a card nobody can answer.
    /// A terminal that reports `FocusLost` and never reports again must not
    /// leave the client believing nobody is looking for the rest of the
    /// session: the next key says otherwise.
    #[test]
    fn a_key_says_the_terminal_is_focused_whatever_it_last_reported() {
        let mut app = App::new("amy");
        app.focused = false;
        app.saw_input();
        assert!(app.focused, "input only reaches a focused terminal");
    }

    #[test]
    fn an_ask_card_comes_down_when_its_ask_leaves_the_pending_set() {
        let (mut app, a, _b) = app_with_two();
        app.ask_card = Some(ask_card("01a05d22", a));

        let still_pending: HashSet<String> = ["01a05d22".to_string()].into_iter().collect();
        assert!(app.take_answered_card(&still_pending).is_none(), "a pending ask keeps its card");
        assert!(app.ask_card.is_some());

        let taken = app.take_answered_card(&HashSet::new()).expect("the answered ask's card is handed back");
        assert_eq!(taken.request_id, "01a05d22");
        assert!(app.ask_card.is_none(), "the card is down");
        assert!(app.take_answered_card(&HashSet::new()).is_none(), "nothing to take twice");
    }

    /// The card is always the current context's ask, so leaving that
    /// context sets it aside: the seat on screen never shows another seat's
    /// card, and the ask stays pending for the refresh to raise again.
    #[test]
    fn switching_seats_sets_the_ask_card_aside() {
        let (mut app, a, b) = app_with_two();
        app.switch_to(a);
        app.ask_card = Some(ask_card("01a05d22", a));
        app.switch_to(a);
        assert!(app.ask_card.is_some(), "a no-op switch keeps the card");
        app.switch_to(b);
        assert!(app.ask_card.is_none(), "the card belongs to a, not to the seat on screen");
    }

    /// The player's edge on submit: the newest block the transcript showed,
    /// and how much of it was rendered (`docs/prompts.md`, "The submit
    /// verb"). A block whose tail was cut carries the block alone.
    #[test]
    fn the_shown_tail_is_the_edge() {
        let aid = ContextId::new();
        let mut view = ContextView::new(ContextMirror::new(aid));
        let streaming = block(aid, 2, BlockKind::Text, Role::Model);
        view.shown_tail = Some((streaming.id, Some(7)));

        let edge = view.edge().expect("a shown tail gives an edge");
        assert_eq!(edge.block, streaming.id);
        assert_eq!(edge.shown, Some(7));

        view.shown_tail = Some((streaming.id, None));
        assert_eq!(view.edge().expect("still an edge").shown, None, "a cut tail carries no count");
    }

    /// A context that has shown nothing at all sends no edge — the kernel
    /// never guesses one.
    #[test]
    fn a_context_with_nothing_shown_gives_no_edge() {
        let aid = ContextId::new();
        let view = ContextView::new(ContextMirror::new(aid));
        assert_eq!(view.edge(), None);
    }
}

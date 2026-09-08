//! Client state: which contexts are watched, what has already been printed,
//! the rank, the pending-ask count, and what the status line says.
//!
//! Pure. Nothing here opens a connection, reads a clock, or draws a cell —
//! every value a decision needs is handed in, so the whole module is
//! unit-testable without a kernel and without a terminal.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use kaijutsu_client::{
    ConnectionStatus, ContextChange, ContextInfo, ContextMirror, RankedSeat, ranked_seats,
};
use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContextId, PrincipalId, Role, Status};

use crate::compose::Compose;
use crate::present::{BlockView, Palette, WrapCache, collapses_by_default};
use crate::status::{CacheHealth, SeatCell, StatusModel, cache_health};

/// The double-tap window, shared by `Ctrl+C Ctrl+C` and the prefix's
/// `Ctrl+A Ctrl+A` (`docs/input.md`, the app's `Esc Esc` pattern).
pub const DOUBLE_TAP: Duration = Duration::from_millis(500);

/// One watched context: its mirror, and what this client has already printed
/// into the terminal's scrollback.
pub struct ContextView {
    pub mirror: ContextMirror,
    /// Blocks already printed to scrollback. They cannot be redrawn — that
    /// is the price of the inline viewport (`docs/tui.md`, ruling 1).
    pub printed: HashSet<BlockId>,
    /// Per-block collapse, seeded from [`collapses_by_default`] on first
    /// arrival and then carried forward, so a later expand is not wiped by
    /// the next redraw.
    pub collapsed: HashMap<BlockId, bool>,
    /// Activity since this context was last on screen — screen's `@` flag.
    pub activity: bool,
    /// Who the last block printed to scrollback was from, so a run of blocks
    /// by one speaker carries one divider even when they print on separate
    /// frames.
    pub last_printed_speaker: Option<String>,
    /// The last block printed, so a tool result printed on a later frame
    /// than its call still joins the pair under one header
    /// (`present::continues_pair`).
    pub last_printed: Option<(BlockId, BlockKind)>,
}

impl ContextView {
    pub fn new(mirror: ContextMirror) -> Self {
        let mut view = Self {
            mirror,
            printed: HashSet::new(),
            collapsed: HashMap::new(),
            activity: false,
            last_printed_speaker: None,
            last_printed: None,
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
    /// The last copy-mode yank, for `Ctrl+A ]` — tmux's paste buffer. The
    /// tui's own, so it never needs aligning with vim's registers or the OS
    /// clipboard (the yank also goes to the clipboard over OSC 52, but that
    /// is a one-way emission the tui cannot read back).
    pub paste_buffer: Option<String>,
    /// What has displaced the inline viewport, when anything has. Only the
    /// editor and the diff viewer take the alternate screen (`docs/tui.md`,
    /// ruling 1), and the key path early-returns on it, which is what makes
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
    /// than one at a time (`docs/tui.md`, "Asks": it grows the viewport, it
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
    /// The picker, when `Ctrl+A "` has it open — `render::viewport_lines`
    /// and `render::live_lines` both read this.
    pub picker: Option<crate::picker::PickerModel>,
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
            screen: crate::editor::ScreenMode::Inline,
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
        }
    }

    /// Take a fresh `list_contexts` answer and recompute the rank.
    pub fn set_contexts(&mut self, contexts: Vec<ContextInfo>) {
        self.seats = ranked_seats(&contexts);
        self.contexts = contexts;
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
    /// when it has none — the same name the status line's seat cells and
    /// copy mode's header use.
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

    /// Record that a block has been printed into scrollback and can never be
    /// redrawn, and remember who spoke it.
    pub fn mark_printed(&mut self, context_id: ContextId, block_id: BlockId, kind: BlockKind, speaker: &str) {
        if let Some(view) = self.views.get_mut(&context_id) {
            view.printed.insert(block_id);
            view.last_printed_speaker = Some(speaker.to_string());
            view.last_printed = Some((block_id, kind));
        }
        self.wrap.forget(&block_id);
    }

    /// Late-change honesty: a change to a block already in scrollback cannot
    /// redraw it, so say so instead of pretending. The change is real in the
    /// kernel and in the next hydrate (`docs/tui.md`, "Conversation").
    ///
    /// Returns `true` when a notice was posted.
    pub fn observe_change(&mut self, context_id: ContextId, change: &ContextChange) -> bool {
        let Some(block_id) = changed_block(change) else {
            return false;
        };
        let printed = self
            .views
            .get(&context_id)
            .is_some_and(|v| v.printed.contains(&block_id));
        if !printed {
            return false;
        }
        self.note(format!("block #{} changed after print", block_id.seq));
        true
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
    /// its mirror holds a `Thinking` block not yet printed — a block that
    /// completed inside one delivery counts, so a fast model's reasoning
    /// opens the pane as surely as a slow one's. Returns whether it latched
    /// now.
    pub fn observe_thinking(&mut self, context_id: ContextId) -> bool {
        if !self.turn_running(context_id) || self.thinking_turns.contains(&context_id) {
            return false;
        }
        let seen = self.views.get(&context_id).is_some_and(|view| {
            view.mirror
                .blocks()
                .iter()
                .any(|b| b.kind == BlockKind::Thinking && !view.printed.contains(&b.id))
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

    /// Sync a still-live block's collapse state from the feed. Collapse is
    /// kernel state (`docs/tui.md`, "Conversation": "a sibling's expand is
    /// yours too") — this is what makes `ContextChange::CollapsedChanged`
    /// actually reach [`ContextView::collapsed`] for a sibling's manual
    /// toggle. A block already printed to scrollback cannot be redrawn either
    /// way (`App::observe_change` posts that notice); updating the map here
    /// is harmless in that case because nothing reads it again.
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

    /// The seat this client answers an ask from: any context it holds that
    /// is not `raised_in` — the last context first, then the rank in seat
    /// order, then any context it knows. `None` when every context it can
    /// name is the ask's own. The kernel refuses an answer from the context
    /// that raised the ask (`docs/gate-and-shell-split.md`, "No
    /// self-approval"): peer seats answer each other, and a player with
    /// several seats is that peer.
    pub fn answering_seat(&self, raised_in: ContextId) -> Option<ContextId> {
        self.previous
            .into_iter()
            .chain(self.seats.iter().map(|s| s.context_id))
            .chain(self.contexts.iter().map(|c| c.id))
            .find(|id| *id != raised_in)
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

/// The block a change names, or `None` for a change that adds one.
fn changed_block(change: &ContextChange) -> Option<BlockId> {
    match change {
        ContextChange::BlockInserted { .. } => None,
        ContextChange::BlockDeleted { block_id }
        | ContextChange::BlockMoved { block_id, .. }
        | ContextChange::TextAppended { block_id, .. }
        | ContextChange::TextReplaced { block_id, .. }
        | ContextChange::StatusChanged { block_id, .. }
        | ContextChange::CollapsedChanged { block_id, .. }
        | ContextChange::ExcludedChanged { block_id, .. }
        | ContextChange::MetadataChanged { block_id, .. }
        | ContextChange::OutputChanged { block_id, .. }
        | ContextChange::SpansChanged { block_id, .. } => Some(*block_id),
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

    /// A change to a block already printed to scrollback cannot redraw it.
    /// The TUI says so in the status line rather than pretending — this is
    /// the price of the inline viewport, paid knowingly.
    #[test]
    fn a_change_to_a_printed_block_posts_a_notice() {
        let (mut app, aid, _) = app_with_two();
        app.views
            .insert(aid, ContextView::new(ContextMirror::new(aid)));
        let b = block(aid, 12, BlockKind::Text, Role::Model);
        app.mark_printed(aid, b.id, b.kind, "model");

        let posted = app.observe_change(
            aid,
            &ContextChange::ExcludedChanged {
                block_id: b.id,
                excluded: true,
            },
        );
        assert!(posted);
        assert_eq!(app.notice(), Some("block #12 changed after print"));
    }

    #[test]
    fn a_change_to_a_block_still_in_the_viewport_posts_nothing() {
        let (mut app, aid, _) = app_with_two();
        app.views
            .insert(aid, ContextView::new(ContextMirror::new(aid)));
        let b = block(aid, 7, BlockKind::Text, Role::Model);

        let posted = app.observe_change(
            aid,
            &ContextChange::TextAppended {
                block_id: b.id,
                suffix: " more".to_string(),
            },
        );
        assert!(!posted);
        assert_eq!(app.notice(), None);
    }

    #[test]
    fn a_late_collapse_of_a_printed_block_also_posts() {
        let (mut app, aid, _) = app_with_two();
        app.views
            .insert(aid, ContextView::new(ContextMirror::new(aid)));
        let b = block(aid, 3, BlockKind::ToolResult, Role::Tool);
        app.mark_printed(aid, b.id, b.kind, "shell");
        assert!(app.observe_change(
            aid,
            &ContextChange::CollapsedChanged {
                block_id: b.id,
                collapsed: false,
            }
        ));
        assert_eq!(app.notice(), Some("block #3 changed after print"));
    }

    #[test]
    fn a_new_block_is_never_a_late_change() {
        let (mut app, aid, _) = app_with_two();
        app.views
            .insert(aid, ContextView::new(ContextMirror::new(aid)));
        let b = block(aid, 1, BlockKind::Text, Role::Model);
        assert!(!app.observe_change(
            aid,
            &ContextChange::BlockInserted {
                block: Box::new(b),
                after_id: None,
            }
        ));
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
                decided_option: None,
                remember_scope: None,
                redeemed_at: None,
            },
        }
    }

    /// An ask is answered from a seat that is not its own context — the
    /// last context when there is one, else the first other seat — and a
    /// client holding only the ask's context has no seat to answer from.
    #[test]
    fn an_ask_is_answered_from_another_seat_the_client_holds() {
        let (mut app, a, b) = app_with_two();
        app.switch_to(a);
        assert_eq!(app.answering_seat(a), Some(b), "the other seat answers a's ask");
        assert_eq!(app.answering_seat(b), Some(a));
        app.switch_to(b);
        app.switch_to(a);
        assert_eq!(app.answering_seat(a), Some(b), "the last context answers first");
        let (only, id) = {
            let mut only = App::new("amy");
            let c = ctx("alone");
            let id = c.id;
            only.set_contexts(vec![c]);
            (only, id)
        };
        assert_eq!(only.answering_seat(id), None, "no seat but the ask's own");
    }

    /// An ask answered from another surface leaves the pending set on the
    /// next poll; the card showing it must come down with it, or every key
    /// stays swallowed by a card nobody can answer.
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
}

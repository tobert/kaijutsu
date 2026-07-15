//! Live state beyond the poll: per-context **tail buffers** (the "tail -f"
//! view of what each context just did) and per-track **beat phasors** (the
//! beat made visible).
//!
//! Both ingest the same kernel-wide `ServerEvent` stream the activity deck
//! rides (see [`super::activity`]), but the ingest system runs **ungated**
//! (every screen, like `metronome::ingest_beat_signals`) so the well opens
//! warm: tails accumulate and phasors stay locked while you're in the
//! conversation view.
//!
//! Beat stance: *distribute tempo, not pulses* (`docs/midi.md`, applied to
//! viz). The kernel ships low-rate [`ServerEvent::BeatSync`] references keyed
//! by the track's **score context**; each becomes a local
//! [`LocalBeat`] phasor here — the "later cut keys per track/score context"
//! that `metronome.rs` anticipated. The pulse animation is derived locally
//! from the phasor every frame; nothing streams per-beat over the wire.
//!
//! Render targets (no card-texture rebuilds — see `WellCardMaterial`):
//! - `dim.y` = **chatter**: the context's decaying event energy
//!   ([`super::activity::RingActivity::context_energy`]) — a cyan rim lift
//!   the instant a card's context is talking.
//! - `dim.z` = **beat**: the envelope of the phasor keyed by this context —
//!   today that lights the score-context card; the track roster (Stage 3
//!   wire) extends it to every attached context's card.
//! - `WellRingsMaterial.energy.y` = the **global** beat envelope — the throat
//!   glow breathes on the beat of whatever is playing.
//! - `Card::tail` = the selected card's own live-tail band, rendered directly
//!   on its face (the retired HUD South panel's old job, absorbed HUD-melt
//!   slice 2).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use kaijutsu_audio::{BeatRef, LocalBeat, RENDER_FLUSH_MIME};
use kaijutsu_client::ServerEvent;
use kaijutsu_crdt::BlockId;
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, Role, Status};

use crate::connection::actor_plugin::ServerEventMessage;

/// Lines kept per context tail (the tail -f window).
pub const TAIL_LINES: usize = 8;
/// Max chars per tail line (head of the block's first content line).
pub const TAIL_LINE_CHARS: usize = 90;
/// Max contexts holding a tail; beyond this the oldest-touched is dropped.
/// Bounds memory across a long app life — the well only ever *shows* the
/// selected context's tail, so eviction is invisible in practice.
const TAIL_CONTEXT_CAP: usize = 256;

/// Beat-envelope decay: `exp(-DECAY × beat_fraction)` — 1.0 on the beat,
/// ~0.08 by the half-beat. Sharp enough to read as a pulse, soft enough not
/// to strobe. **Amy-tunable.**
const BEAT_ENVELOPE_DECAY: f64 = 5.0;

/// Drop a phasor that hasn't seen a reference for this long. References
/// arrive every 8 beats while a clock rolls (4s at 120 BPM), and stop/pause
/// sends an explicit flush — this guard only catches the abnormal paths
/// (kernel restart mid-play, dropped stream) so a dead track can't pulse
/// forever.
const PHASOR_STALE: Duration = Duration::from_secs(30);

// ============================================================================
// TAILS
// ============================================================================

/// One rendered tail line: a kind glyph + the head of the block's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailLine {
    pub glyph: &'static str,
    pub text: String,
    /// Set on a **placeholder** line (an empty streaming insert — "⋯
    /// composing") so the block's later `Done`/`Error` status flip can
    /// resolve it in place ([`ContextTails::resolve`]); cleared once
    /// resolved. `None` for lines that arrived whole.
    pub block: Option<BlockId>,
}

impl TailLine {
    pub fn new(glyph: &'static str, text: impl Into<String>) -> Self {
        Self { glyph, text: text.into(), block: None }
    }

    /// The display form the card's tail band renders: `glyph text`.
    pub fn display(&self) -> String {
        format!("{} {}", self.glyph, self.text)
    }
}

/// A context's rolling tail (oldest → newest) + when it was last touched
/// (app-elapsed seconds, for cap eviction).
#[derive(Default)]
struct Tail {
    lines: VecDeque<TailLine>,
    touched: f64,
}

/// Per-context tail buffers fed from the kernel-wide block stream.
#[derive(Resource, Default)]
pub struct ContextTails {
    tails: HashMap<ContextId, Tail>,
}

impl ContextTails {
    /// Append a line to `ctx`'s tail (evicting its oldest line past
    /// [`TAIL_LINES`]), stamping the touch time; past [`TAIL_CONTEXT_CAP`]
    /// contexts the oldest-touched whole tail is dropped.
    pub fn push(&mut self, ctx: ContextId, line: TailLine, now: f64) {
        let tail = self.tails.entry(ctx).or_default();
        if tail.lines.len() >= TAIL_LINES {
            tail.lines.pop_front();
        }
        tail.lines.push_back(line);
        tail.touched = now;

        if self.tails.len() > TAIL_CONTEXT_CAP {
            if let Some(oldest) = self
                .tails
                .iter()
                .min_by(|a, b| a.1.touched.total_cmp(&b.1.touched))
                .map(|(id, _)| *id)
            {
                self.tails.remove(&oldest);
            }
        }
    }

    /// The tail for `ctx`, oldest → newest (tail -f order). Empty when the
    /// context hasn't produced a line since the app started.
    pub fn iter_lines(&self, ctx: &ContextId) -> impl Iterator<Item = &TailLine> {
        self.tails.get(ctx).into_iter().flat_map(|t| t.lines.iter())
    }

    /// Resolve a placeholder line in place when its block's turn concludes:
    /// "⋯ composing" → "✦ replied" / "✕ turn failed". No-op for blocks the
    /// tail never placeholdered (whole-content lines, evicted lines) and for
    /// non-terminal flips (Running). The window is [`TAIL_LINES`] long, so
    /// the scan is trivial.
    pub fn resolve(&mut self, ctx: &ContextId, block: BlockId, status: Status) {
        let Some(tail) = self.tails.get_mut(ctx) else { return };
        let Some(line) = tail.lines.iter_mut().find(|l| l.block == Some(block)) else {
            return;
        };
        match status {
            Status::Done => {
                line.glyph = "✦";
                line.text = "replied".into();
                line.block = None;
            }
            Status::Error => {
                line.glyph = "✕";
                line.text = "turn failed".into();
                line.block = None;
            }
            _ => {}
        }
    }
}

/// Pick + truncate the newest `n_lines` of `ctx`'s live tail buffer, each
/// capped at `line_chars`, oldest → newest, joined with `\n`. `None` when the
/// context hasn't produced a tail line since the app started — the caller
/// decides what "nothing yet" means: the retired HUD South panel used to fall
/// back to the polled preview; the card face's own gist line
/// (`text::card_text_glyphs`) already shows that same preview, so its tail
/// band ([`super::scene::Card::tail`], via [`sync_selected_card_tail`]) skips
/// entirely rather than repeating it.
///
/// Shared pure text-shaping — the retired HUD South panel's own logic before
/// this extraction, now the one place the card face's live-tail band picks
/// its lines from (`docs/timewell.md`'s HUD melt, slice 2).
pub fn tail_lines(tails: &ContextTails, ctx: ContextId, n_lines: usize, line_chars: usize) -> Option<String> {
    let lines: Vec<String> = tails
        .iter_lines(&ctx)
        .map(|l| crate::text::truncate_chars(&l.display(), line_chars))
        .collect();
    if lines.is_empty() {
        return None;
    }
    let newest = lines.len();
    Some(lines[newest.saturating_sub(n_lines)..].join("\n"))
}

/// Head of the first non-empty content line, truncated to
/// [`TAIL_LINE_CHARS`]. `None` when there is no visible text at all.
fn head_line(content: &str) -> Option<String> {
    let line = content.lines().find(|l| !l.trim().is_empty())?;
    Some(crate::text::truncate_chars(line.trim(), TAIL_LINE_CHARS))
}

/// The `command` string from a tool call's input JSON, if it has one — the
/// human-recognizable line for shell-shaped tools. `None` for other tools or
/// unparseable input.
fn command_arg_head(block: &BlockSnapshot) -> Option<String> {
    let input = block.tool_input.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(input).ok()?;
    head_line(v.get("command")?.as_str()?)
}

/// Map an inserted block to a tail line, or `None` for blocks that carry no
/// glanceable signal (thinking, structural kinds).
///
/// Model text usually inserts **empty** and streams in via `BlockTextOps`
/// (CRDT deltas this module doesn't decode), so an empty model insert becomes
/// a "⋯ composing" **placeholder** tagged with its block id — the block's
/// `Done`/`Error` flip resolves it in place ([`ContextTails::resolve`]), so
/// the tail narrates the turn without holding a CRDT replica (Gemini review,
/// 2026-07-04). Everything else catches blocks that arrive whole — user
/// prompts, tool calls, results, errors, notifications, and materialized
/// score cells (tagged with their track).
pub fn tail_line(block: &BlockSnapshot) -> Option<TailLine> {
    match block.kind {
        BlockKind::Text => {
            let Some(head) = head_line(&block.content) else {
                // User text never streams — an empty user row is just noise.
                if block.role == Role::User {
                    return None;
                }
                return Some(TailLine {
                    glyph: "✦",
                    text: "⋯ composing".into(),
                    block: Some(block.id),
                });
            };
            // A materialized score cell carries its lane — show it.
            if let Some(track) = &block.track {
                return Some(TailLine::new("♪", format!("{}: {}", track.as_str(), head)));
            }
            let glyph = if block.role == Role::User { "❯" } else { "✦" };
            Some(TailLine::new(glyph, head))
        }
        BlockKind::ToolCall => {
            // The tool name is the signal; the input JSON body is noise —
            // except a `command` arg (shell/kaish calls), where the command
            // IS the story: a tail of bare "▸ shell" ×4 told nothing
            // (live-verify, 2026-07-04).
            let name = block.tool_name.as_deref().unwrap_or("tool");
            let text = match command_arg_head(block) {
                Some(cmd) => format!("{name}: {cmd}"),
                None => name.to_string(),
            };
            Some(TailLine::new("▸", crate::text::truncate_chars(&text, TAIL_LINE_CHARS)))
        }
        BlockKind::ToolResult => {
            let head = head_line(&block.content)?;
            let glyph = if block.is_error { "✕" } else { "◂" };
            Some(TailLine::new(glyph, head))
        }
        BlockKind::Error => Some(TailLine::new(
            "✕",
            head_line(&block.content).unwrap_or_else(|| "error".into()),
        )),
        BlockKind::Drift => Some(TailLine::new("≈", head_line(&block.content)?)),
        BlockKind::Notification => Some(TailLine::new("◆", head_line(&block.content)?)),
        BlockKind::File => {
            let text = block
                .file_path
                .clone()
                .or_else(|| head_line(&block.content))?;
            Some(TailLine::new("⎘", crate::text::truncate_chars(&text, TAIL_LINE_CHARS)))
        }
        // Thinking streams in empty (and is inner voice, not activity);
        // everything else is structural.
        _ => None,
    }
}

// ============================================================================
// BEATS
// ============================================================================

/// A phasor + when it last saw a wire reference (staleness guard).
struct Phasor {
    beat: LocalBeat,
    last_ref: Instant,
}

/// Per-track beat phasors, keyed by the track's **score context** (the id
/// both `BeatSync` and `RenderCue` carry). Multi-track from day one — this is
/// the generalization `metronome.rs` deferred.
#[derive(Resource, Default)]
pub struct WellBeats {
    phasors: HashMap<ContextId, Phasor>,
}

/// Pure envelope shape: 1.0 on the beat, decaying exponentially through the
/// beat. `frac` is the fractional position within the current beat (0..1).
pub fn beat_envelope_at(frac: f64) -> f32 {
    (-BEAT_ENVELOPE_DECAY * frac).exp() as f32
}

impl WellBeats {
    /// Fold a wire reference into the phasor for `ctx` (anchoring on first).
    ///
    /// `at` is the reference's own back-dated emission instant
    /// (`BeatRef::backdated_at`) — what the phasor folds against, so a flood
    /// of buffered refs settles at the newest ref's true position instead of
    /// walking several beats at one shared receipt `now`. `received` is the
    /// *actual* receipt instant, separate from `at`: it is what `last_ref`
    /// (the [`Self::prune_stale`] liveness clock) stamps. These differ on
    /// purpose — folding at a back-dated `at` can leave `at` seconds behind
    /// `received` on a delivery flood, and stamping liveness from the
    /// (older) `at` instead of the (fresher) `received` would let
    /// `prune_stale` kill a phasor that is, in wall-clock reality, still
    /// live (a sustained backlog of old-but-not-stale refs proves the track
    /// is alive even while every individual ref reads a bit behind).
    pub fn observe(&mut self, ctx: ContextId, reference: BeatRef, at: Instant, received: Instant) {
        match self.phasors.get_mut(&ctx) {
            Some(p) => {
                p.beat.observe(reference, at);
                p.last_ref = received;
            }
            None => {
                self.phasors.insert(
                    ctx,
                    Phasor { beat: LocalBeat::new(reference, at), last_ref: received },
                );
            }
        }
    }

    /// Bump the liveness clock for an EXISTING phasor without touching its
    /// beat position — the arm for a reference that arrived but was too
    /// stale to fold (`BeatRef::backdated_at` returned `None`). A stale ref
    /// still proves the track is alive (something arrived), so `prune_stale`
    /// must not reap it; but folding it would anchor the phasor's position in
    /// the past, so the beat itself is left untouched. A no-op if `ctx` has
    /// no phasor yet — there is nothing to keep alive, and this must never
    /// create one (that would anchor a fresh phasor with no position at all).
    pub fn touch(&mut self, ctx: &ContextId, received: Instant) {
        if let Some(p) = self.phasors.get_mut(ctx) {
            p.last_ref = received;
        }
    }

    /// Transport flush (stop/pause): drop the phasor so the pulse halts —
    /// same contract as `Metronome::reset`, but per track.
    pub fn reset(&mut self, ctx: &ContextId) {
        self.phasors.remove(ctx);
    }

    /// Drop phasors that stopped receiving references without a flush
    /// (kernel restart, dropped stream) so a dead track can't pulse forever.
    pub fn prune_stale(&mut self, now: Instant) {
        self.phasors
            .retain(|_, p| now.duration_since(p.last_ref) < PHASOR_STALE);
    }

    /// The beat envelope (0..1) for the phasor keyed by `ctx`; 0.0 when no
    /// track is rolling under that key.
    pub fn envelope(&self, ctx: &ContextId, now: Instant) -> f32 {
        self.envelope_and_frac(ctx, now).0
    }

    /// The beat envelope plus the fractional position within the current beat
    /// (0..1 — the track-ray pulse's position along the beam); `(0.0, 0.0)`
    /// when no track is rolling under that key.
    pub fn envelope_and_frac(&self, ctx: &ContextId, now: Instant) -> (f32, f32) {
        self.phasors
            .get(ctx)
            .map(|p| {
                let pos = p.beat.position(now);
                let frac = pos - pos.floor();
                (beat_envelope_at(frac), frac as f32)
            })
            .unwrap_or((0.0, 0.0))
    }

    /// The phasor's raw beat position (unbounded, NOT wrapped to `0..1` the
    /// way [`Self::envelope_and_frac`]'s `frac` is) for the track keyed by
    /// `ctx` — `None` when no phasor is live under that key. This is the
    /// **freeze signal** the tracker station's scroll math anchors on
    /// (`tracker::grid::row_offset`'s `p` argument): `Some` while a track's
    /// clock is rolling, `None` the instant a transport flush drops the
    /// phasor ([`Self::reset`]). A caller scrolling rows on this position
    /// caches the last `Some` value and simply stops writing on `None` —
    /// exact freeze, not a fallback to `0.0` (which would snap the grid back
    /// to the playhead instead of holding still).
    pub fn beat_position(&self, ctx: &ContextId, now: Instant) -> Option<f64> {
        self.phasors.get(ctx).map(|p| p.beat.position(now))
    }

    /// The loudest envelope across every rolling track — the well's shared
    /// heartbeat (phase across independent clock domains is meaningless, so
    /// max, not sum).
    pub fn global_envelope(&self, now: Instant) -> f32 {
        self.phasors
            .values()
            .map(|p| {
                let pos = p.beat.position(now);
                beat_envelope_at(pos - pos.floor())
            })
            .fold(0.0, f32::max)
    }

    /// Whether any track's clock is rolling (has a live phasor). Strictly a
    /// test helper — the reading card's transport readout
    /// (`text::specs_text`/`text::reading_specs_text`) uses `TrackInfo.playing`
    /// from the `listTracks` poll, not the phasor set.
    #[cfg(test)]
    pub fn any_rolling(&self) -> bool {
        !self.phasors.is_empty()
    }
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Ingest the kernel-wide event stream into tails + phasors. Runs **ungated**
/// (every screen) so the well opens warm; both resources are bounded
/// ([`TAIL_CONTEXT_CAP`], one phasor per rolling track).
///
/// Each `BeatSync` folds at its own back-dated emission instant
/// (`BeatRef::backdated_at`), not this frame's shared `now_inst` — the same
/// flood-resistance fix as `metronome::ingest_beat_signals`. A stale ref
/// (age > `REF_STALE_MAX`) still proves the track alive: it bumps the
/// phasor's liveness clock via [`WellBeats::touch`] without folding a beat
/// position from the past.
pub fn ingest_live_events(
    mut events: MessageReader<ServerEventMessage>,
    mut tails: ResMut<ContextTails>,
    mut beats: ResMut<WellBeats>,
    time: Res<Time>,
) {
    let now_inst = Instant::now();
    let now_epoch_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let now = time.elapsed_secs_f64();
    for ServerEventMessage(ev) in events.read() {
        match ev {
            ServerEvent::BlockInserted { context_id, block, .. } => {
                if let Some(line) = tail_line(block) {
                    tails.push(*context_id, line, now);
                }
            }
            ServerEvent::BlockStatusChanged { context_id, block_id, status } => {
                tails.resolve(context_id, *block_id, *status);
            }
            ServerEvent::BeatSync { context_id, beat_ref } => {
                match beat_ref.backdated_at(now_inst, now_epoch_ns) {
                    Some(at) => beats.observe(*context_id, *beat_ref, at, now_inst),
                    None => beats.touch(context_id, now_inst),
                }
            }
            ServerEvent::RenderCue { context_id, cue } if cue.mime == RENDER_FLUSH_MIME => {
                beats.reset(context_id);
            }
            _ => {}
        }
    }
    beats.prune_stale(now_inst);
}

/// Quantization step for the live uniform lanes: coarse enough that a settled
/// card stops re-extracting its material, fine enough that the decay reads
/// smooth under bloom.
const LIVE_LANE_STEP: f32 = 1.0 / 64.0;

fn quantize(v: f32) -> f32 {
    (v / LIVE_LANE_STEP).round() * LIVE_LANE_STEP
}

/// Steady border strength for a card on a track: the lane's hue as identity.
/// LDR — passive structure, not action (the beat thump is the bright part).
const TRACK_BORDER_STRENGTH: f32 = 0.55;

/// Push each card's live lanes into its material: `dim.y` = chatter (the
/// context's decaying event energy), `dim.z` = beat envelope, and `border` =
/// its track's hue when attached. The beat is keyed through
/// [`super::rays::WellTracks::beat_key_of`] — every card on a lane (players
/// and score alike) thumps with its track's phasor; a context not on the
/// roster falls back to its own id (which still lights a score context up
/// before the first track poll lands). Values are quantized and
/// change-guarded so a quiet card never touches `Assets<WellCardMaterial>`
/// (same discipline as `scene::dim_nonfocused_rings`).
pub fn sync_card_live_uniforms(
    activity: Res<super::activity::RingActivity>,
    beats: Res<WellBeats>,
    tracks: Res<super::rays::WellTracks>,
    mut materials: ResMut<Assets<crate::shaders::WellCardMaterial>>,
    cards: Query<(
        &super::scene::Card,
        &MeshMaterial3d<crate::shaders::WellCardMaterial>,
    )>,
) {
    let now = Instant::now();
    for (card, handle) in cards.iter() {
        let chatter = quantize(
            (activity.context_energy(&card.context_id) / super::activity::CONTEXT_MAX)
                .clamp(0.0, 1.0),
        );
        let beat_key = tracks
            .beat_key_of
            .get(&card.context_id)
            .unwrap_or(&card.context_id);
        let beat = quantize(beats.envelope(beat_key, now));
        let border = match tracks.track_of.get(&card.context_id) {
            Some(track_id) => {
                let c = super::scene::accent_color(track_id).to_linear();
                Vec4::new(c.red, c.green, c.blue, TRACK_BORDER_STRENGTH)
            }
            None => Vec4::ZERO,
        };
        // Read via the non-dirtying `get`; only reach for `get_mut` on change.
        let Some(cur) = materials
            .get(&handle.0)
            .map(|m| (m.dim.y, m.dim.z, m.border))
        else {
            continue;
        };
        if cur != (chatter, beat, border)
            && let Some(mat) = materials.get_mut(&handle.0)
        {
            mat.dim.y = chatter;
            mat.dim.z = beat;
            mat.border = border;
        }
    }
}

/// Tail lines shown in the selected card's live-tail band — fewer than the
/// retired HUD South panel used to show (`SOUTH_TAIL_LINES` was 5, a wider
/// panel) since the card face is smaller and the band is meant to stay small
/// and dim under the title/badge/gist area, not dominate it.
const CARD_TAIL_LINES: usize = 3;

/// Selected-card-ONLY, dived-only live-tail sync: writes [`super::scene::Card::tail`]
/// **only when its content actually changed** (same guarded-write discipline
/// as `scene::highlight_selection`/`highlight_lineage` — the change guard the
/// mission brief asked to find and reuse), so it rides the EXISTING
/// `Changed<Card>` gate `text::build_card_scenes` already has instead of
/// adding a second rebuild path. Every non-selected card's tail clears to
/// `None` the same way (one pass over every card, same shape as the
/// selection/lineage overlays — not a special-cased single-entity lookup).
///
/// Dived-only, like every other card-TEXT builder (`text::build_card_scenes`'s
/// own doc has the "unreadable pixels at room scale" reasoning this system
/// shares) — `ingest_live_events` keeps filling [`ContextTails`] ungated
/// regardless of screen/zoom, so nothing is missed: the next dive recomputes
/// fresh from whatever accumulated while ambient.
pub fn sync_selected_card_tail(
    state: Res<super::scene::TimeWellState>,
    tails: Res<ContextTails>,
    mut cards: Query<&mut super::scene::Card>,
) {
    for mut card in cards.iter_mut() {
        let next = if Some(card.context_id) == state.selected {
            tail_lines(&tails, card.context_id, CARD_TAIL_LINES, super::text::GIST_LINE_CHARS)
        } else {
            None
        };
        if card.tail != next {
            card.tail = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_crdt::BlockId;
    use kaijutsu_types::{PrincipalId, ToolKind, TrackId};

    fn ctx(n: u8) -> ContextId {
        ContextId::from_bytes([n; 16])
    }

    fn bid(n: u8) -> BlockId {
        BlockId::new(ctx(n), PrincipalId::nil(), 0)
    }

    // ── tail_line ──

    #[test]
    fn user_and_model_text_get_distinct_glyphs() {
        let user = BlockSnapshot::text(bid(1), None, Role::User, "run the tests");
        let model = BlockSnapshot::text(bid(1), None, Role::Model, "on it");
        assert_eq!(tail_line(&user).unwrap().glyph, "❯");
        assert_eq!(tail_line(&model).unwrap().glyph, "✦");
        assert_eq!(tail_line(&user).unwrap().text, "run the tests");
    }

    #[test]
    fn empty_model_insert_becomes_a_placeholder_the_status_flip_resolves() {
        // Model turns insert empty then stream via TextOps — the tail shows a
        // tagged "composing" placeholder that the terminal status resolves.
        let empty = BlockSnapshot::text(bid(1), None, Role::Model, "");
        let line = tail_line(&empty).expect("placeholder line");
        assert_eq!(line.text, "⋯ composing");
        assert_eq!(line.block, Some(bid(1)), "tagged for resolution");

        let mut tails = ContextTails::default();
        tails.push(ctx(1), line, 0.0);
        // A non-terminal flip (Running) leaves the placeholder alone.
        tails.resolve(&ctx(1), bid(1), kaijutsu_types::Status::Running);
        assert_eq!(tails.iter_lines(&ctx(1)).next().unwrap().text, "⋯ composing");
        // Done resolves it in place and clears the tag.
        tails.resolve(&ctx(1), bid(1), kaijutsu_types::Status::Done);
        let resolved = tails.iter_lines(&ctx(1)).next().unwrap();
        assert_eq!(resolved.display(), "✦ replied");
        assert_eq!(resolved.block, None, "tag cleared once resolved");
        // A later flip for the same block is a no-op (tag gone).
        tails.resolve(&ctx(1), bid(1), kaijutsu_types::Status::Error);
        assert_eq!(tails.iter_lines(&ctx(1)).next().unwrap().display(), "✦ replied");

        // An errored turn reads as a failure.
        let mut tails = ContextTails::default();
        tails.push(ctx(1), tail_line(&empty).unwrap(), 0.0);
        tails.resolve(&ctx(1), bid(1), kaijutsu_types::Status::Error);
        assert_eq!(tails.iter_lines(&ctx(1)).next().unwrap().display(), "✕ turn failed");

        // Empty USER text stays out of the tail (it never streams).
        let blank = BlockSnapshot::text(bid(1), None, Role::User, "  \n\t\n");
        assert!(tail_line(&blank).is_none());
    }

    #[test]
    fn tool_call_shows_the_tool_name_not_the_input_json() {
        let call = BlockSnapshot::tool_call(
            bid(1),
            None,
            ToolKind::Builtin,
            "kaijutsu:read",
            serde_json::json!({"path": kaijutsu_types::paths::RC_ROOT}),
            Role::Model,
            None,
        );
        let line = tail_line(&call).unwrap();
        assert_eq!(line.glyph, "▸");
        assert_eq!(line.text, "kaijutsu:read");
        assert!(!line.text.contains('{'), "input JSON stays out of the tail");
    }

    #[test]
    fn shell_shaped_tool_call_shows_its_command() {
        let call = BlockSnapshot::tool_call(
            bid(1),
            None,
            ToolKind::Shell,
            "shell",
            serde_json::json!({"command": "kj transport play --track welltest"}),
            Role::User,
            None,
        );
        let line = tail_line(&call).unwrap();
        assert_eq!(line.display(), "▸ shell: kj transport play --track welltest");
    }

    #[test]
    fn score_cell_carries_its_track_lane() {
        let mut cell = BlockSnapshot::text(bid(1), None, Role::Model, "|: G2 B2 d2 :|");
        cell.track = Some(TrackId::new("bass").unwrap());
        let line = tail_line(&cell).unwrap();
        assert_eq!(line.glyph, "♪");
        assert!(line.text.starts_with("bass: "), "lane prefixed: {}", line.text);
    }

    #[test]
    fn long_content_truncates_to_first_line_head() {
        let long = format!("{}\nsecond line", "x".repeat(300));
        let block = BlockSnapshot::text(bid(1), None, Role::User, long);
        let line = tail_line(&block).unwrap();
        assert!(line.text.chars().count() <= TAIL_LINE_CHARS);
        assert!(line.text.ends_with('…'), "elided: {}", line.text);
        assert!(!line.text.contains("second"), "first line only");
    }

    // ── ContextTails ──

    #[test]
    fn tail_caps_at_window_evicting_oldest_line() {
        let mut tails = ContextTails::default();
        for i in 0..(TAIL_LINES + 3) {
            tails.push(ctx(1), TailLine::new("✦", format!("line {i}")), i as f64);
        }
        let lines: Vec<_> = tails.iter_lines(&ctx(1)).collect();
        assert_eq!(lines.len(), TAIL_LINES, "window stays capped");
        assert_eq!(lines[0].text, "line 3", "oldest lines evicted");
        assert_eq!(lines.last().unwrap().text, format!("line {}", TAIL_LINES + 2));
    }

    #[test]
    fn context_cap_evicts_the_oldest_touched_tail() {
        let mut tails = ContextTails::default();
        // Fill to the cap with ascending touch times, then one more.
        for i in 0..TAIL_CONTEXT_CAP {
            tails.push(
                ContextId::from_bytes([(i % 251) as u8, (i / 251) as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
                TailLine::new("✦", "x"),
                i as f64,
            );
        }
        assert_eq!(tails.tails.len(), TAIL_CONTEXT_CAP);
        let oldest = *tails
            .tails
            .iter()
            .min_by(|a, b| a.1.touched.total_cmp(&b.1.touched))
            .map(|(id, _)| id)
            .unwrap();
        tails.push(ctx(9), TailLine::new("✦", "new"), 1e9);
        assert_eq!(tails.tails.len(), TAIL_CONTEXT_CAP, "capped");
        assert!(!tails.tails.contains_key(&oldest), "oldest-touched dropped");
        assert!(!tails.iter_lines(&ctx(9)).next().is_none(), "newcomer kept");
    }

    // ── tail_lines ──

    #[test]
    fn tail_lines_is_none_for_an_untouched_context() {
        let tails = ContextTails::default();
        assert_eq!(tail_lines(&tails, ctx(1), 3, 40), None);
    }

    #[test]
    fn tail_lines_returns_the_newest_n_oldest_to_newest() {
        let mut tails = ContextTails::default();
        for i in 0..6 {
            tails.push(ctx(1), TailLine::new("✦", format!("event {i}")), i as f64);
        }
        let joined = tail_lines(&tails, ctx(1), 3, 40).unwrap();
        let shown: Vec<&str> = joined.lines().collect();
        assert_eq!(shown.len(), 3, "newest 3 lines: {joined:?}");
        assert!(shown[0].contains("event 3"), "oldest of the kept window first: {joined:?}");
        assert!(shown[2].contains("event 5"), "newest line last: {joined:?}");
    }

    #[test]
    fn tail_lines_requesting_more_than_available_returns_them_all() {
        let mut tails = ContextTails::default();
        tails.push(ctx(1), TailLine::new("✦", "only one"), 0.0);
        let joined = tail_lines(&tails, ctx(1), 5, 40).unwrap();
        assert_eq!(joined, "✦ only one");
    }

    #[test]
    fn tail_lines_truncates_each_line_to_the_char_budget() {
        let mut tails = ContextTails::default();
        tails.push(ctx(1), TailLine::new("✦", "x".repeat(100)), 0.0);
        let joined = tail_lines(&tails, ctx(1), 3, 20).unwrap();
        assert!(joined.chars().count() <= 20, "line over budget: {joined:?}");
        assert!(joined.ends_with('…'), "elided: {joined:?}");
    }

    // ── beats ──

    #[test]
    fn envelope_peaks_on_the_beat_and_decays_through_it() {
        assert!((beat_envelope_at(0.0) - 1.0).abs() < 1e-6);
        assert!(beat_envelope_at(0.1) > beat_envelope_at(0.5));
        assert!(beat_envelope_at(0.9) < 0.05, "quiet by the next beat");
    }

    #[test]
    fn phasor_envelope_follows_position_and_reset_silences_it() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.envelope(&ctx(1), t0), 0.0, "no phasor yet");

        // 120 BPM (2 beats/sec): anchor exactly on a beat.
        beats.observe(ctx(1), BeatRef::new(8.0, 2.0), t0, t0);
        let on_beat = beats.envelope(&ctx(1), t0);
        assert!((on_beat - 1.0).abs() < 1e-3, "on the beat: {on_beat}");

        let off_beat = beats.envelope(&ctx(1), t0 + Duration::from_millis(250));
        assert!(off_beat < on_beat, "decays mid-beat: {off_beat}");

        // Next beat (500ms) peaks again.
        let next = beats.envelope(&ctx(1), t0 + Duration::from_millis(500));
        assert!(next > off_beat, "re-peaks on the next beat: {next}");

        beats.reset(&ctx(1));
        assert_eq!(beats.envelope(&ctx(1), t0), 0.0, "flush silences the pulse");
    }

    #[test]
    fn beat_position_is_some_while_rolling_and_advances() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.beat_position(&ctx(1), t0), None, "no phasor yet");

        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        let p0 = beats.beat_position(&ctx(1), t0).expect("phasor now live");
        assert!((p0 - 0.0).abs() < 1e-6, "anchored at beat 0: {p0}");

        let p1 = beats
            .beat_position(&ctx(1), t0 + Duration::from_millis(500))
            .expect("still rolling");
        assert!(p1 > p0, "advances with time: {p0} -> {p1}");
    }

    #[test]
    fn beat_position_is_none_after_reset() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        assert!(beats.beat_position(&ctx(1), t0).is_some());

        beats.reset(&ctx(1));
        assert_eq!(beats.beat_position(&ctx(1), t0), None, "flush drops the phasor");
    }

    #[test]
    fn global_envelope_is_the_loudest_track() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.global_envelope(t0), 0.0);
        // Track A anchored on-beat, track B anchored mid-beat.
        beats.observe(ctx(1), BeatRef::new(4.0, 2.0), t0, t0);
        beats.observe(ctx(2), BeatRef::new(4.5, 2.0), t0, t0);
        let g = beats.global_envelope(t0);
        let a = beats.envelope(&ctx(1), t0);
        assert!((g - a).abs() < 1e-6, "global = loudest (on-beat) track");
        assert!(beats.any_rolling());
    }

    #[test]
    fn stale_phasor_is_pruned_without_a_flush() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        beats.prune_stale(t0 + PHASOR_STALE / 2);
        assert!(beats.any_rolling(), "fresh phasor survives");
        beats.prune_stale(t0 + PHASOR_STALE + Duration::from_secs(1));
        assert!(!beats.any_rolling(), "stale phasor dropped");
    }

    /// A stale-but-received reference (`backdated_at` returned `None`) still
    /// proves the track is alive — `touch` must bump the liveness clock (so
    /// `prune_stale` doesn't reap a phasor that's still receiving, just
    /// receiving old references) WITHOUT moving the beat position (folding a
    /// stale ref would anchor the phasor in the past).
    #[test]
    fn touch_keeps_a_phasor_alive_without_moving_its_position() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0, t0);
        let p_before = beats.beat_position(&ctx(1), t0).expect("phasor live");

        // Without a touch, the phasor goes stale at PHASOR_STALE.
        let t_touch = t0 + PHASOR_STALE - Duration::from_millis(1);
        beats.touch(&ctx(1), t_touch);
        // Now well past the ORIGINAL anchor's staleness window, but only
        // just past the touch — must survive because touch reset the clock.
        beats.prune_stale(t_touch + PHASOR_STALE / 2);
        assert!(beats.any_rolling(), "touch kept the phasor alive past the original window");

        let p_after = beats.beat_position(&ctx(1), t0).expect("still live");
        assert_eq!(p_after, p_before, "touch must not move the beat position");

        // Far enough past the touch, it still eventually prunes.
        beats.prune_stale(t_touch + PHASOR_STALE + Duration::from_secs(1));
        assert!(!beats.any_rolling(), "touch delays but does not prevent eventual pruning");
    }

    /// `touch` on a context with no phasor is a no-op — it must never create
    /// one (that would anchor a fresh phasor with no beat position at all).
    #[test]
    fn touch_on_an_unknown_context_is_a_no_op() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.touch(&ctx(1), t0);
        assert!(!beats.any_rolling(), "touch must not create a phasor");
    }

    // ── Bevy wiring ──

    #[test]
    fn inserted_block_lands_in_the_context_tail() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<ContextTails>()
            .init_resource::<WellBeats>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        let block = BlockSnapshot::text(bid(7), None, Role::User, "hello well");
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BlockInserted {
            context_id: ctx(7),
            block: Box::new(block),
            ops: vec![],
        }));
        app.update();

        let tails = app.world().resource::<ContextTails>();
        let lines: Vec<_> = tails.iter_lines(&ctx(7)).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].display(), "❯ hello well");
    }

    #[test]
    fn beat_sync_anchors_and_flush_cue_drops_the_phasor() {
        use kaijutsu_audio::{CuePayload, RenderCue};

        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<ContextTails>()
            .init_resource::<WellBeats>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(3),
            beat_ref: BeatRef::new(0.0, 2.0),
        }));
        app.update();
        assert!(app.world().resource::<WellBeats>().any_rolling());

        app.world_mut().write_message(ServerEventMessage(ServerEvent::RenderCue {
            context_id: ctx(3),
            cue: RenderCue {
                mime: RENDER_FLUSH_MIME.into(),
                payload: CuePayload::Inline(vec![]),
                lead: Duration::ZERO,
            },
        }));
        app.update();
        assert!(!app.world().resource::<WellBeats>().any_rolling(), "flush drops the phasor");
    }

    /// A `BeatSync` stamped stale-old (`epoch_ns` older than `REF_STALE_MAX`)
    /// must not move an already-anchored phasor's position — `ingest_live_events`
    /// routes the `None` arm of `backdated_at` to `WellBeats::touch`, not
    /// `observe`. Wired at the system level (not just the pure `WellBeats` unit
    /// test above) to prove `ingest_live_events` actually calls `backdated_at`
    /// and branches on it, rather than always folding.
    #[test]
    fn a_stale_beat_sync_touches_liveness_without_moving_the_phasor() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<ContextTails>()
            .init_resource::<WellBeats>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        // Anchor with an unstamped (epoch_ns=0) ref first — falls back to
        // receipt time, so this frame's position is well-defined.
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(5),
            beat_ref: BeatRef::new(2.0, 2.0),
        }));
        app.update();
        let pos_before = app
            .world()
            .resource::<WellBeats>()
            .beat_position(&ctx(5), Instant::now())
            .expect("anchored");

        // A second ref, stamped 10 s old (well past REF_STALE_MAX) but with a
        // wildly different beat value — if this folded, position would jump.
        let ancient_epoch_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos() as u64
            - 10_000_000_000;
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(5),
            beat_ref: kaijutsu_audio::BeatRef { beat: 999.0, tempo_bps: 2.0, epoch_ns: ancient_epoch_ns },
        }));
        app.update();

        assert!(app.world().resource::<WellBeats>().any_rolling(), "touch keeps it alive");
        let pos_after = app
            .world()
            .resource::<WellBeats>()
            .beat_position(&ctx(5), Instant::now())
            .expect("still anchored");
        // Position should have advanced only by ordinary free-run (a couple
        // frames' worth), nowhere near the stale ref's beat=999.
        assert!(
            pos_after < pos_before + 1.0,
            "stale ref must not fold: pos_before={pos_before} pos_after={pos_after}"
        );
    }

    fn minimal_card(id: ContextId) -> super::super::scene::Card {
        super::super::scene::Card {
            context_id: id,
            data: super::super::card::CardData {
                title: "t".into(),
                accent: "coder".into(),
                model_badge: String::new(),
                fork_badge: None,
                keywords: vec![],
                preview: None,
                band: kaijutsu_viz::layout::Band::Active,
                forked_from: None,
                cluster_label: None,
                paused: false,
            },
            status: None,
            selected: false,
            in_lineage: false,
            drifting: false,
            base_scale: 1.0,
            tail: None,
        }
    }

    #[test]
    fn sync_selected_card_tail_tracks_selection_and_only_the_selected_card() {
        let mut app = App::new();
        app.init_resource::<ContextTails>()
            .init_resource::<super::super::scene::TimeWellState>()
            .add_systems(Update, sync_selected_card_tail);

        let sel = ctx(1);
        let other = ctx(2);
        app.world_mut().resource_mut::<ContextTails>().push(sel, TailLine::new("✦", "hello"), 0.0);
        app.world_mut().resource_mut::<super::super::scene::TimeWellState>().selected = Some(sel);

        let sel_entity = app.world_mut().spawn(minimal_card(sel)).id();
        let other_entity = app.world_mut().spawn(minimal_card(other)).id();

        app.update();
        assert_eq!(
            app.world().get::<super::super::scene::Card>(sel_entity).unwrap().tail.as_deref(),
            Some("✦ hello"),
            "selected card gets the tail"
        );
        assert_eq!(
            app.world().get::<super::super::scene::Card>(other_entity).unwrap().tail,
            None,
            "non-selected card stays untouched"
        );

        // Deselecting clears the previously-tailed card's band — the same
        // guarded-write pass runs over every card, not a special-cased
        // single-entity lookup, so "nothing selected" naturally clears it.
        app.world_mut().resource_mut::<super::super::scene::TimeWellState>().selected = None;
        app.update();
        assert_eq!(
            app.world().get::<super::super::scene::Card>(sel_entity).unwrap().tail,
            None,
            "deselecting clears the tail"
        );
    }

    #[test]
    fn sync_selected_card_tail_updates_as_new_lines_arrive() {
        let mut app = App::new();
        app.init_resource::<ContextTails>()
            .init_resource::<super::super::scene::TimeWellState>()
            .add_systems(Update, sync_selected_card_tail);

        let sel = ctx(1);
        app.world_mut().resource_mut::<super::super::scene::TimeWellState>().selected = Some(sel);
        let entity = app.world_mut().spawn(minimal_card(sel)).id();

        // No tail content yet — and no fallback to `data.preview` either
        // (the card face's own gist line already shows that; see
        // `tail_lines`'s own doc for why the card path skips the fallback the
        // retired HUD South panel used).
        app.update();
        assert_eq!(app.world().get::<super::super::scene::Card>(entity).unwrap().tail, None);

        app.world_mut().resource_mut::<ContextTails>().push(sel, TailLine::new("▸", "shell: ls"), 1.0);
        app.update();
        assert_eq!(
            app.world().get::<super::super::scene::Card>(entity).unwrap().tail.as_deref(),
            Some("▸ shell: ls")
        );
    }
}

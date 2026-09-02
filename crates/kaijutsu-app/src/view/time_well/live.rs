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
use std::time::Instant;

use bevy::prelude::*;
use kaijutsu_audio::{RefDisposition, RENDER_FLUSH_MIME};
use kaijutsu_present::beats::WellBeats;
use kaijutsu_client::ServerEvent;
use kaijutsu_types::BlockId;
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

        if self.tails.len() > TAIL_CONTEXT_CAP
            && let Some(oldest) = self
                .tails
                .iter()
                .min_by(|a, b| a.1.touched.total_cmp(&b.1.touched))
                .map(|(id, _)| *id)
        {
            self.tails.remove(&oldest);
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
/// Model text usually inserts **empty** and streams in via
/// `ContextChange::TextAppended` (plain text suffixes this module doesn't
/// track), so an empty model insert becomes a "⋯ composing" **placeholder**
/// tagged with its block id — the block's `Done`/`Error` flip resolves it
/// in place ([`ContextTails::resolve`]), so the tail narrates the turn
/// without holding its own copy of the streamed text (Gemini review,
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

/// The per-track phasor map as a Bevy resource.
///
/// The map itself is toolkit-free ([`kaijutsu_present::beats::WellBeats`]) so
/// the terminal client shares it; this is only the newtype the orphan rule
/// requires. `DerefMut` is safe here because nothing gates on
/// `WellBeats`'s change ticks — every reader asks it a question at the
/// current instant instead.
#[derive(Resource, Default, Deref, DerefMut)]
pub struct WellBeatsRes(pub WellBeats);

// ============================================================================
// SYSTEMS
// ============================================================================

/// Ingest the kernel-wide event stream into tails + phasors. Runs **ungated**
/// (every screen) so the well opens warm; both resources are bounded
/// ([`TAIL_CONTEXT_CAP`], one phasor per rolling track).
///
/// Each `BeatSync` is routed by its [`RefDisposition`]: `Fold` (age ≤
/// `REF_FOLD_MAX`, or unstamped) folds at its own back-dated emission instant
/// — not this frame's shared `now_inst` — the same flood-resistance fix as
/// `metronome::ingest_beat_signals`. `Touch` and `Drop` (older than
/// `REF_FOLD_MAX`, whether or not past `REF_STALE_MAX`) both still prove the
/// track alive: either bumps the phasor's liveness clock via
/// [`WellBeats::touch`] without folding a beat position from the past — this
/// is the one place `Touch` and `Drop` behave alike, because `WellBeats`
/// (unlike the single-phasor metronome) HAS a liveness clock to bump.
pub fn ingest_live_events(
    mut events: MessageReader<ServerEventMessage>,
    mut tails: ResMut<ContextTails>,
    mut beats: ResMut<WellBeatsRes>,
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
                match beat_ref.disposition(now_inst, now_epoch_ns) {
                    RefDisposition::Fold(at) => {
                        if let Some(slew) = beats.observe(*context_id, *beat_ref, at, now_inst) {
                            kaijutsu_telemetry::record_phasor_slew(
                                "time_well",
                                slew.error_beats,
                                slew.deadbanded,
                            );
                        }
                    }
                    RefDisposition::Touch | RefDisposition::Drop => {
                        beats.touch(context_id, now_inst)
                    }
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
    beats: Res<WellBeatsRes>,
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
            && let Some(mut mat) = materials.get_mut(&handle.0)
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
    use std::time::Duration;
    use kaijutsu_audio::BeatRef;
    use kaijutsu_types::BlockId;
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
        assert!(tails.iter_lines(&ctx(9)).next().is_some(), "newcomer kept");
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

    // ── Bevy wiring ──

    #[test]
    fn inserted_block_lands_in_the_context_tail() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<ContextTails>()
            .init_resource::<WellBeatsRes>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        let block = BlockSnapshot::text(bid(7), None, Role::User, "hello well");
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BlockInserted {
            context_id: ctx(7),
            block: Box::new(block),
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
            .init_resource::<WellBeatsRes>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(3),
            beat_ref: BeatRef::new(0.0, 2.0),
        }));
        app.update();
        assert!(app.world().resource::<WellBeatsRes>().any_rolling());

        app.world_mut().write_message(ServerEventMessage(ServerEvent::RenderCue {
            context_id: ctx(3),
            cue: RenderCue {
                mime: RENDER_FLUSH_MIME.into(),
                payload: CuePayload::Inline(vec![]),
                lead: Duration::ZERO,
                epoch_ns: 0,
                onset_beat: None,
            },
        }));
        app.update();
        assert!(!app.world().resource::<WellBeatsRes>().any_rolling(), "flush drops the phasor");
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
            .init_resource::<WellBeatsRes>()
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
            .resource::<WellBeatsRes>()
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

        assert!(app.world().resource::<WellBeatsRes>().any_rolling(), "touch keeps it alive");
        let pos_after = app
            .world()
            .resource::<WellBeatsRes>()
            .beat_position(&ctx(5), Instant::now())
            .expect("still anchored");
        // Position should have advanced only by ordinary free-run (a couple
        // frames' worth), nowhere near the stale ref's beat=999.
        assert!(
            pos_after < pos_before + 1.0,
            "stale ref must not fold: pos_before={pos_before} pos_after={pos_after}"
        );
    }

    /// A `BeatSync` in the `Touch` band (older than `REF_FOLD_MAX` but within
    /// `REF_STALE_MAX`) must ALSO touch, not fold — the middle rung of the
    /// disposition ladder that `Drop` alone (the test above) doesn't exercise.
    /// `WellBeats` treats `Touch` and `Drop` alike (unlike the metronome,
    /// which has no liveness clock to bump for either).
    #[test]
    fn a_touch_band_beat_sync_also_touches_without_folding() {
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<ContextTails>()
            .init_resource::<WellBeatsRes>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_live_events);

        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(6),
            beat_ref: BeatRef::new(2.0, 2.0),
        }));
        app.update();
        let pos_before = app
            .world()
            .resource::<WellBeatsRes>()
            .beat_position(&ctx(6), Instant::now())
            .expect("anchored");

        // 2 s old: past REF_FOLD_MAX (1s), within REF_STALE_MAX (5s) — Touch.
        let touch_epoch_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos() as u64
            - 2_000_000_000;
        app.world_mut().write_message(ServerEventMessage(ServerEvent::BeatSync {
            context_id: ctx(6),
            beat_ref: kaijutsu_audio::BeatRef { beat: 999.0, tempo_bps: 2.0, epoch_ns: touch_epoch_ns },
        }));
        app.update();

        assert!(app.world().resource::<WellBeatsRes>().any_rolling(), "Touch keeps it alive too");
        let pos_after = app
            .world()
            .resource::<WellBeatsRes>()
            .beat_position(&ctx(6), Instant::now())
            .expect("still anchored");
        assert!(
            pos_after < pos_before + 1.0,
            "a Touch-band ref must not fold either: pos_before={pos_before} pos_after={pos_after}"
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

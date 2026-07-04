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
//! - The HUD South panel renders the selected card's tail (see `super::hud`).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use kaijutsu_audio::{BeatRef, LocalBeat, RENDER_FLUSH_MIME};
use kaijutsu_client::ServerEvent;
use kaijutsu_types::{BlockKind, BlockSnapshot, ContextId, Role};

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
}

impl TailLine {
    /// The display form the HUD renders: `glyph text`.
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
}

/// Head of the first non-empty content line, truncated to
/// [`TAIL_LINE_CHARS`]. `None` when there is no visible text at all.
fn head_line(content: &str) -> Option<String> {
    let line = content.lines().find(|l| !l.trim().is_empty())?;
    Some(crate::text::truncate_chars(line.trim(), TAIL_LINE_CHARS))
}

/// Map an inserted block to a tail line, or `None` for blocks that carry no
/// glanceable signal (empty streaming inserts, thinking, structural kinds).
///
/// Model text usually inserts **empty** and streams in via `BlockTextOps`
/// (CRDT deltas this module doesn't decode), so a live turn shows up as the
/// chatter glow + running rim rather than a tail line; the tail catches the
/// blocks that arrive whole — user prompts, tool calls, results, errors,
/// notifications, and materialized score cells (tagged with their track).
pub fn tail_line(block: &BlockSnapshot) -> Option<TailLine> {
    match block.kind {
        BlockKind::Text => {
            let head = head_line(&block.content)?;
            // A materialized score cell carries its lane — show it.
            if let Some(track) = &block.track {
                return Some(TailLine {
                    glyph: "♪",
                    text: format!("{}: {}", track.as_str(), head),
                });
            }
            let glyph = if block.role == Role::User { "❯" } else { "✦" };
            Some(TailLine { glyph, text: head })
        }
        BlockKind::ToolCall => {
            // The tool name is the signal; the input JSON body is noise.
            let name = block.tool_name.as_deref().unwrap_or("tool");
            Some(TailLine {
                glyph: "▸",
                text: crate::text::truncate_chars(name, TAIL_LINE_CHARS),
            })
        }
        BlockKind::ToolResult => {
            let head = head_line(&block.content)?;
            let glyph = if block.is_error { "✕" } else { "◂" };
            Some(TailLine { glyph, text: head })
        }
        BlockKind::Error => Some(TailLine {
            glyph: "✕",
            text: head_line(&block.content).unwrap_or_else(|| "error".into()),
        }),
        BlockKind::Drift => {
            let head = head_line(&block.content)?;
            Some(TailLine { glyph: "≈", text: head })
        }
        BlockKind::Notification => {
            let head = head_line(&block.content)?;
            Some(TailLine { glyph: "◆", text: head })
        }
        BlockKind::File => {
            let text = block
                .file_path
                .clone()
                .or_else(|| head_line(&block.content))?;
            Some(TailLine {
                glyph: "⎘",
                text: crate::text::truncate_chars(&text, TAIL_LINE_CHARS),
            })
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
    pub fn observe(&mut self, ctx: ContextId, reference: BeatRef, at: Instant) {
        match self.phasors.get_mut(&ctx) {
            Some(p) => {
                p.beat.observe(reference, at);
                p.last_ref = at;
            }
            None => {
                self.phasors.insert(
                    ctx,
                    Phasor { beat: LocalBeat::new(reference, at), last_ref: at },
                );
            }
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
        self.phasors
            .get(ctx)
            .map(|p| {
                let pos = p.beat.position(now);
                beat_envelope_at(pos - pos.floor())
            })
            .unwrap_or(0.0)
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

    /// Whether any track's clock is rolling (has a live phasor). Test-only
    /// today; the HUD's track readout (Slice 3) is the intended consumer.
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
pub fn ingest_live_events(
    mut events: MessageReader<ServerEventMessage>,
    mut tails: ResMut<ContextTails>,
    mut beats: ResMut<WellBeats>,
    time: Res<Time>,
) {
    let now_inst = Instant::now();
    let now = time.elapsed_secs_f64();
    for ServerEventMessage(ev) in events.read() {
        match ev {
            ServerEvent::BlockInserted { context_id, block, .. } => {
                if let Some(line) = tail_line(block) {
                    tails.push(*context_id, line, now);
                }
            }
            ServerEvent::BeatSync { context_id, beat_ref } => {
                beats.observe(*context_id, *beat_ref, now_inst);
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

/// Push each card's live lanes into its material: `dim.y` = chatter (the
/// context's decaying event energy), `dim.z` = beat envelope (score-context
/// cards while their track rolls). Values are quantized and change-guarded so
/// a quiet card never touches `Assets<WellCardMaterial>` (same discipline as
/// `scene::dim_nonfocused_rings`).
pub fn sync_card_live_uniforms(
    activity: Res<super::activity::RingActivity>,
    beats: Res<WellBeats>,
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
        let beat = quantize(beats.envelope(&card.context_id, now));
        // Read via the non-dirtying `get`; only reach for `get_mut` on change.
        let Some(cur) = materials.get(&handle.0).map(|m| (m.dim.y, m.dim.z)) else {
            continue;
        };
        if cur != (chatter, beat)
            && let Some(mat) = materials.get_mut(&handle.0)
        {
            mat.dim.y = chatter;
            mat.dim.z = beat;
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
    fn empty_streaming_insert_yields_no_line() {
        // Model turns insert empty then stream via TextOps — no tail line;
        // the chatter glow carries that signal instead.
        let empty = BlockSnapshot::text(bid(1), None, Role::Model, "");
        assert!(tail_line(&empty).is_none());
        let blank = BlockSnapshot::text(bid(1), None, Role::Model, "  \n\t\n");
        assert!(tail_line(&blank).is_none());
    }

    #[test]
    fn tool_call_shows_the_tool_name_not_the_input_json() {
        let call = BlockSnapshot::tool_call(
            bid(1),
            None,
            ToolKind::Builtin,
            "kaijutsu:read",
            serde_json::json!({"path": "/etc/rc"}),
            Role::Model,
            None,
        );
        let line = tail_line(&call).unwrap();
        assert_eq!(line.glyph, "▸");
        assert_eq!(line.text, "kaijutsu:read");
        assert!(!line.text.contains('{'), "input JSON stays out of the tail");
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
            tails.push(ctx(1), TailLine { glyph: "✦", text: format!("line {i}") }, i as f64);
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
                TailLine { glyph: "✦", text: "x".into() },
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
        tails.push(ctx(9), TailLine { glyph: "✦", text: "new".into() }, 1e9);
        assert_eq!(tails.tails.len(), TAIL_CONTEXT_CAP, "capped");
        assert!(!tails.tails.contains_key(&oldest), "oldest-touched dropped");
        assert!(!tails.iter_lines(&ctx(9)).next().is_none(), "newcomer kept");
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
        beats.observe(ctx(1), BeatRef::new(8.0, 2.0), t0);
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
    fn global_envelope_is_the_loudest_track() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        assert_eq!(beats.global_envelope(t0), 0.0);
        // Track A anchored on-beat, track B anchored mid-beat.
        beats.observe(ctx(1), BeatRef::new(4.0, 2.0), t0);
        beats.observe(ctx(2), BeatRef::new(4.5, 2.0), t0);
        let g = beats.global_envelope(t0);
        let a = beats.envelope(&ctx(1), t0);
        assert!((g - a).abs() < 1e-6, "global = loudest (on-beat) track");
        assert!(beats.any_rolling());
    }

    #[test]
    fn stale_phasor_is_pruned_without_a_flush() {
        let mut beats = WellBeats::default();
        let t0 = Instant::now();
        beats.observe(ctx(1), BeatRef::new(0.0, 2.0), t0);
        beats.prune_stale(t0 + PHASOR_STALE / 2);
        assert!(beats.any_rolling(), "fresh phasor survives");
        beats.prune_stale(t0 + PHASOR_STALE + Duration::from_secs(1));
        assert!(!beats.any_rolling(), "stale phasor dropped");
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
}

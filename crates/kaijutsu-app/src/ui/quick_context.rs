//! The quick-context overlay — the Ctrl+A prefix's own panel.
//!
//! Arming the prefix does two things now: the footer shows the chord table
//! (`ui::dock::update_hints`) and this panel *peeks*. It is the answer to
//! "what is going on right now" that you get for free while deciding which
//! chord to press, and it vanishes with the pending state — resolved,
//! cancelled, or timed out — unless you hold it with `Ctrl+A h`. Held, it
//! stops being translucent and stays until you release it (`Ctrl+A h` again,
//! or Esc).
//!
//! ## Sections, not a panel
//!
//! The content is a `Vec<`[`PanelSection`]`>` because this surface is
//! expected to accrete: the roster is the first section, not the panel's
//! identity. A new section is a function returning a [`PanelSection`] plus
//! one line in [`build_sections`] — deliberately not a registry, a trait, or
//! a plugin hook, none of which the second section will need either.
//!
//! ## Rendering
//!
//! One MSDF render-texture surface, the same machinery the docks use
//! (`ui::dock`'s module doc): glyphs collected into `MsdfBlockGlyphs`, the
//! texture composited by a `BlockFxMaterial`, sizing handled by
//! `view::block_render::resize_block_textures` via
//! [`msdf_surface_bundle`](crate::view::block_render::msdf_surface_bundle).
//! Chosen over Bevy `Text` nodes (which the app does not use for chrome
//! anywhere) and over `view::overlay`'s compose-surface path (that one is
//! built around a single editable text run with a cursor — this panel is
//! many short runs in several colors, which is exactly the dock's shape).
//! The dock's own glyph-collection and measurement helpers are reused rather
//! than copied.
//!
//! ## Honesty
//!
//! Everything the panel shows about the roster is state the kernel actually
//! published, including the states where it published nothing: see
//! `connection::roster`'s module doc. An empty body under a "ROSTER" heading
//! is never allowed to mean four different things at once, so the footer
//! always says which one it is.

use bevy::prelude::*;

use crate::connection::roster::{LivenessKind, RosterFeed, RosterFetch, RosterRow};
use crate::input::events::ActionFired;
use crate::input::prefix::PrefixState;
use crate::input::Action;
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph};
use crate::text::shaping::VelloFont;
use crate::text::{bevy_color_to_brush, ShapingFonts};
use crate::ui::dock::{collect_dock_text_glyphs, measure_text as measure_dock_text};
use crate::ui::theme::Theme;
use crate::view::block_render::msdf_surface_bundle;
use crate::view::ui_rtt::UiRttTexture;

/// Body-row font size (matches the dock's small widgets).
const ROW_FONT_SIZE: f32 = 13.0;
/// Section-heading font size.
const HEAD_FONT_SIZE: f32 = 12.0;
/// Baseline-to-baseline spacing, logical px.
const LINE_HEIGHT: f64 = 18.0;
/// Inner padding, logical px.
const PAD: f64 = 12.0;
/// Width clamps — narrow enough to sit beside content, wide enough for a
/// status line worth reading.
const MIN_WIDTH: f64 = 260.0;
const MAX_WIDTH: f64 = 520.0;
/// Longest status/error tail we render before eliding. A wall of text in a
/// transient peek is unreadable; the full text is in the log either way.
const MAX_DETAIL_CHARS: usize = 72;

/// Most roster rows drawn before the panel switches to a count. The panel
/// sizes itself to its content and nothing clips it, so an unbounded fleet
/// would grow it straight off the bottom of the window. The overflow is
/// stated (`+N more`), never silently dropped — and because rows arrive
/// grouped by liveness kind, the ones that survive the cut are the confident
/// ones (`bound` before `recent` before `attested`), which is the right way
/// round for a glance.
const MAX_ROSTER_ROWS: usize = 20;

/// Text alpha while peeking vs held. The peek is deliberately ghosted —
/// it is transient chrome you are looking *past* on the way to a chord.
const PEEK_TEXT_ALPHA: f32 = 0.62;
const PEEK_BG_ALPHA: f32 = 0.55;
const HELD_BG_ALPHA: f32 = 0.94;

/// How often the panel rebuilds while nothing else changed. Ages ("◐ 4m",
/// "fetched 3s ago") are the only thing that moves on their own, and they
/// move at second resolution at best.
const AGE_REBUILD_INTERVAL: f64 = 1.0;

// ============================================================================
// STATE
// ============================================================================

/// Whether the overlay is up, and why.
///
/// `armed` mirrors [`PrefixState::armed`] — read from the resource, never
/// from raw input (the action table is the only key path; docs/input.md).
/// `held` is the `Ctrl+A h` latch.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct QuickContextState {
    /// The Ctrl+A prefix is pending — the panel peeks.
    pub armed: bool,
    /// `Ctrl+A h` — the panel stays after the prefix clears, and solidifies.
    pub held: bool,
}

impl QuickContextState {
    /// On screen at all.
    pub fn visible(&self) -> bool {
        self.held || self.armed
    }

    /// On screen *transiently* — translucent, about to go.
    pub fn peeking(&self) -> bool {
        self.armed && !self.held
    }
}

/// Marker for the overlay's positioned container.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct QuickContextPanel;

/// Marker for the MSDF text surface inside the container.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
pub struct QuickContextSurface;

// ============================================================================
// CONTENT MODEL (pure — no Bevy, no RPC)
// ============================================================================

/// What a line means, which is what decides its color. Kept semantic rather
/// than literal so the panel picks up a theme change for free and adds no
/// `ThemeData` fields of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTone {
    /// A section heading.
    Head,
    /// An ordinary entry.
    Row,
    /// Present but de-emphasized — a row the kernel says is not live, or a
    /// footer.
    Dim,
    /// Something the reader should notice but that is not a failure: a
    /// kernel with no roster, rows we could not parse.
    Warn,
    /// A failure.
    Bad,
}

/// One rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelLine {
    pub text: String,
    pub tone: LineTone,
}

impl PanelLine {
    fn new(text: impl Into<String>, tone: LineTone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// A titled group of lines. The unit a future quick-context section adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelSection {
    pub title: String,
    pub lines: Vec<PanelLine>,
}

/// Everything the panel shows, in order. One section today; adding the next
/// one is a `push` here.
pub fn build_sections(feed: &RosterFeed, now_ms: i64, now_secs: f64) -> Vec<PanelSection> {
    vec![roster_section(feed, now_ms, now_secs)]
}

/// The roster section: rows grouped by how the kernel knows they are live,
/// then a footer that always states which fetch state produced this body.
pub fn roster_section(feed: &RosterFeed, now_ms: i64, now_secs: f64) -> PanelSection {
    let mut lines = Vec::new();

    // The rows arrive already grouped and sorted (`parse_roster_index`), so
    // this is a straight walk — capped, with the remainder counted rather
    // than dropped (see `MAX_ROSTER_ROWS`).
    for row in feed.rows.iter().take(MAX_ROSTER_ROWS) {
        lines.push(row_line(row, now_ms));
    }
    if let Some(hidden) = feed.rows.len().checked_sub(MAX_ROSTER_ROWS).filter(|n| *n > 0) {
        lines.push(PanelLine::new(format!("+{hidden} more"), LineTone::Dim));
    }

    if feed.rows.is_empty() && feed.is_fresh() {
        // A fresh read that found nobody is a real answer, and says so —
        // this line is what stops an empty body from being ambiguous.
        lines.push(PanelLine::new("nobody around", LineTone::Dim));
    }

    lines.extend(footer_lines(feed, now_secs));

    PanelSection {
        title: "ROSTER".to_string(),
        lines,
    }
}

/// One roster row as a line.
///
/// `● amy [active] at the keys @moltar`
/// `◐ 4m context:0011223`
/// `◇ claude-code (stale)`
///
/// A `recent` row ALWAYS carries its age: "inferred from activity" is only
/// meaningful with the activity's age attached, and an age we do not have is
/// `?`, not a silently omitted one.
///
/// **The age subtracts two clocks, knowingly.** `row.recorded_at` is the
/// *kernel's* wall clock; `now_ms` is *ours*. `docs/midi.md` "The one
/// timebase" is why the kernel stamps `recorded_at` itself rather than
/// trusting a source's clock — but that discipline ends at the kernel, and
/// nothing on the wire tells a client what the kernel thinks the time is, so
/// a client-rendered delta has no way to be skew-free today. We render it
/// anyway on the assumption that kaijutsu's machines share an NTP-disciplined
/// LAN, which bounds the error far below the resolution this display uses
/// (`format_age` is coarse from a minute up, and clamps a negative delta to
/// `now`). An age like "4m" is therefore ±seconds, not exact. The fix —
/// giving the index a kernel-now reference — is filed in docs/issues.md.
pub fn row_line(row: &RosterRow, now_ms: i64) -> PanelLine {
    let kind = row.liveness_kind.as_ref();
    let mut text = String::new();

    text.push_str(kind.map(|k| k.glyph()).unwrap_or("?"));
    if matches!(kind, Some(LivenessKind::Recent)) {
        text.push(' ');
        text.push_str(&match row.recorded_at {
            Some(at) => format_age(now_ms.saturating_sub(at)),
            None => "?".to_string(),
        });
    }
    text.push(' ');
    text.push_str(&row.display_label());

    if let Some(av) = &row.availability {
        text.push_str(&format!(" [{}]", av.chip()));
    }
    if let Some(status) = &row.status_text {
        text.push_str(&format!(" {}", elide(status)));
    }
    if let Some(host) = &row.host {
        text.push_str(&format!(" @{host}"));
    }

    // Liveness itself, last, because it changes how the whole line reads.
    let tone = match row.live {
        Some(true) => LineTone::Row,
        Some(false) => {
            // A `bound` row going false means a connection was observed to
            // close — that entity is *gone*. Any other kind going false
            // means the kernel could not re-confirm it: stale, not dead.
            text.push_str(match kind {
                Some(LivenessKind::Bound) => " (gone)",
                _ => " (stale)",
            });
            LineTone::Dim
        }
        // Unknown is unknown. Not dim (that would read as "not here"), not
        // absent (the row exists) — marked, and kept at full weight.
        None => {
            text.push_str(" (?)");
            LineTone::Row
        }
    };

    PanelLine::new(text, tone)
}

/// The footer: how old this picture is, how much of it we could not read,
/// and — when it applies — why there is no picture at all.
pub fn footer_lines(feed: &RosterFeed, now_secs: f64) -> Vec<PanelLine> {
    let mut out = Vec::new();
    match &feed.state {
        RosterFetch::Never => {
            out.push(PanelLine::new("never fetched", LineTone::Warn));
        }
        RosterFetch::Fresh => {
            let age = match feed.age_secs(now_secs) {
                Some(secs) => format!("fetched {} ago", format_age((secs * 1000.0) as i64)),
                // Fresh without a success stamp cannot happen through
                // `drain_roster_index`; say so rather than inventing an age.
                None => "fetched (age unknown)".to_string(),
            };
            out.push(PanelLine::new(age, LineTone::Dim));
        }
        RosterFetch::NoRoster { .. } => {
            out.push(PanelLine::new(
                "no roster on this kernel",
                LineTone::Warn,
            ));
        }
        RosterFetch::Error { detail } => {
            out.push(PanelLine::new(
                format!("error: {}", elide(detail)),
                LineTone::Bad,
            ));
        }
    }
    if feed.unparsed > 0 {
        out.push(PanelLine::new(
            format!("{} unparsed rows", feed.unparsed),
            LineTone::Warn,
        ));
    }
    out
}

/// A duration in milliseconds as a short age: `now`, `42s`, `4m`, `3h`, `2d`.
///
/// A negative delta means the kernel's clock is ahead of ours. The kernel is
/// the one timebase for freshness (`docs/midi.md`), so we do not try to
/// correct for it and we do not render a negative age — it clamps to `now`,
/// which is what a few hundred milliseconds of skew actually means.
pub fn format_age(delta_ms: i64) -> String {
    let secs = delta_ms.max(0) / 1000;
    if secs < 1 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Shorten a kernel-supplied string for a single line. Elision is marked, so
/// a truncated status never passes for the whole one.
fn elide(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if cleaned.chars().count() <= MAX_DETAIL_CHARS {
        return cleaned;
    }
    let head: String = cleaned.chars().take(MAX_DETAIL_CHARS - 1).collect();
    format!("{head}\u{2026}")
}

// ============================================================================
// SYSTEMS
// ============================================================================

/// Mirror the prefix machine into [`QuickContextState::armed`].
///
/// Reads the `PrefixState` RESOURCE, never the keyboard: arming, resolving
/// and the timeout are all the dispatcher's business, and this only follows
/// what it decided. That also makes the panel inherit `PREFIX_TIMEOUT_MS`
/// for free rather than keeping a second copy of it.
pub fn sync_quick_context_arm(prefix: Res<PrefixState>, mut state: ResMut<QuickContextState>) {
    let armed = prefix.armed();
    if state.armed != armed {
        state.armed = armed;
    }
}

/// `Ctrl+A h` latches the panel up; Esc (via the overlay's own context)
/// releases it.
pub fn handle_quick_context_actions(
    mut actions: MessageReader<ActionFired>,
    mut state: ResMut<QuickContextState>,
) {
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::HoldQuickContext => state.held = !state.held,
            Action::UnpinQuickContext => {
                if state.held {
                    state.held = false;
                }
            }
            _ => {}
        }
    }
}

/// Show/hide the container. Children inherit, so this is the one visibility
/// write.
pub fn sync_quick_context_visibility(
    state: Res<QuickContextState>,
    mut panels: Query<&mut Visibility, With<QuickContextPanel>>,
) {
    if !state.is_changed() {
        return;
    }
    let want = if state.visible() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in panels.iter_mut() {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Spawn the overlay, hidden, as a child of the tiling root — the same
/// parent the docks use, so it lives in the app's UI tree rather than a
/// second root. `position_type: Absolute` takes it out of the column's flow;
/// it floats at the top-right, clear of the north dock.
pub fn spawn_quick_context(
    mut commands: Commands,
    theme: Res<Theme>,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    tiling_root: Query<Entity, With<super::tiling_reconciler::TilingRoot>>,
    existing: Query<Entity, With<QuickContextPanel>>,
) {
    if !existing.is_empty() {
        return;
    }
    let Ok(root) = tiling_root.single() else {
        return;
    };

    let material = fx_materials.add(BlockFxMaterial::default());

    let panel = commands
        .spawn((
            QuickContextPanel,
            Node {
                position_type: PositionType::Absolute,
                // Clear of the 40px north dock.
                top: Val::Px(52.0),
                right: Val::Px(16.0),
                border: UiRect::all(Val::Px(1.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(theme.panel_bg.with_alpha(PEEK_BG_ALPHA)),
            BorderColor::all(theme.border),
            GlobalZIndex(crate::constants::ZLayer::QUICK_CONTEXT),
            Visibility::Hidden,
        ))
        .with_children(|parent| {
            parent.spawn((
                QuickContextSurface,
                msdf_surface_bundle(material),
                Node {
                    width: Val::Px(MIN_WIDTH as f32),
                    height: Val::Px(LINE_HEIGHT as f32),
                    ..default()
                },
            ));
        })
        .id();
    commands.entity(root).add_child(panel);
}

/// Rebuild the panel's glyphs.
///
/// Runs in `PostUpdate` after layout and before `resize_block_textures`,
/// which allocates the render texture from the `UiRttTexture` dimensions set
/// here — the same contract every hand-rolled MSDF surface honors.
///
/// Skips entirely while hidden (a peek that is not up costs nothing), and
/// while up rebuilds on a real change or the age tick.
#[allow(clippy::too_many_arguments)]
pub fn render_quick_context(
    state: Res<QuickContextState>,
    feed: Res<RosterFeed>,
    theme: Res<Theme>,
    time: Res<Time>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut surface: Query<(&mut MsdfBlockGlyphs, &mut UiRttTexture, &mut Node), With<QuickContextSurface>>,
    mut panel: Query<&mut BackgroundColor, With<QuickContextPanel>>,
    mut last_build: Local<f64>,
) {
    if !state.visible() {
        return;
    }
    let now_secs = time.elapsed_secs_f64();
    let stale = now_secs - *last_build >= AGE_REBUILD_INTERVAL;
    if !state.is_changed() && !feed.is_changed() && !theme.is_changed() && !stale {
        return;
    }

    let Ok((mut glyphs_out, mut rtt, mut node)) = surface.single_mut() else {
        return;
    };
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(ref mut atlas) = atlas else {
        return;
    };
    *last_build = now_secs;

    let alpha = if state.peeking() { PEEK_TEXT_ALPHA } else { 1.0 };
    let sections = build_sections(&feed, kaijutsu_types::now_millis() as i64, now_secs);

    // Measure first: the panel is as wide as its widest line, clamped.
    let mut content_w: f64 = 0.0;
    for section in &sections {
        content_w = content_w.max(measure_dock_text(&section.title, HEAD_FONT_SIZE, font));
        for line in &section.lines {
            content_w = content_w.max(measure_dock_text(&line.text, ROW_FONT_SIZE, font));
        }
    }
    let width = (content_w + PAD * 2.0).clamp(MIN_WIDTH, MAX_WIDTH);

    let mut glyphs: Vec<PositionedGlyph> = Vec::new();
    let mut y = PAD;
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            y += LINE_HEIGHT * 0.5;
        }
        let head_brush = bevy_color_to_brush(tone_color(LineTone::Head, &theme).with_alpha(alpha));
        collect_dock_text_glyphs(
            &mut glyphs,
            &section.title,
            PAD,
            y,
            HEAD_FONT_SIZE,
            font,
            &head_brush,
            atlas,
            &mut font_data_map,
        );
        y += LINE_HEIGHT;

        for line in &section.lines {
            let brush = bevy_color_to_brush(tone_color(line.tone, &theme).with_alpha(alpha));
            collect_dock_text_glyphs(
                &mut glyphs,
                &line.text,
                PAD,
                y,
                ROW_FONT_SIZE,
                font,
                &brush,
                atlas,
                &mut font_data_map,
            );
            y += LINE_HEIGHT;
        }
    }
    let height = y + PAD - LINE_HEIGHT * 0.25;

    glyphs_out.glyphs = glyphs;
    glyphs_out.version = glyphs_out.version.wrapping_add(1).max(1);
    rtt.built_width = width as f32;
    rtt.built_height = height as f32;
    node.width = Val::Px(width as f32);
    node.height = Val::Px(height as f32);

    if let Ok(mut bg) = panel.single_mut() {
        let bg_alpha = if state.peeking() {
            PEEK_BG_ALPHA
        } else {
            HELD_BG_ALPHA
        };
        bg.0 = theme.panel_bg.with_alpha(bg_alpha);
    }
}

/// Semantic tone → an existing theme color. No new `ThemeData` fields: the
/// panel borrows the same palette the dock chrome already uses, so a theme
/// that never heard of this overlay still styles it correctly.
fn tone_color(tone: LineTone, theme: &Theme) -> Color {
    match tone {
        LineTone::Head => theme.accent,
        LineTone::Row => theme.fg,
        LineTone::Dim => theme.fg_dim,
        LineTone::Warn => theme.warning,
        LineTone::Bad => theme.error,
    }
}

// ============================================================================
// PLUGIN
// ============================================================================

pub struct QuickContextPlugin;

impl Plugin for QuickContextPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<QuickContextState>()
            .register_type::<QuickContextState>()
            .register_type::<QuickContextPanel>()
            .register_type::<QuickContextSurface>()
            // PostStartup, like the docks: TilingRoot is spawned in Startup.
            .add_systems(PostStartup, spawn_quick_context)
            .add_systems(
                Update,
                (
                    // The arm mirror runs first so a chord that resolves in
                    // the same frame (which disarms the prefix) cannot beat
                    // the hold latch and blink the panel off.
                    sync_quick_context_arm,
                    handle_quick_context_actions,
                    sync_quick_context_visibility,
                )
                    .chain()
                    .in_set(crate::input::InputPhase::Handle),
            )
            .add_systems(
                PostUpdate,
                render_quick_context
                    .after(bevy::ui::UiSystems::Layout)
                    .before(crate::view::block_render::resize_block_textures),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::roster::{parse_roster_index, Availability, ParsedIndex};

    fn feed_with(doc: &str) -> RosterFeed {
        let mut feed = RosterFeed::default();
        let parsed = parse_roster_index(doc).expect("test document parses");
        feed.set_for_test(parsed, RosterFetch::Fresh, 100.0);
        feed
    }

    fn header() -> &'static str {
        "entity_kind\tentity_id\tlabel\tliveness_kind\tlive\thost\tstatus_text\tavailability\trecorded_at"
    }

    // ── visibility follows the chord timing ───────────────────────────────

    /// The panel is the prefix's panel: armed shows it, disarmed hides it,
    /// and nothing else is needed to make that true.
    #[test]
    fn arming_the_prefix_peeks_and_disarming_ends_it() {
        let mut app = App::new();
        app.init_resource::<PrefixState>()
            .init_resource::<QuickContextState>()
            .add_systems(Update, sync_quick_context_arm);

        app.update();
        assert!(!app.world().resource::<QuickContextState>().visible());

        app.world_mut().resource_mut::<PrefixState>().arm();
        app.update();
        let state = app.world().resource::<QuickContextState>();
        assert!(state.visible());
        assert!(state.peeking(), "an unheld peek is translucent");

        // Resolving a chord (or Esc, or the timeout) disarms the prefix —
        // the panel must follow it down without its own timer.
        app.world_mut().resource_mut::<PrefixState>().disarm();
        app.update();
        assert!(!app.world().resource::<QuickContextState>().visible());
    }

    /// The prefix's own timeout is the panel's timeout — one constant, not
    /// two that can drift apart.
    #[test]
    fn the_peek_ends_when_the_prefix_times_out() {
        let mut app = App::new();
        app.init_resource::<PrefixState>()
            .init_resource::<QuickContextState>()
            .add_systems(Update, sync_quick_context_arm);

        app.world_mut().resource_mut::<PrefixState>().arm();
        app.update();
        assert!(app.world().resource::<QuickContextState>().visible());

        {
            let mut prefix = app.world_mut().resource_mut::<PrefixState>();
            // The dispatcher ticks the timeout at the top of its own loop.
            std::thread::sleep(std::time::Duration::from_millis(
                crate::input::prefix::PREFIX_TIMEOUT_MS as u64 + 5,
            ));
            assert!(prefix.tick_timeout());
        }
        app.update();
        assert!(!app.world().resource::<QuickContextState>().visible());
    }

    // ── the hold latch ────────────────────────────────────────────────────

    fn hold_app() -> App {
        let mut app = App::new();
        app.init_resource::<PrefixState>()
            .init_resource::<QuickContextState>()
            .add_message::<ActionFired>()
            .add_systems(
                Update,
                (sync_quick_context_arm, handle_quick_context_actions).chain(),
            );
        app
    }

    fn fire(app: &mut App, action: Action) {
        app.world_mut()
            .resource_mut::<Messages<ActionFired>>()
            .write(ActionFired::new(action, crate::input::InputContext::Global));
    }

    /// `Ctrl+A h` fires while the prefix is armed and the prefix disarms in
    /// the same breath — the panel must survive that, which is the whole
    /// point of the latch.
    #[test]
    fn holding_survives_the_prefix_clearing() {
        let mut app = hold_app();
        app.world_mut().resource_mut::<PrefixState>().arm();
        app.update();
        assert!(app.world().resource::<QuickContextState>().peeking());

        // Resolving the chord disarms the prefix and emits the action.
        app.world_mut().resource_mut::<PrefixState>().disarm();
        fire(&mut app, Action::HoldQuickContext);
        app.update();

        let state = app.world().resource::<QuickContextState>();
        assert!(state.held);
        assert!(state.visible(), "held after the prefix cleared");
        assert!(!state.peeking(), "held is solid, not a peek");
    }

    /// `Ctrl+A h` again releases it — the chord is a latch, not a one-way
    /// door.
    #[test]
    fn holding_again_releases() {
        let mut app = hold_app();
        fire(&mut app, Action::HoldQuickContext);
        app.update();
        assert!(app.world().resource::<QuickContextState>().held);

        fire(&mut app, Action::HoldQuickContext);
        app.update();
        assert!(!app.world().resource::<QuickContextState>().visible());
    }

    /// Esc reaches this handler as `UnpinQuickContext` (the overlay's own
    /// context claims the key — see `input::context`), and releases the
    /// hold and nothing else.
    #[test]
    fn escape_releases_the_hold() {
        let mut app = hold_app();
        fire(&mut app, Action::HoldQuickContext);
        app.update();
        assert!(app.world().resource::<QuickContextState>().held);

        fire(&mut app, Action::UnpinQuickContext);
        app.update();
        assert!(!app.world().resource::<QuickContextState>().held);
        assert!(!app.world().resource::<QuickContextState>().visible());
    }

    /// An unheld panel gets no Esc handling at all: the overlay's context is
    /// only active while held, so Esc still belongs to the surface below.
    #[test]
    fn releasing_an_unheld_panel_is_a_no_op() {
        let mut app = hold_app();
        fire(&mut app, Action::UnpinQuickContext);
        app.update();
        assert!(!app.world().resource::<QuickContextState>().held);
    }

    // ── age formatting ────────────────────────────────────────────────────

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(format_age(0), "now");
        assert_eq!(format_age(999), "now");
        assert_eq!(format_age(1_000), "1s");
        assert_eq!(format_age(42_000), "42s");
        assert_eq!(format_age(60_000), "1m");
        assert_eq!(format_age(4 * 60_000), "4m");
        assert_eq!(format_age(3 * 3_600_000), "3h");
        assert_eq!(format_age(2 * 86_400_000), "2d");
    }

    /// Clock skew (the kernel's stamp ahead of ours) must not render a
    /// negative age. The kernel is the timebase; a small lead means "now".
    #[test]
    fn a_future_timestamp_clamps_to_now() {
        assert_eq!(format_age(-5_000), "now");
    }

    // ── row rendering ─────────────────────────────────────────────────────

    fn row(kind: Option<LivenessKind>, live: Option<bool>) -> RosterRow {
        RosterRow {
            entity_kind: "principal".into(),
            entity_id: "abcdef0123456789".into(),
            label: Some("amy".into()),
            liveness_kind: kind,
            live,
            host: None,
            status_text: None,
            availability: None,
            recorded_at: Some(0),
        }
    }

    /// A `recent` row is only meaningful with its age attached.
    #[test]
    fn a_recent_row_always_shows_its_age() {
        let line = row_line(&row(Some(LivenessKind::Recent), Some(true)), 240_000);
        assert!(line.text.starts_with("\u{25d0} 4m "), "got {:?}", line.text);

        // ...including when we do not have one.
        let mut r = row(Some(LivenessKind::Recent), Some(true));
        r.recorded_at = None;
        let line = row_line(&r, 240_000);
        assert!(line.text.starts_with("\u{25d0} ? "), "got {:?}", line.text);
    }

    /// `live == None` is unknown: marked, at full weight, never dimmed into
    /// "not here" and never dropped.
    #[test]
    fn unknown_liveness_is_marked_not_dimmed() {
        let line = row_line(&row(Some(LivenessKind::Bound), None), 0);
        assert!(line.text.contains("(?)"), "got {:?}", line.text);
        assert_eq!(line.tone, LineTone::Row);
    }

    /// Not-live reads differently depending on HOW we knew — an observed
    /// disconnect is gone, an unconfirmed attestation is merely stale.
    #[test]
    fn not_live_distinguishes_gone_from_stale() {
        let bound = row_line(&row(Some(LivenessKind::Bound), Some(false)), 0);
        assert!(bound.text.ends_with("(gone)"), "got {:?}", bound.text);
        assert_eq!(bound.tone, LineTone::Dim);

        let attested = row_line(&row(Some(LivenessKind::Attested), Some(false)), 0);
        assert!(attested.text.ends_with("(stale)"), "got {:?}", attested.text);
        assert_eq!(attested.tone, LineTone::Dim);
    }

    /// A liveness kind from a newer kernel renders as an honest `?` row, not
    /// a borrowed glyph and not a dropped line.
    #[test]
    fn an_unknown_kind_still_gets_a_row() {
        let line = row_line(&row(Some(LivenessKind::Unknown("heard".into())), Some(true)), 0);
        assert!(line.text.starts_with("? amy"), "got {:?}", line.text);
    }

    #[test]
    fn a_row_carries_availability_status_and_host_when_present() {
        let mut r = row(Some(LivenessKind::Bound), Some(true));
        r.availability = Some(Availability::Active);
        r.status_text = Some("at the keys".into());
        r.host = Some("moltar".into());
        let line = row_line(&r, 0);
        assert_eq!(line.text, "\u{25cf} amy [active] at the keys @moltar");

        // ...and shows none of them when the kernel has none.
        let bare = row_line(&row(Some(LivenessKind::Bound), Some(true)), 0);
        assert_eq!(bare.text, "\u{25cf} amy");
    }

    // ── the honest footer ─────────────────────────────────────────────────

    /// The four "no rows" states must be four different panels. This is the
    /// test that fails if an empty list is ever allowed to speak for itself.
    #[test]
    fn every_empty_state_says_which_one_it_is() {
        let mut never = RosterFeed::default();
        never.state = RosterFetch::Never;
        let mut fresh = RosterFeed::default();
        fresh.set_for_test(ParsedIndex::default(), RosterFetch::Fresh, 100.0);
        let mut absent = RosterFeed::default();
        absent.state = RosterFetch::NoRoster {
            detail: "not found: /run/roster/index".into(),
        };
        let mut failed = RosterFeed::default();
        failed.state = RosterFetch::Error {
            detail: "connection reset".into(),
        };

        let text = |f: &RosterFeed| {
            roster_section(f, 0, 100.0)
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
                .join(" | ")
        };

        let (n, f, a, e) = (text(&never), text(&fresh), text(&absent), text(&failed));
        assert!(n.contains("never fetched"), "{n}");
        assert!(f.contains("nobody around"), "{f}");
        assert!(a.contains("no roster on this kernel"), "{a}");
        assert!(e.contains("error: connection reset"), "{e}");
        // All four distinct — that is the whole property.
        let all = [&n, &f, &a, &e];
        for (i, x) in all.iter().enumerate() {
            for y in all.iter().skip(i + 1) {
                assert_ne!(x, y, "two empty states rendered identically");
            }
        }
    }

    #[test]
    fn unparsed_rows_are_reported_in_the_footer() {
        let doc = format!(
            "{}\n\
             principal\t1\tamy\tbound\ttrue\t\t\t\t\n\
             garbage\n",
            header()
        );
        let feed = feed_with(&doc);
        let section = roster_section(&feed, 0, 100.0);
        let joined: Vec<&str> = section.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            joined.iter().any(|l| l.contains("1 unparsed rows")),
            "{joined:?}"
        );
    }

    #[test]
    fn the_footer_states_how_stale_the_picture_is() {
        let feed = feed_with(&format!("{}\n", header()));
        // 100.0 was the fetch time; 340s later.
        let section = roster_section(&feed, 0, 440.0);
        assert!(
            section.lines.iter().any(|l| l.text == "fetched 5m ago"),
            "{:?}",
            section.lines
        );
    }

    /// A long kernel-supplied status is shortened WITH an ellipsis — a
    /// silent truncation would pass a fragment off as the whole message.
    #[test]
    fn long_details_are_elided_visibly() {
        let long = "x".repeat(200);
        let out = elide(&long);
        assert_eq!(out.chars().count(), MAX_DETAIL_CHARS);
        assert!(out.ends_with('\u{2026}'));
        assert_eq!(elide("short"), "short");
    }

    /// A big fleet must not grow the panel off the bottom of the window —
    /// and the rows that fall off must be counted, not dropped.
    #[test]
    fn a_large_roster_is_capped_and_says_how_much_it_hid() {
        let mut doc = String::from(header());
        doc.push('\n');
        for i in 0..(MAX_ROSTER_ROWS + 7) {
            // Zero-padded so lexical order matches numeric order — the cut
            // is then predictable enough to assert on.
            doc.push_str(&format!(
                "principal\t{i}\tp{i:03}\tbound\ttrue\t\t\t\t\n"
            ));
        }
        let feed = feed_with(&doc);
        let section = roster_section(&feed, 0, 100.0);
        let rows: Vec<&PanelLine> = section
            .lines
            .iter()
            .filter(|l| l.text.starts_with('\u{25cf}'))
            .collect();
        assert_eq!(rows.len(), MAX_ROSTER_ROWS);
        assert!(
            section.lines.iter().any(|l| l.text == "+7 more"),
            "{:?}",
            section.lines
        );
        // The footer still gets said after the overflow line.
        assert!(section.lines.iter().any(|l| l.text.starts_with("fetched ")));
    }

    /// Exactly at the cap there is no overflow line — an off-by-one here
    /// would claim "+0 more".
    #[test]
    fn a_roster_exactly_at_the_cap_says_nothing_extra() {
        let mut doc = String::from(header());
        doc.push('\n');
        for i in 0..MAX_ROSTER_ROWS {
            doc.push_str(&format!(
                "principal\t{i}\tp{i:03}\tbound\ttrue\t\t\t\t\n"
            ));
        }
        let feed = feed_with(&doc);
        let section = roster_section(&feed, 0, 100.0);
        assert!(
            !section.lines.iter().any(|l| l.text.contains("more")),
            "{:?}",
            section.lines
        );
    }

    /// Sections are a list so the next one is a `push`. Today it is one.
    #[test]
    fn the_panel_is_a_list_of_sections() {
        let feed = feed_with(&format!("{}\n", header()));
        let sections = build_sections(&feed, 0, 100.0);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "ROSTER");
    }
}

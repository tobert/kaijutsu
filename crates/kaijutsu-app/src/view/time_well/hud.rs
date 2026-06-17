//! The well's edge HUD: the selected card's data fanned out to the four edges
//! (N/S/E/W) instead of a panel pulled into the center, so the glowing core +
//! rings stay the open browser space. As you browse (arrow/Tab/band-hop) the HUD
//! tracks the selection:
//!
//! - **N** (top): identity — title · context-type · live status.
//! - **E** (right): specs — model, fork kind, band, keywords, cluster.
//! - **W** (left): lineage — the fork-ancestry chain (this ◂ parent ◂ …).
//! - **S** (bottom): preview — a snippet of the most representative block.
//!
//! First cut renders with Bevy's native `Text` (the bundled mono font, same as
//! the unfocused-pane summary); a vello/MSDF styling pass can follow if the data
//! layout lands. Spawned on enter, despawned on exit, refreshed each frame but
//! only rewritten when the formatted text actually changes (no per-frame
//! relayout).

use bevy::prelude::*;
use kaijutsu_types::{ContextId, Status};
use kaijutsu_viz::layout::Band;

use super::scene::{Card, TimeWellState};

/// Which edge a HUD text node lives on. Shared despawn happens via this query.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum HudSlot {
    North,
    East,
    West,
    South,
}

/// Container marker for the whole HUD (despawned together on exit).
#[derive(Component)]
pub struct WellHud;

const HUD_FONT: &str = "fonts/CascadiaCodeNF.ttf";

/// Spawn the four edge HUD readouts (empty until a selection drives them).
pub fn spawn_well_hud(mut commands: Commands, asset_server: Res<AssetServer>, theme: Res<crate::ui::theme::Theme>) {
    let font = asset_server.load(HUD_FONT);

    // N + S are centered horizontally via a full-width flex container; E + W are
    // narrow boxes anchored to the side, vertically centered.
    spawn_centered(
        &mut commands,
        HudSlot::North,
        &font,
        theme.fg,
        18.0,
        Some(Val::Px(16.0)),
        None,
    );
    spawn_centered(
        &mut commands,
        HudSlot::South,
        &font,
        theme.fg_dim,
        13.0,
        None,
        Some(Val::Px(56.0)),
    );
    spawn_side(&mut commands, HudSlot::East, &font, theme.fg_dim, false);
    spawn_side(&mut commands, HudSlot::West, &font, theme.fg_dim, true);
}

/// A centered top/bottom readout: a full-width flex row that centers its text.
fn spawn_centered(
    commands: &mut Commands,
    slot: HudSlot,
    font: &Handle<Font>,
    color: Color,
    size: f32,
    top: Option<Val>,
    bottom: Option<Val>,
) {
    commands
        .spawn((
            WellHud,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(0.0),
                right: Val::Px(0.0),
                top: top.unwrap_or(Val::Auto),
                bottom: bottom.unwrap_or(Val::Auto),
                justify_content: JustifyContent::Center,
                ..default()
            },
        ))
        .with_children(|p| {
            p.spawn((
                slot,
                Text::new(""),
                TextFont {
                    font: font.clone(),
                    font_size: size,
                    ..default()
                },
                TextColor(color),
                TextLayout::new_with_justify(Justify::Center),
            ));
        });
}

/// A side (E/W) readout: a narrow absolute box, vertically centered-ish.
fn spawn_side(commands: &mut Commands, slot: HudSlot, font: &Handle<Font>, color: Color, west: bool) {
    let mut node = Node {
        position_type: PositionType::Absolute,
        top: Val::Percent(26.0),
        width: Val::Px(300.0),
        ..default()
    };
    if west {
        node.left = Val::Px(28.0);
    } else {
        node.right = Val::Px(28.0);
        node.justify_content = JustifyContent::FlexEnd;
    }
    commands.spawn((
        WellHud,
        slot,
        Text::new(""),
        TextFont {
            font: font.clone(),
            font_size: 13.0,
            ..default()
        },
        TextColor(color),
        TextLayout::new_with_justify(if west { Justify::Left } else { Justify::Right }),
        node,
    ));
}

/// Despawn the whole HUD on exit.
pub fn despawn_well_hud(mut commands: Commands, hud: Query<Entity, With<WellHud>>) {
    for e in hud.iter() {
        commands.entity(e).despawn();
    }
}

/// Refresh the four readouts from the current selection. Recomputes every frame
/// (cheap formatting) but writes a `Text` only when its string changes, so
/// unchanged edges never re-layout. Nothing selected → all edges blank.
pub fn update_well_hud(
    state: Res<TimeWellState>,
    cards: Query<&Card>,
    mut hud: Query<(&HudSlot, &mut Text)>,
) {
    let selected = state.selected.and_then(|sel| cards.iter().find(|c| c.context_id == sel));

    for (slot, mut text) in hud.iter_mut() {
        let next = match (selected, slot) {
            (Some(card), HudSlot::North) => hud_north(&card.data, card.status),
            (Some(card), HudSlot::East) => hud_east(&card.data),
            (Some(card), HudSlot::West) => hud_west(card.context_id, &state),
            (Some(card), HudSlot::South) => hud_south(&card.data),
            (None, _) => String::new(),
        };
        if text.0 != next {
            text.0 = next;
        }
    }
}

fn status_label(status: Option<Status>) -> &'static str {
    match status {
        Some(Status::Running) => "● running",
        Some(Status::Error) => "✕ error",
        Some(Status::Done) => "✓ done",
        _ => "idle",
    }
}

fn band_label(band: Band) -> &'static str {
    match band {
        Band::Hot => "hot",
        Band::RecentConcluded => "recent",
        Band::Haystack => "haystack",
    }
}

fn hud_north(d: &super::card::CardData, status: Option<Status>) -> String {
    let kind = if d.accent.is_empty() { "—" } else { d.accent.as_str() };
    format!("{}\n{} · {}", d.title, kind, status_label(status))
}

fn hud_east(d: &super::card::CardData) -> String {
    let mut s = String::new();
    s.push_str("SPECS\n");
    s.push_str(&format!(
        "model    {}\n",
        if d.model_badge.is_empty() { "—" } else { &d.model_badge }
    ));
    s.push_str(&format!(
        "fork     {}\n",
        d.fork_badge.as_deref().unwrap_or("—")
    ));
    s.push_str(&format!("band     {}\n", band_label(d.band)));
    let keys = if d.keywords.is_empty() {
        "—".to_string()
    } else {
        d.keywords.join(", ")
    };
    s.push_str(&format!("keywords {keys}"));
    if let Some(c) = &d.cluster_label {
        s.push_str(&format!("\ncluster  ◇ {c}"));
    }
    s
}

fn hud_west(selected: ContextId, state: &TimeWellState) -> String {
    // Walk the fork-ancestry chain up (this ◂ parent ◂ …), titles from the join.
    let mut out = String::from("LINEAGE\n");
    let mut cur = Some(selected);
    let mut depth = 0;
    while let Some(id) = cur {
        let title = state
            .join
            .get(&id)
            .map(|c| c.id.display_or(Some(c.label.as_str())))
            .unwrap_or_else(|| id.short());
        if depth == 0 {
            out.push_str(&title);
        } else {
            out.push_str(&format!("\n◂ {title}"));
        }
        // Stop after a handful of generations; guard against cycles.
        depth += 1;
        if depth >= 6 {
            out.push_str("\n◂ …");
            break;
        }
        cur = state.join.get(&id).and_then(|c| c.forked_from);
        if cur == Some(id) {
            break;
        }
    }
    if depth == 1 {
        out.push_str("\n(root)");
    }
    out
}

fn hud_south(d: &super::card::CardData) -> String {
    match &d.preview {
        Some(p) => {
            let snippet: String = p.chars().take(160).collect();
            if p.chars().count() > 160 {
                format!("{snippet}…")
            } else {
                snippet
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(band: Band) -> super::super::card::CardData {
        super::super::card::CardData {
            title: "alpha".into(),
            accent: "coder".into(),
            model_badge: "anthropic/claude-opus-4-8".into(),
            fork_badge: Some("full".into()),
            keywords: vec!["rings".into(), "pulse".into()],
            preview: None,
            band,
            forked_from: None,
            cluster_label: None,
        }
    }

    #[test]
    fn north_shows_title_type_and_status() {
        let n = hud_north(&card(Band::Hot), Some(Status::Running));
        assert!(n.starts_with("alpha"), "title leads");
        assert!(n.contains("coder"), "context-type shown");
        assert!(n.contains("running"), "live status shown");
    }

    #[test]
    fn north_empty_type_falls_back_to_dash() {
        let mut c = card(Band::Hot);
        c.accent = String::new();
        let n = hud_north(&c, None);
        assert!(n.contains("— · idle"), "empty type → dash, no status → idle: {n}");
    }

    #[test]
    fn east_lists_specs_with_dash_fallbacks() {
        let mut c = card(Band::RecentConcluded);
        c.model_badge = String::new();
        c.fork_badge = None;
        c.keywords = vec![];
        let e = hud_east(&c);
        assert!(e.contains("model    —"), "empty model → dash: {e}");
        assert!(e.contains("fork     —"), "no fork → dash");
        assert!(e.contains("keywords —"), "no keywords → dash");
        assert!(e.contains("band     recent"), "band labeled");
        assert!(!e.contains("cluster"), "no cluster line when unclustered");
    }

    #[test]
    fn east_shows_cluster_only_when_present() {
        let mut c = card(Band::Haystack);
        c.cluster_label = Some("storage".into());
        let e = hud_east(&c);
        assert!(e.contains("cluster  ◇ storage"), "cluster line present: {e}");
        assert!(e.contains("rings, pulse"), "keywords joined");
    }

    #[test]
    fn south_truncates_long_preview() {
        let mut c = card(Band::Hot);
        c.preview = Some("x".repeat(300));
        let s = hud_south(&c);
        assert!(s.ends_with('…'), "long preview is elided");
        assert!(s.chars().count() <= 161, "bounded length");

        c.preview = Some("short".into());
        assert_eq!(hud_south(&c), "short", "short preview passes through");
    }
}

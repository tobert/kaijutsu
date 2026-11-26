use bevy::camera::{RenderTarget, visibility::RenderLayers};
use bevy::color::Mix;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureFormat};

use super::model::{Section, Workspace};
use super::{NeoMode, SceneContent, score_preview::ScorePreview};
use crate::ui::quick_context::{LineTone, PanelLine, tone_color};
use crate::ui::{screen::Screen, theme::Theme};

mod layout;
pub(super) mod scenery;
pub(super) mod thread;
pub(super) mod turns;

const STAGE_LAYER: usize = 12;
const CARD_LAYER: usize = 13;
const REFERENCE_CARD_WIDTH: f32 = 864.0;
const STAGE_ORDER: isize = -1;

#[derive(Component)]
pub(super) struct SceneViewport;

#[derive(Component)]
pub(super) struct ThreadNotch;

#[derive(Component)]
pub(super) struct TurnHeading;

struct Card {
    surface: Entity,
    camera: Entity,
    ui: Entity,
    image: Handle<Image>,
    material: Handle<StandardMaterial>,
    group: usize,
    rank: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_active_front_sheet_has_focus_corners() {
        let mut world = World::new();
        let mut card = Card {
            ui: world.spawn_empty().id(), surface: Entity::PLACEHOLDER, camera: Entity::PLACEHOLDER,
            image: default(), material: default(), group: 1, rank: 0,
        };
        for (group, rank, focused, expected) in [(1, 0, true, 4), (1, 0, false, 0),
            (2, 0, true, 4), (2, 1, true, 0), (0, 0, false, 0)]
        {
            card.group = group;
            card.rank = rank;
            draw_card(&mut world.commands(), &default(), &Theme::default(), &card, &[], "Empty", focused, UVec2::new(640, 320), None, None);
            world.flush();
            let corners = world.query::<(&Name, &Node)>().iter(&world)
                .filter(|(name, node)| name.as_str() == "Neo Focus Corner" && node.position_type == PositionType::Absolute)
                .count();
            assert_eq!(corners, expected, "group {group}, rank {rank}, focused {focused}");
            assert!(world.get::<BackgroundGradient>(card.ui).is_some(), "the face spans the whole sheet");
            let rim = world.get::<BorderColor>(card.ui).unwrap();
            assert_ne!(rim.top, rim.bottom);
        }
    }

    #[test]
    fn selected_call_label_keeps_its_intrinsic_text_size() {
        let mut world = World::new();
        let ui = world.spawn_empty().id();
        let card = Card {
            ui, surface: Entity::PLACEHOLDER, camera: Entity::PLACEHOLDER,
            image: default(), material: default(), group: 0, rank: 0,
        };
        let lines = ["Conversation", "Earlier call · diff_block"].map(|s| PanelLine { text: s.into(), tone: LineTone::Head });
        draw_card(&mut world.commands(), &default(), &Theme::default(), &card, &lines, "", true, UVec2::new(800, 800), Some(1), None);
        world.flush();
        let (text, children) = world.query_filtered::<(&Text, Option<&Children>), With<ThreadNotch>>().single(&world).unwrap();
        assert_eq!(text.0, "Earlier call · diff_block");
        assert!(children.is_none(), "a layout child overrides the text leaf's intrinsic size");
    }
}

#[derive(Resource)]
pub(super) struct SceneAssets {
    camera: Entity,
    image: Handle<Image>,
    mono: Handle<Font>,
    serif: Handle<Font>,
    cards: Vec<Card>,
}

pub(super) fn setup(
    mut commands: Commands,
    fonts: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    palette: Res<crate::view::scene_palette::HeroicPalette>,
) {
    let image = images.add(Image::new_target_texture(1, 1, TextureFormat::Bgra8UnormSrgb, None));
    let camera = commands.spawn((
        Name::new("Neo Scene Camera"),
        Camera3d::default(),
        Projection::from(PerspectiveProjection { fov: layout::fov(16.0 / 9.0), ..default() }),
        Camera { order: STAGE_ORDER, is_active: false, ..default() },
        Tonemapping::None,
        RenderTarget::Image(image.clone().into()),
        Transform::from_xyz(0.0, 0.0, layout::CAMERA_DISTANCE),
        RenderLayers::layer(STAGE_LAYER),
        Msaa::Sample4,
    )).id();
    scenery::spawn(&mut commands, &mut meshes, &mut materials, &palette);
    let plane = meshes.add(Rectangle::new(1.0, 1.0));
    let mut cards = Vec::new();
    for group in 0..layout::GROUPS {
        let count = if group == 0 { 1 } else { layout::STACK_DEPTH };
        for rank in 0..count {
            let pose = layout::pose(group, rank, 16.0 / 9.0);
            let image = images.add(Image::new_target_texture(1, 1, TextureFormat::Bgra8UnormSrgb, None));
            let card_camera = commands.spawn((
                Name::new("Neo Card Camera"),
                Camera2d,
                Camera { order: STAGE_ORDER - 1 - cards.len() as isize, is_active: false, ..default() },
                RenderTarget::Image(image.clone().into()),
                RenderLayers::layer(CARD_LAYER),
                Msaa::Off,
            )).id();
            let ui = commands.spawn((
                Name::new("Neo Card Content"),
                UiTargetCamera(card_camera),
                Node {
                    width: percent(100),
                    height: percent(100),
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::clip(),
                    border: if rank == 0 {
                        UiRect { left: px(1), top: px(1), right: px(2), bottom: px(3) }
                    } else { UiRect::all(px(1)) },
                    ..default()
                },
                BorderColor::default(),
            )).id();
            let material = materials.add(StandardMaterial {
                base_color_texture: Some(image.clone()),
                unlit: true,
                alpha_mode: AlphaMode::Blend,
                ..default()
            });
            let name = match group {
                0 => "Neo Conversation Card",
                1 => "Neo Artifact Card",
                _ => "Neo Event Card",
            };
            let card_mesh = if group == 0 { meshes.add(Rectangle::new(1.0, 1.0)) } else { plane.clone() };
            let surface = commands.spawn((
                Name::new(name),
                Mesh3d(card_mesh),
                MeshMaterial3d(material.clone()),
                pose,
                Visibility::Hidden,
                RenderLayers::layer(STAGE_LAYER),
            )).id();
            cards.push(Card { surface, camera: card_camera, ui, image, material, group, rank });
        }
    }
    commands.insert_resource(SceneAssets {
        camera, image,
        mono: fonts.load("fonts/NotoMono-Regular.ttf"),
        serif: fonts.load("fonts/NotoSerif-Regular.ttf"),
        cards,
    });
}

pub(super) fn sync(
    mut commands: Commands,
    mode: Res<NeoMode>,
    screen: Res<State<Screen>>,
    workspace: Res<Workspace>,
    content: Res<SceneContent>,
    mut assets: ResMut<SceneAssets>,
    theme: Res<Theme>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cameras: Query<(&mut Camera, &mut Projection, &mut RenderTarget)>,
    mut surfaces: Query<(&mut Visibility, &mut Transform)>,
    mut viewports: Query<(&mut ImageNode, &mut Node, &mut ZIndex, &ComputedNode), With<SceneViewport>>,
) {
    let mut size = UVec2::ZERO;
    let shown = mode.enabled && *screen.get() == Screen::Conversation;
    if let Ok((mut node, mut style, mut layer, bounds)) = viewports.single_mut() {
        let display = if shown { Display::Flex } else { Display::None };
        if style.display != display {
            style.display = display;
        }
        layer.set_if_neq(ZIndex(1));
        if node.image != assets.image {
            node.image = assets.image.clone();
        }
        if !bounds.is_empty() {
            let pixels = bounds.size();
            let scale = (2048.0 / pixels.max_element()).min(1.0);
            size = (pixels * scale).round().as_uvec2().max(UVec2::ONE);
        }
    }
    let active = shown && size != UVec2::ZERO;

    let Ok((mut camera, mut projection, _)) = cameras.get_mut(assets.camera) else { return };
    let activation_changed = active != camera.is_active;
    if activation_changed {
        camera.is_active = active;
    }
    if (theme.is_changed() || activation_changed) && active {
        camera.clear_color = theme.bg.with_alpha(1.0).into();
    }
    let resized = active && resize_image(&mut images, &assets.image, size);
    let aspect = size.x as f32 / size.y.max(1) as f32;
    if resized && let Projection::Perspective(projection) = &mut *projection {
        projection.fov = layout::fov(aspect);
    }
    if !activation_changed && (!active || !(resized || theme.is_changed() || content.is_changed() || workspace.is_changed())) {
        return;
    }

    let SceneAssets { cards, mono, serif, .. } = &mut *assets;
    for card in cards {
        let (lines, fallback, focused) = match card.group {
            0 => (Some(content.conversation.as_slice()), "Conversation", false),
            1 => (
                content.artifacts.get(card.rank).map(Vec::as_slice),
                "No recent patches or scores",
                workspace.section == Section::Workbench,
            ),
            _ => (
                content.events.get(card.rank).map(Vec::as_slice),
                "No recent events",
                workspace.section == Section::History,
            ),
        };
        let visible = active && (card.rank == 0 || lines.is_some());
        let Ok((mut camera, _, mut target)) = cameras.get_mut(card.camera) else { continue };
        let Ok((mut visibility, mut transform)) = surfaces.get_mut(card.surface) else { continue };
        if camera.is_active != visible {
            camera.is_active = visible;
        }
        visibility.set_if_neq(if visible { Visibility::Visible } else { Visibility::Hidden });
        if !visible {
            continue;
        }

        camera.clear_color = Color::NONE.into();
        let pose = layout::pose(card.group, card.rank, aspect);
        transform.set_if_neq(pose);
        let texture_size = layout::texture_size(size, pose);
        if images.get(&card.image).is_none_or(|image| image.size() != texture_size)
            && let Some(mut material) = materials.get_mut(&card.material)
        {
            let image = images.add(Image::new_target_texture(
                texture_size.x, texture_size.y, TextureFormat::Bgra8UnormSrgb, None,
            ));
            *target = RenderTarget::Image(image.clone().into());
            material.base_color_texture = Some(image.clone());
            card.image = image;
        }
        let font = if card.group == 0 { &*serif } else { &*mono };
        let score = if card.group == 1 && card.rank == 0 { content.score.as_ref() } else { None };
        draw_card(&mut commands, font, &theme, card, lines.unwrap_or_default(), fallback, focused, texture_size, content.notch_line, score);
    }
}

fn resize_image(images: &mut Assets<Image>, handle: &Handle<Image>, size: UVec2) -> bool {
    if images.get(handle).is_none_or(|image| image.size() == size) {
        return false;
    }
    let Some(mut image) = images.get_mut(handle) else { return false };
    image.resize(Extent3d { width: size.x, height: size.y, depth_or_array_layers: 1 });
    true
}

fn draw_card(
    commands: &mut Commands,
    font: &Handle<Font>,
    theme: &Theme,
    card: &Card,
    lines: &[PanelLine],
    fallback: &str,
    focused: bool,
    texture_size: UVec2,
    notch_line: Option<usize>,
    score: Option<&ScorePreview>,
) {
    let scale = texture_size.x as f32 / REFERENCE_CARD_WIDTH;
    let padding = (22.0 * scale).max(6.0);
    let heading_size = (30.0 * scale).max(12.0);
    let body_size = (25.0 * scale).max(11.0);
    let tint = match card.group {
        1 => theme.accent2,
        2 => theme.ansi.yellow.mix(&theme.fg_dim, 0.48),
        _ => theme.accent,
    };
    let front = card.rank == 0;
    let distance = 1.0 / (1 + card.rank) as f32;
    let edge = theme.panel_bg.mix(&tint, 0.58 * distance);
    let bevel = theme.bg.mix(&tint, 0.17 * distance);
    let face = BackgroundGradient::from(LinearGradient::to_bottom_right(vec![
        theme.panel_bg.mix(&tint, 0.10 * distance).with_alpha(0.98).into(),
        theme.bg.mix(&theme.panel_bg, 0.25).with_alpha(0.96).into(),
    ]));
    let title = lines.first().map_or(fallback, |line| line.text.as_str());
    let title_color = if front { theme.fg.mix(&tint, 0.25) }
        else { theme.fg_dim.mix(&theme.panel_bg, 0.10 * card.rank as f32) };
    commands.entity(card.ui).despawn_children().insert((face, BorderColor {
        top: edge, left: edge, bottom: bevel, right: bevel,
    })).with_children(|root| {
        if front && focused && card.group != 0 {
            focus_corners(root, tint.mix(&theme.fg, 0.47), scale);
        }
        root.spawn((
            Node {
                align_self: AlignSelf::Start,
                margin: UiRect::axes(px(padding), px((10.0 * scale).max(3.0))),
                padding: UiRect::axes(px((8.0 * scale).max(3.0)), px((3.0 * scale).max(1.0))),
                ..default()
            },
        )).with_child((
            Text::new(title),
            TextFont { font: font.into(), font_size: FontSize::Px(heading_size), ..default() },
            TextColor(title_color),
            TextLayout::default().with_no_wrap(),
        ));

        if card.rank != 0 {
            return;
        }
        if let Some(score) = score {
            let available = (texture_size.as_vec2() - Vec2::new(padding * 2.0, padding + heading_size * 2.2))
                .max(Vec2::ONE);
            let size = score.size.as_vec2();
            let draw = size * (available / size).min_element();
            root.spawn((
                Name::new("Neo Score Preview"),
                ImageNode { color: theme.fg, ..ImageNode::new(score.image.clone()) },
                Node {
                    width: px(draw.x),
                    height: px(draw.y),
                    flex_shrink: 0.0,
                    margin: UiRect::left(px(padding)),
                    ..default()
                },
            ));
            return;
        }
        root.spawn(Node {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Start,
            width: percent(100),
            margin: UiRect::left(px(padding)),
            overflow: Overflow::clip(),
            ..default()
        }).with_children(|body| {
            for (index, line) in lines.iter().enumerate().skip(1) {
                let label = matches!(line.tone, LineTone::Head);
                let notch = card.group == 0 && notch_line == Some(index);
                let margin = px(if label { (8.0 * scale).max(2.0) } else { 0.0 });
                let text = (
                    Text::new(&line.text),
                    TextFont { font: font.into(), font_size: FontSize::Px(body_size), ..default() },
                    TextColor(if label { theme.accent } else { tone_color(line.tone, theme) }),
                    if card.group != 1 && !label {
                        TextLayout::default()
                    } else {
                        TextLayout::default().with_no_wrap()
                    },
                    Node {
                        flex_shrink: 0.0,
                        max_width: percent(100),
                        margin: if notch { UiRect::default() } else { UiRect::top(margin) },
                        padding: UiRect::horizontal(px((5.0 * scale).max(2.0))),
                        ..default()
                    },
                );
                if notch {
                    body.spawn(Node {
                        width: percent(100),
                        align_items: AlignItems::Center,
                        flex_shrink: 0.0,
                        margin: UiRect::top(margin),
                        ..default()
                    }).with_children(|row| {
                        row.spawn((text, ThreadNotch));
                        row.spawn((
                            Node {
                                flex_grow: 1.0,
                                height: px((1.5 * scale).max(1.0)),
                                ..default()
                            },
                            BackgroundColor(theme.accent.with_alpha(0.55)),
                        ));
                    });
                } else {
                    let mut text = body.spawn(text);
                    if card.group == 0 && label && matches!(line.text.as_str(), "User" | "Assistant") {
                        text.insert(TurnHeading);
                    }
                }
            }
        });
    });
}

#[derive(Component)]
pub(super) struct FocusCorner;

pub(super) fn focus_corners(root: &mut ChildSpawnerCommands, color: Color, scale: f32) {
    let stroke = px((3.0 * scale).max(1.5));
    for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
        let side = |on| if on { px(0) } else { Val::Auto };
        let border = |on| if on { stroke } else { px(0) };
        root.spawn((
            Name::new("Neo Focus Corner"),
            FocusCorner,
            Visibility::Inherited,
            Node {
                position_type: PositionType::Absolute,
                left: side(left), right: side(!left), top: side(top), bottom: side(!top),
                width: px((24.0 * scale).max(11.0)),
                height: px((14.0 * scale).max(7.0)),
                border: UiRect { left: border(left), right: border(!left), top: border(top), bottom: border(!top) },
                ..default()
            },
            BorderColor::all(color),
            ZIndex(1),
        ));
    }
}

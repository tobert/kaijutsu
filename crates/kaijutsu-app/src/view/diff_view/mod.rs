//! The full diff viewer — `Screen::Diff` over a **frozen** [`DiffCore`].
//!
//! `docs/diff.md` Decision 4/5 and slice 5. The inline preview
//! (`text::diff` + the `Diff` arm of `view::block_render`) stays exactly as it
//! is; this is the other surface, reached by `v` on a focused diff block, and
//! it renders the *whole* diff with vi motions and yank-to-clipboard.
//!
//! Three contracts shape everything here:
//!
//! - **Freeze-on-open.** Opening parses the block's content *fresh* and binds
//!   to `(block_id, content hash)` for the viewer's whole life. If the block
//!   changes underneath, a stale banner appears and `R` rebinds — the viewer
//!   never re-parses on its own. Cursor position, hunk indices and folds are
//!   all positions in the frozen model; swapping it out would move the
//!   reader's place without telling them. Yank reads the frozen core, always.
//! - **Esc belongs to vi.** `Screen::Diff` declares
//!   [`KeyboardGrab::DiffView`](crate::input::context::KeyboardGrab), so every
//!   key that isn't a Global binding or the Ctrl+A prefix reaches `DiffCore`.
//!   Esc leaves visual mode; `q`/`ZQ`/`:q` leave the screen.
//! - **A declared diff that does not parse is a visible error**, never an
//!   empty viewer.
//!
//! The state machine is deliberately app-local: `DiffCore` is pure and knows
//! nothing about blocks, screens, or clipboards. This module is the only place
//! the three meet.

use bevy::prelude::*;

use kaijutsu_crdt::BlockId;
use kaijutsu_diff::{DiffCore, DiffIntent, limits::MAX_RENDER_BYTES};
use kaijutsu_types::ContentType;

use crate::cell::{CellEditor, EditorEntities, FocusTarget, MainCell};
use crate::input::events::GrabbedKey;
use crate::input::{ActionFired, ClipboardWriter};
use crate::ui::screen::Screen;

pub mod render;

/// A hash of block content, the identity half of the freeze-on-open binding.
///
/// Any stable content hash would do; this is `DefaultHasher` because the
/// question it answers is only ever "is this the same bytes I parsed?" — never
/// "which bytes were they", and never across processes.
fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Does this block's content type mean the viewer should open on it?
///
/// The two mechanisms `docs/diff.md` describes, with their two failure
/// policies intact: a **declared** `ContentType::Diff` always opens (an error
/// state if it doesn't parse — the block asked to be a diff and is wrong about
/// it), while `Plain` opens only if the content actually parses as a diff.
/// Sniffing may enrich, never accuse.
pub fn openable_diff(content_type: ContentType, text: &str) -> Option<OpenAs> {
    match content_type {
        ContentType::Diff => Some(OpenAs::Declared),
        ContentType::Plain => {
            // A leading ` ```diff ` fence first (a model writing prose fences
            // its diffs), then the bare text — the same mechanism, and the
            // same "the fence must lead the block" rule, the inline sniff
            // uses, so the preview and the viewer never disagree about what
            // is a diff.
            let inner = crate::text::rich::extract_fenced_block(text, "diff").unwrap_or(text);
            let model = kaijutsu_diff::parse_with(
                inner,
                &kaijutsu_diff::DiffOptions {
                    profile: crate::text::diff::diff_profile_for(ContentType::Plain),
                    ..Default::default()
                },
            )
            .ok()?;
            // An empty parse is not a diff (`parse` accepts empty input).
            (!model.files.is_empty()).then(|| OpenAs::Sniffed {
                inner: inner.to_string(),
            })
        }
        ContentType::Markdown | ContentType::Abc | ContentType::Svg | ContentType::Image => None,
    }
}

/// How a block earned the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAs {
    /// `content_type == Diff` — authoritative; opens even if it won't parse.
    Declared,
    /// Plain text that sniffed as a diff. Carries the diff body actually
    /// found, so a fenced block is viewed without its fence.
    Sniffed {
        /// The diff body itself.
        inner: String,
    },
}

/// The viewer's whole state. `None` when no viewer is open.
#[derive(Resource, Default)]
pub struct ActiveDiffView {
    /// The open session, if any.
    pub session: Option<DiffViewSession>,
}

// SAFETY: mirrors `input::vim::VimMachineResource`, and for the same reason.
// `DiffCore` holds a modalkit `ModalMachine`, whose only non-Send/Sync member
// is an internal `Box<dyn Dialog>` for TUI command-line dialogs — a feature
// this viewer does not use and cannot reach: the box is private to modalkit
// and no accessor on `DiffCore` exposes it, so nothing here can hand it to a
// second thread. Bevy's own resource borrow rules do the rest (aliasing XOR
// mutation across systems), and the two systems that touch this live in
// different schedules anyway.
unsafe impl Send for ActiveDiffView {}
unsafe impl Sync for ActiveDiffView {}

/// One frozen viewing session.
pub struct DiffViewSession {
    /// The block this was opened from.
    pub block_id: BlockId,
    /// Hash of the exact content that was parsed. The freeze.
    pub content_hash: u64,
    /// A short title for the header strip.
    pub title: String,
    /// The parsed viewer, or the visible error that replaced it.
    pub content: DiffViewContent,
    /// True once the block's live content stopped matching `content_hash`.
    /// Drives the stale banner; never triggers a rebind by itself.
    pub stale: bool,
}

/// A parsed viewer, or the error state that stands in for one.
pub enum DiffViewContent {
    /// The frozen, navigable diff.
    Ready(Box<DiffCore>),
    /// The block declared itself a diff and does not parse. Carries what to
    /// draw; there is nothing to navigate.
    Failed {
        /// The parse error, verbatim.
        reason: String,
        /// The block's content, so the reader can see what it actually holds.
        source: String,
    },
}

impl DiffViewSession {
    /// The core, mutably.
    fn core_mut(&mut self) -> Option<&mut DiffCore> {
        match &mut self.content {
            DiffViewContent::Ready(core) => Some(core),
            DiffViewContent::Failed { .. } => None,
        }
    }
}

/// Parse `text` into a frozen session.
///
/// **The render byte budget is spent here, before anything is laid out**:
/// `truncate_to_bytes(MAX_RENDER_BYTES)` cuts on whole-hunk boundaries and
/// marks the result incomplete, because the parse ceiling is 16 MiB and Parley
/// runs on the main thread. The `DiffCore`'s canonical text — and therefore
/// every yank — is the truncated projection, which carries the
/// `#!kaijutsu-diff truncated:` marker so it cannot be mistaken for a patch.
pub fn open_session(
    block_id: BlockId,
    title: String,
    content: &str,
    how: &OpenAs,
) -> DiffViewSession {
    let (body, declared) = match how {
        OpenAs::Declared => (content, ContentType::Diff),
        OpenAs::Sniffed { inner, .. } => (inner.as_str(), ContentType::Plain),
    };
    let options = kaijutsu_diff::DiffOptions {
        profile: crate::text::diff::diff_profile_for(declared),
        ..Default::default()
    };
    let parsed = match kaijutsu_diff::parse_with(body, &options) {
        Ok(model) => {
            let bounded = kaijutsu_diff::truncate_to_bytes(&model, MAX_RENDER_BYTES);
            DiffViewContent::Ready(Box::new(DiffCore::new(bounded)))
        }
        Err(e) => DiffViewContent::Failed {
            reason: e.to_string(),
            source: body.to_string(),
        },
    };
    DiffViewSession {
        block_id,
        // Hash the WHOLE block content, not the parsed body: the freeze is
        // against the block, and a change to a fence header is still a change.
        content_hash: content_hash(content),
        title,
        content: parsed,
        stale: false,
    }
}

/// `v` on a focused block: open the viewer if that block is a diff.
fn handle_open_diff_viewer(
    mut actions: MessageReader<ActionFired>,
    focus: Res<FocusTarget>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut active: ResMut<ActiveDiffView>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    let mut asked = false;
    for ActionFired { action, .. } in actions.read() {
        if matches!(action, crate::input::action::Action::OpenDiffViewer) {
            asked = true;
        }
    }
    if !asked {
        return;
    }

    let Some(block_id) = focus.block_id else {
        return;
    };
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Some(block) = editor.block_snapshot(&block_id) else {
        return;
    };

    let Some(how) = openable_diff(block.content_type, &block.content) else {
        // Not a diff — say so rather than flashing an empty screen.
        info!("diff viewer: block {block_id:?} is not a diff; nothing to open");
        return;
    };

    let title = diff_title(&block);
    active.session = Some(open_session(block_id, title, &block.content, &how));
    next_screen.set(Screen::Diff);
}

/// A one-line title for the header strip: the tool that produced the block if
/// it says, otherwise a neutral label. Never the diff's own text.
fn diff_title(block: &kaijutsu_types::BlockSnapshot) -> String {
    match block.tool_name.as_deref() {
        Some(name) => format!("diff · {name}"),
        None => "diff".to_string(),
    }
}

/// Notice — and only notice — that the frozen block changed underneath.
///
/// Freeze-on-open forbids a silent rebind, so this sets a flag the header
/// strip reads. `R` (a [`DiffIntent::Refresh`]) is what actually re-parses.
fn detect_stale_diff_view(
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut active: ResMut<ActiveDiffView>,
) {
    let Some(session) = active.session.as_mut() else {
        return;
    };
    if session.stale {
        return;
    }
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    // A block that vanished (compaction, fork) counts as changed: the viewer
    // is now showing something with no source, which the reader should know.
    let changed = match editor.block_snapshot(&session.block_id) {
        Some(block) => content_hash(&block.content) != session.content_hash,
        None => true,
    };
    if changed {
        info!(
            "diff viewer: block {:?} changed underneath",
            session.block_id
        );
        session.stale = true;
    }
}

/// Feed the grabbed keyboard stream to `DiffCore` and act on what it drains.
///
/// The app is a key forwarder and an intent executor, nothing more: it never
/// interprets a keystroke itself, which is what keeps `q`-in-a-`:`-line from
/// being mistaken for a close and Esc from being mistaken for an exit.
fn diff_view_dispatch_keys(
    mut keyboard: MessageReader<GrabbedKey>,
    mut literal: MessageReader<crate::input::events::LiteralPrefix>,
    keys: Res<ButtonInput<KeyCode>>,
    mut active: ResMut<ActiveDiffView>,
    clipboard: Option<Res<ClipboardWriter>>,
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    mut next_screen: ResMut<NextState<Screen>>,
) {
    // A literal Ctrl+A has no meaning on a read-only surface; consume it so it
    // cannot leak to whatever runs next.
    literal.read().for_each(|_| {});

    let Some(session) = active.session.as_mut() else {
        keyboard.read().for_each(|_| {});
        return;
    };

    let mut terminal_keys = Vec::new();
    for grabbed in keyboard.read() {
        let event = &grabbed.0;
        if crate::view::editor::keys::is_modifier_key(event.key_code) {
            continue;
        }
        if let Some(key) = crate::input::vim::keyconv::bevy_to_terminal_key(event, &keys) {
            terminal_keys.push(key);
        }
    }

    // The error state has no core to drive; only "leave" is meaningful there.
    let Some(core) = session.core_mut() else {
        if terminal_keys.iter().any(is_leave_key) {
            next_screen.set(Screen::Conversation);
        }
        return;
    };
    if terminal_keys.is_empty() {
        return;
    }

    let mut refresh = false;
    for intent in core.apply_keys(terminal_keys) {
        match intent {
            DiffIntent::Yank { text } => match clipboard.as_ref() {
                // Reads the FROZEN core, never the block's current content —
                // copying a hunk that has since changed is a correctness bug
                // you cannot see.
                Some(writer) => {
                    debug!("diff viewer: yanked {} bytes", text.len());
                    writer.write(text);
                }
                None => warn!("diff viewer: yank discarded — no clipboard available"),
            },
            DiffIntent::Close => next_screen.set(Screen::Conversation),
            DiffIntent::Refresh => refresh = true,
        }
    }

    if refresh {
        refresh_session(session, &entities, &main_cells);
    }
}

/// The explicit half of freeze-on-open: re-read the block and re-freeze on its
/// current content. A brand-new session, deliberately — cursor, folds and
/// selection all belong to the model that is being replaced.
fn refresh_session(
    session: &mut DiffViewSession,
    entities: &EditorEntities,
    main_cells: &Query<&CellEditor, With<MainCell>>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Some(block) = editor.block_snapshot(&session.block_id) else {
        warn!(
            "diff viewer: cannot refresh — block {:?} is gone",
            session.block_id
        );
        return;
    };
    let Some(how) = openable_diff(block.content_type, &block.content) else {
        warn!(
            "diff viewer: cannot refresh — block {:?} is no longer a diff",
            session.block_id
        );
        return;
    };
    *session = open_session(
        session.block_id,
        std::mem::take(&mut session.title),
        &block.content,
        &how,
    );
}

/// In the error state there is no vi machine, so the two universal "get me out
/// of here" keys are recognized directly. Deliberately narrow: `q` and Esc,
/// nothing that could be mistaken for navigation.
fn is_leave_key(key: &modalkit::key::TerminalKey) -> bool {
    use modalkit::crossterm::event::{KeyCode as Ct, KeyEvent, KeyModifiers};
    *key == modalkit::key::TerminalKey::from(KeyEvent::new(Ct::Char('q'), KeyModifiers::NONE))
        || *key == modalkit::key::TerminalKey::from(KeyEvent::new(Ct::Esc, KeyModifiers::NONE))
}

/// Drop the session when the screen goes away, so a stale core can never be
/// revived by a later `Screen::Diff` entry that failed to open one.
fn clear_diff_view(mut active: ResMut<ActiveDiffView>) {
    active.session = None;
}

/// Wires the viewer's state, its open action, its key grab and its surface.
pub struct DiffViewPlugin;

impl Plugin for DiffViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveDiffView>()
            .add_systems(
                Update,
                handle_open_diff_viewer.after(crate::input::InputPhase::Dispatch),
            )
            .add_systems(
                Update,
                (detect_stale_diff_view, diff_view_dispatch_keys)
                    .chain()
                    .after(crate::input::InputPhase::Dispatch)
                    .run_if(in_state(Screen::Diff)),
            )
            .add_systems(OnEnter(Screen::Diff), render::spawn_diff_panel)
            .add_systems(
                OnExit(Screen::Diff),
                (render::despawn_diff_panel, clear_diff_view),
            )
            // The surface build needs ComputedNode, so it runs post-layout;
            // the cursor sync reads the geometry the build just computed.
            .add_systems(
                PostUpdate,
                (
                    render::build_diff_surface.after(bevy::ui::UiSystems::Layout),
                    render::sync_diff_cursor.after(render::build_diff_surface),
                )
                    .run_if(in_state(Screen::Diff)),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_diff::fixtures;

    fn block_id() -> BlockId {
        use kaijutsu_crdt::PrincipalId;
        BlockId::new(kaijutsu_crdt::ContextId::new(), PrincipalId::new(), 1)
    }

    fn session(content: &str, ct: ContentType) -> DiffViewSession {
        let how = openable_diff(ct, content).expect("openable");
        open_session(block_id(), "t".into(), content, &how)
    }

    /// The frozen core, when the session has one.
    fn core(s: &DiffViewSession) -> Option<&DiffCore> {
        match &s.content {
            DiffViewContent::Ready(c) => Some(c),
            DiffViewContent::Failed { .. } => None,
        }
    }

    // ── what opens ──────────────────────────────────────────────────────────

    #[test]
    fn a_declared_diff_opens_even_when_it_does_not_parse() {
        // Declared is authoritative: the block asked to be a diff and is wrong
        // about it, which is a state the reader must SEE, not one to hide by
        // refusing to open.
        assert_eq!(
            openable_diff(ContentType::Diff, "not a diff at all\n"),
            Some(OpenAs::Declared)
        );
        let s = session("not a diff at all\n", ContentType::Diff);
        assert!(core(&s).is_none(), "unparseable content has no core");
        let DiffViewContent::Failed { reason, source } = &s.content else {
            panic!("expected the error state");
        };
        assert!(!reason.is_empty(), "the failure must name itself");
        assert!(source.contains("not a diff"), "the source stays visible");
    }

    #[test]
    fn plain_text_opens_only_when_it_really_is_a_diff() {
        assert_eq!(openable_diff(ContentType::Plain, "hello\nworld\n"), None);
        assert_eq!(openable_diff(ContentType::Plain, ""), None);
        assert!(
            openable_diff(
                ContentType::Plain,
                &fixtures::read("canonical/single_file_modify.diff")
            )
            .is_some()
        );
    }

    /// The fence must LEAD the block — the same rule `extract_fenced_block`
    /// enforces for the inline preview, so the two surfaces can never
    /// disagree about whether a block is a diff.
    #[test]
    fn a_fenced_diff_opens_on_the_fence_contents() {
        let inner = fixtures::read("canonical/single_file_modify.diff");
        let fenced = format!("```diff\n{inner}```\n");
        let how = openable_diff(ContentType::Plain, &fenced).expect("the fence sniffs");
        let OpenAs::Sniffed { inner: body } = &how else {
            panic!("a fence is a sniff, not a declaration");
        };
        assert!(
            body.starts_with("diff --git"),
            "the fence itself is not viewed"
        );

        let s = open_session(block_id(), "t".into(), &fenced, &how);
        let core = core(&s).expect("the fence body parses");
        assert!(core.text().starts_with("diff --git"));

        // Prose before the fence is not a fenced block here, and the bare
        // text does not parse either — so it simply does not open.
        assert_eq!(
            openable_diff(ContentType::Plain, &format!("here you go:\n\n{fenced}")),
            None,
        );
    }

    #[test]
    fn other_content_types_never_open_the_viewer() {
        let diffy = fixtures::read("canonical/single_file_modify.diff");
        for ct in [
            ContentType::Markdown,
            ContentType::Abc,
            ContentType::Svg,
            ContentType::Image,
        ] {
            assert_eq!(openable_diff(ct, &diffy), None, "{ct:?} must not open");
        }
    }

    // ── freeze-on-open ──────────────────────────────────────────────────────

    /// The freeze binds to `(block_id, content hash)`. Identical content is
    /// the same freeze; any change at all — including one outside the parsed
    /// body — is a different one.
    #[test]
    fn the_freeze_is_the_hash_of_the_whole_block_content() {
        let text = fixtures::read("canonical/single_file_modify.diff");
        let a = session(&text, ContentType::Diff);
        let b = session(&text, ContentType::Diff);
        assert_eq!(a.content_hash, b.content_hash, "same bytes, same freeze");

        let c = session(&format!("{text}\n"), ContentType::Diff);
        assert_ne!(a.content_hash, c.content_hash, "a changed block is stale");

        // The freeze hashes the WHOLE block, not the parsed body: a change
        // to the fence wrapper is still a change to what the reader opened.
        let inner = fixtures::read("canonical/single_file_modify.diff");
        let one = session(&format!("```diff\n{inner}```"), ContentType::Plain);
        let two = session(&format!("```diff\n{inner}```\n"), ContentType::Plain);
        assert_ne!(one.content_hash, two.content_hash);
        assert_eq!(
            core(&one).unwrap().text(),
            core(&two).unwrap().text(),
            "test premise: only the wrapper differs",
        );
    }

    #[test]
    fn a_new_session_is_never_born_stale() {
        let s = session(
            &fixtures::read("canonical/multi_file.diff"),
            ContentType::Diff,
        );
        assert!(!s.stale);
    }

    // ── the render budget ───────────────────────────────────────────────────

    /// `MAX_RENDER_BYTES` is spent at open, before a glyph is shaped — and
    /// what survives says so, so a yank of it can never read as a whole patch.
    #[test]
    fn opening_spends_the_render_byte_budget_before_anything_is_drawn() {
        // One file per hunk so the whole-hunk cut has somewhere to land.
        let mut files = Vec::new();
        for i in 0..6_000 {
            let pad = "a fairly long filler line so the bytes add up\n".repeat(6);
            let before = format!("before {i}\n{pad}");
            let after = format!("after {i}\n{pad}");
            files.push(
                kaijutsu_diff::diff_file(
                    &kaijutsu_diff::FileSpec::modified(&format!("f{i}.txt"), &before, &after),
                    &kaijutsu_diff::DiffOptions::default(),
                )
                .unwrap(),
            );
        }
        let text = kaijutsu_diff::format(&kaijutsu_diff::DiffModel::new(files));
        assert!(
            text.len() > MAX_RENDER_BYTES,
            "test premise: it's oversized"
        );

        let s = session(&text, ContentType::Diff);
        let core = core(&s).expect("it parses");
        assert!(
            core.text().len() <= MAX_RENDER_BYTES,
            "{} bytes reached the viewer",
            core.text().len()
        );
        assert!(!core.model().is_complete(), "a cut diff must admit it");
        assert!(
            core.text()
                .starts_with(kaijutsu_diff::TRUNCATION_MARKER_PREFIX),
            "the marker must lead the text a yank would copy",
        );
    }

    // ── the leave hatch in the error state ──────────────────────────────────

    #[test]
    fn only_q_and_escape_leave_the_error_state() {
        use modalkit::crossterm::event::{KeyCode as Ct, KeyEvent, KeyModifiers};
        let key = |c| modalkit::key::TerminalKey::from(KeyEvent::new(c, KeyModifiers::NONE));
        assert!(is_leave_key(&key(Ct::Char('q'))));
        assert!(is_leave_key(&key(Ct::Esc)));
        for c in ['j', 'k', 'y', 'v', 'V', 'R', 'Q'] {
            assert!(!is_leave_key(&key(Ct::Char(c))), "{c} must not leave");
        }
    }
}

//! What one key, one paste or one picker key does.

use std::time::Instant;

use anyhow::Result;

use crate::app::App;
use crate::asks;
use crate::bridge::KernelBridge;
use crate::ctrl_z;
use crate::interrupt;
use crate::keys::{Intent, Keys};
use crate::picker::Outcome as PickerOutcome;
use crate::render;

use crossterm::event::KeyEvent;

use crate::copy;
use crate::diff::{self, DiffKey};
use crate::editor::{self, ScreenMode};

use super::feeds::Feeds;
use super::seat::{apply_kj_completion, close_and_demote, handle_colon_line, interrupt_ctrl_c, mirror_ops, switch_seat, toggle_shell};
use super::surfaces::act_open_diff;
use super::terminal::{TermLock, write_osc52};

/// Route one key to the open picker, then act on its [`PickerOutcome`].
/// A placement verb that ran marks the roster changed (`App::roster_changed`)
/// so the loop starts a refresh round now rather than on the next tick;
/// the round's `apply` rebuilds the open picker. `armed` is the `Ctrl+A`
/// prefix's state, for the key trace.
pub(super) async fn handle_picker_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
    armed: bool,
    feeds: &mut Feeds,
) -> Result<()> {
    let Some(picker) = app.picker.as_mut() else {
        return Ok(());
    };
    let outcome = picker.handle_key(key);
    // `act`'s key line, for the keys the picker reads before `act` would
    // see them; `outcome` says what the key resolved to. Opt-in in the same
    // way (`RUST_LOG=kaijutsu_tui::keys=trace`).
    tracing::trace!(target: "kaijutsu_tui::keys", code = ?key.code, modifiers = ?key.modifiers,
        kind = ?key.kind, armed, surface = "picker", ?outcome, "key");
    match outcome {
        PickerOutcome::None => {}
        PickerOutcome::Dismiss => app.picker = None,
        PickerOutcome::Switch(id) => {
            // The picker comes down first, so the switch's own notice is
            // the one on screen and the band shrinks on the same frame.
            app.picker = None;
            switch_seat(bridge, app, id, feeds).await;
        }
        PickerOutcome::Placement { context_id, argv } => match bridge.execute_kj(context_id, argv).await {
            // A latch exits 2; its message is the command that confirms it.
            Ok(result) if result.latch.is_some() => {
                let message = result.latch.map(|l| l.message).unwrap_or_default();
                if let Some(picker) = app.picker.as_mut() {
                    picker.latched();
                }
                app.note(format!("the same key again confirms: {message}"));
            }
            Ok(result) if result.exit_code != 0 => {
                let why = result.stderr.lines().next().unwrap_or("").trim().to_string();
                app.note(format!("placement failed (exit {}): {why}", result.exit_code));
            }
            Ok(result) => {
                app.note(result.stdout.lines().next().unwrap_or("done").to_string());
                app.roster_changed = true;
            }
            Err(e) => app.note(format!("placement failed: {e}")),
        },
    }
    Ok(())
}

/// Whether a key asked the loop to hand the terminal back to the host shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Acted {
    Continue,
    Suspend,
}

/// What the key path remembers from one key to the next: the `Ctrl+A`
/// prefix, the `Ctrl+C` ladder and the `Ctrl+Z` double tap.
pub(super) struct KeyState {
    pub(super) keys: Keys,
    interrupt_ladder: interrupt::Ladder,
    pub(super) ctrl_z_tap: ctrl_z::Tap,
}

impl KeyState {
    pub(super) fn new() -> Self {
        Self {
            keys: Keys::new(),
            interrupt_ladder: interrupt::Ladder::new(),
            ctrl_z_tap: ctrl_z::Tap::new(),
        }
    }
}

/// Act on one key.
pub(super) async fn act(
    bridge: &KernelBridge,
    app: &mut App,
    key_state: &mut KeyState,
    key: crossterm::event::KeyEvent,
    feeds: &mut Feeds,
    term_lock: &TermLock,
    width: u16,
) -> Result<Acted> {
    let KeyState { keys, interrupt_ladder, ctrl_z_tap } = key_state;
    // The editor is the sanctioned raw key reader (`docs/input.md`): while a
    // vi surface has the screen every key belongs to it, `Ctrl+C` included,
    // except the `Ctrl+A` prefix and its chord ([`Keys::claims`]).
    if editor::route_key(app, keys.claims(&key)) == editor::KeyRoute::FullScreen {
        act_full_screen(app, key, term_lock);
        return Ok(Acted::Continue);
    }
    // The ledger view captures its own keys while open — j/k/Esc are never
    // compose text. The `Ctrl+A` prefix pops over it, as it does over the
    // picker and the ask card (`docs/tui.md`, "Keys").
    if app.ledger_view.is_some() && !keys.claims(&key) {
        asks::ledger_key(app, key);
        return Ok(Acted::Continue);
    }
    // An ask card owns keys only once it is armed; until then every key
    // goes where it would with no card up, and pushes arming out
    // (`docs/tui.md`, "Asks"). A key release is never a keystroke.
    let card_owns_key = if key.kind == crossterm::event::KeyEventKind::Release {
        app.ask_card_armed()
    } else {
        app.ask_card_takes_key(Instant::now())
    };
    // An armed ask card keeps its own a/A/d/v/Esc: the card is the more
    // urgent surface, and the scrolled view waits under it.
    if scrolled_takes_key(app, card_owns_key, keys, &key) {
        if scrolled_key(app, &key, term_lock, width) == ScrolledKey::Handled {
            return Ok(Acted::Continue);
        }
        app.snap_transcript();
    }

    // The armed ask card owns a/A/d/v/Esc and holds compose text; a
    // `Ctrl+A` chord, `Ctrl+C` and `Ctrl+Z` act under it as they would under
    // no card (`docs/tui.md`, "Asks").
    let intent = if card_owns_key {
        match asks::route_under_card(key, keys) {
            asks::CardRoute::Card(card_key) => {
                asks::card_key(app, card_key);
                return Ok(Acted::Continue);
            }
            asks::CardRoute::Held => return Ok(Acted::Continue),
            asks::CardRoute::Chord(intent) => intent,
        }
    } else {
        keys.interpret(key)
    };

    // Over a full-screen surface the prefix moves between seats and hands
    // vim its own `Ctrl+A`; the surface stays with its context
    // (`App::switch_to`). Nothing else is drawn there to act on.
    if app.screen.is_full_screen() {
        match chord_over_full_screen(&intent) {
            OverFullScreen::Run => {}
            OverFullScreen::ToSurface(key) => {
                act_full_screen(app, key, term_lock);
                return Ok(Acted::Continue);
            }
            OverFullScreen::Hold => {
                app.note("only seat chords and Ctrl+A a work over a full-screen surface");
                return Ok(Acted::Continue);
            }
        }
    }

    // Opt-in (`RUST_LOG=kaijutsu_tui::keys=trace`): it records every
    // keystroke, draft text included, so it never rides a broader filter.
    tracing::trace!(target: "kaijutsu_tui::keys", code = ?key.code, modifiers = ?key.modifiers,
        kind = ?key.kind, armed = keys.armed(), ?intent, "key");

    if app.picker.is_some() || app.ledger_view.is_some() {
        match chord_over_overlay(&intent, app.picker.is_some(), app.ledger_view.is_some()) {
            OverOverlay::Keep => {}
            OverOverlay::Hold => return Ok(Acted::Continue),
            OverOverlay::Close => {
                app.picker = None;
                app.ledger_view = None;
                return Ok(Acted::Continue);
            }
            OverOverlay::CloseAndRun => {
                app.picker = None;
                app.ledger_view = None;
            }
        }
    }

    match intent {
        Intent::Ignored | Intent::LegendChanged => {}
        Intent::Interrupt => {
            interrupt_ctrl_c(bridge, app, interrupt_ladder).await;
        }
        Intent::InputKey(key) => match tail_key(app, &key, width) {
            TailKey::Draft => compose_key(bridge, app, key, feeds, term_lock).await?,
            // The tail is already on screen; there is nothing below it.
            TailKey::Nothing => {}
            TailKey::LeaveTail(step) => leave_tail(app, width, step),
        },
        // The first press toggles now; a second inside the window suspends
        // instead, and the toggle it lands on is kept (`docs/tui.md`, "Keys").
        Intent::CtrlZ => {
            if ctrl_z_tap.press(Instant::now()) {
                return Ok(Acted::Suspend);
            }
            toggle_shell(bridge, app, feeds).await;
        }
        Intent::SwitchSeat(n) => match app.seat_context(n) {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note(format!("no context on seat {n}")),
        },
        Intent::StepSeat(step) => match app.seat_neighbor(step) {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note("no seats"),
        },
        // `Ctrl+A ]` is `p`: one register holds the draft's yanks and
        // deletes and copy mode's yank.
        Intent::Paste => match app.current {
            Some(ctx) => match app.compose.put() {
                Some(ops) if ops.is_empty() => app.note("nothing to put — yank in the draft, or scroll up, then v and y"),
                Some(ops) => mirror_ops(app, ctx, &ops),
                None => app.note("close the ':' line before putting"),
            },
            None => app.note("no context attached"),
        },
        // `switch_seat` re-watches, which is what makes the toggle work
        // after a feed ended and dropped the context's view: a context with
        // no view renders nothing at all.
        Intent::LastContext => match app.previous {
            Some(id) => switch_seat(bridge, app, id, feeds).await,
            None => app.note("no previous context"),
        },
        Intent::OpenDiff => act_open_diff(app, width),
        Intent::CopyMode => match app.current {
            // Already off the tail: the chord is where the reader is, not a
            // second entry that would re-anchor them at the live tail.
            Some(_) if !app.following() => app.clear_notice(),
            Some(_) => {
                leave_tail(app, width, Step::Still);
                app.clear_notice();
            }
            None => app.note("no context to read"),
        },
        Intent::PrefillKj(body) => match app.current {
            Some(ctx) => {
                let ops = app.compose.open_command(body);
                mirror_ops(app, ctx, &ops);
            }
            None => app.note("no context attached"),
        },
        Intent::NotYet(message) => app.note(message),
        Intent::Unbound(code) => {
            app.note(format!("Ctrl+A {} is not bound", crate::keys::key_label(code)))
        }
        Intent::CloseAndDemote => match app.current {
            Some(ctx) => close_and_demote(bridge, app, ctx, feeds).await,
            None => app.note("no context attached"),
        },
        Intent::OpenLedger => asks::request_ledger(app),
        Intent::Tab => {
            if app.compose.kj_typed().is_some() {
                apply_kj_completion(app);
            } else {
                // The key as it arrived, modifiers and all, not a
                // synthesized bare `Tab`: `keys.rs` only routes an
                // unmodified `Tab` here today, and a future modifier should
                // reach compose rather than be dropped on the floor.
                compose_key(bridge, app, key, feeds, term_lock).await?;
            }
        }
        Intent::TogglePicker => app.open_picker(kaijutsu_types::now_millis()),
        // A local view change: no RPC, and a scrolled view stays where the
        // reader is (`docs/tui.md`, "Folds").
        Intent::ToggleFold => match render::toggle_fold(app, width) {
            Ok(_) => app.clear_notice(),
            Err(why) => app.note(why),
        },
    }
    Ok(Acted::Continue)
}

/// Whether the scrolled transcript takes `key` as one of copy mode's own.
///
/// Off the live tail the transcript owns copy mode's keys, and every other
/// key snaps it back and is then handled as if typed at the tail
/// (`docs/tui.md`, "Scrolling is copy mode"). Three kinds of key are never
/// its: one an armed ask card owns; the `Ctrl+A` prefix and its chord
/// ([`Keys::claims`]), so a seat switch leaves the reader's place where it
/// was; and `Ctrl+O` ([`Keys::toggles_fold`]), which folds where the reader
/// is rather than at the tail.
fn scrolled_takes_key(app: &App, card_owns_key: bool, keys: &Keys, key: &crossterm::event::KeyEvent) -> bool {
    app.scrolled().is_some() && !card_owns_key && !keys.claims(key) && !Keys::toggles_fold(key)
}

/// Whether the scrolled transcript took a key, or handed it back to the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrolledKey {
    Handled,
    Snap,
}

/// Act on one key while the transcript is off the live tail
/// (`docs/tui.md`, "Scrolling is copy mode").
///
/// The motions, the mark and the prompt are `copy.rs`'s; the two acts that
/// need more than the view are this side's — a search reads the whole
/// transcript to find its row, and a yank renders the marked rows, writes
/// them over OSC 52 and yanks them into the draft's vi registers.
fn scrolled_key(
    app: &mut App,
    key: &crossterm::event::KeyEvent,
    term_lock: &TermLock,
    width: u16,
) -> ScrolledKey {
    // Release events arrive only where the terminal negotiated the enhanced
    // keyboard protocol; acting on both edges would scroll twice a press,
    // the same reason `Keys::interpret` drops them.
    if key.kind == crossterm::event::KeyEventKind::Release {
        return ScrolledKey::Handled;
    }
    // Keys arrive faster than frames — a wheel tick is three of them, and a
    // committed search is followed straight away by `v` — so the view is
    // settled against the current row counts before it reads one. Without
    // this a key would act on the rows the last frame drew.
    let index = render::row_index(app, width);
    let height = render::transcript_height(app, width);
    let outcome = {
        let Some(view) = app.scrolled_mut() else {
            return ScrolledKey::Snap;
        };
        view.settle(&index, height);
        copy::handle_key(view, key)
    };
    match outcome {
        copy::Outcome::Snap => ScrolledKey::Snap,
        copy::Outcome::Moved => ScrolledKey::Handled,
        copy::Outcome::Leave => {
            app.snap_transcript();
            ScrolledKey::Handled
        }
        copy::Outcome::Find { needle, from, forward, skip_current } => {
            let rows: Vec<String> = render::transcript_all_lines(app, width)
                .iter()
                .map(copy::line_text)
                .collect();
            if let Some(target) = copy::find(&rows, &needle, from, forward, skip_current)
                && let Some(view) = app.scrolled_mut()
            {
                view.jump_to(target);
            }
            ScrolledKey::Handled
        }
        copy::Outcome::Yank(start, end) => {
            let text = render::transcript_rows_text(app, width, start, end);
            app.snap_transcript();
            match write_osc52(term_lock, &text) {
                Ok(()) => app.note(format!("yanked {} lines — p or Ctrl+A ] puts", text.lines().count())),
                Err(e) => app.note(format!("clipboard write failed: {e}")),
            }
            app.compose.yank_lines(&text);
            ScrolledKey::Handled
        }
    }
}

/// How far leaving the live tail scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `Ctrl+A [`: the same rows, now under copy mode's keys.
    Still,
    /// `Up`, which is also one third of a wheel tick.
    Line,
    /// `PageUp`.
    Page,
}

/// What a key means while the transcript is on the live tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailKey {
    /// The draft's, as ever.
    Draft,
    /// Nothing to do: the tail is already what is on screen.
    Nothing,
    /// Leave the tail and scroll.
    LeaveTail(Step),
}

/// `Up` at the top edge of the draft scrolls the transcript and `Down` at
/// the live tail does nothing — one rule for a typed arrow and for the wheel,
/// which the terminal sends as arrow keys and neither vim nor this client can
/// tell apart (`docs/tui.md`, "The mouse stays the terminal's").
fn tail_key(app: &mut App, key: &crossterm::event::KeyEvent, width: u16) -> TailKey {
    use crossterm::event::KeyCode;
    // The `:` bar keeps `Up` and `Down` for its own history.
    if app.compose.command_line().is_some() {
        return TailKey::Draft;
    }
    match key.code {
        // Measured in the draft's *drawn* rows, not its logical lines: a
        // one-line draft wider than the screen has rows above and below the
        // cursor, and an `Up` from one of them belongs to the draft. A
        // one-row draft is the common case, so every `Up` scrolls.
        KeyCode::Up if app.compose.cursor_on_first_row(width) => TailKey::LeaveTail(Step::Line),
        KeyCode::PageUp => TailKey::LeaveTail(Step::Page),
        KeyCode::Down if app.compose.cursor_on_last_row(width) => TailKey::Nothing,
        KeyCode::PageDown => TailKey::Nothing,
        _ => TailKey::Draft,
    }
}

/// Leave the live tail: the view is anchored where it already sits, so the
/// screen does not move, and then it scrolls by `step`. The reader lands on
/// the view's last row, the way tmux enters copy mode at the current screen.
fn leave_tail(app: &mut App, width: u16, step: Step) {
    let page = render::transcript_height(app, width);
    let mut view = copy::Scrolled::entering(page);
    match step {
        Step::Still => {}
        Step::Line => view.scroll(-1),
        Step::Page => view.scroll(-(page as isize)),
    }
    // Settled here rather than on the first frame, so the readout the band
    // draws next says where the reader is instead of `line 0/0`.
    let index = render::row_index(app, width);
    view.settle(&index, page);
    // The place belongs to the context on screen. A context whose feed
    // ended has no transcript to place a reader in and renders nothing at
    // all, so say so rather than drop the gesture.
    if !app.set_scrolled(view) {
        app.note("no transcript here; Ctrl+A <digit> reattaches");
    }
}

/// A pasted newline as `\n`, whatever the terminal sent: xterm sends
/// `\r`, some send `\r\n`. Order matters — the pair first, or its `\r`
/// becomes a second newline.
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Where a bracketed paste goes.
#[derive(Debug, PartialEq, Eq)]
enum PasteTarget {
    Draft,
    CommandLine,
    /// The alternate screen's editor session, by its kernel handle.
    Editor(u64),
    Refused(&'static str),
}

/// A paste is text for the draft, the `:` bar, or an open editor session.
/// Every other surface refuses it with a notice rather than reading it as
/// keys: a full-screen surface with no editor session (the diff viewer),
/// the picker's and the ledger's filters, and an ask card are not wired for
/// one. The editor's own `:` line refuses too, here rather than in the
/// kernel: the kernel refuses that paste as well (`PASTE_REFUSED_COMMAND_
/// LINE`), but its message rides `EditorState.message`, which the strip
/// draws only while no `:` line is open (`editor::strip_line`), so the
/// player would see nothing happen. A notice says why.
fn paste_target(app: &App) -> PasteTarget {
    if let Some(screen) = app.screen.editor() {
        if screen.state.command_line.is_some() {
            return PasteTarget::Refused("close the ':' line before pasting into the editor");
        }
        return PasteTarget::Editor(screen.session);
    }
    if editor::route_key(app, false) == editor::KeyRoute::FullScreen {
        return PasteTarget::Refused("paste on the alternate screen is not wired; use the draft");
    }
    if app.ledger_view.is_some() || app.picker.is_some() || app.ask_card_armed() {
        return PasteTarget::Refused("paste is not wired into this surface; Esc to the draft first");
    }
    if app.compose.command_line().is_some() { PasteTarget::CommandLine } else { PasteTarget::Draft }
}

/// Act on one `Event::Paste`. Line endings are normalized to `\n`, since
/// terminals differ on what they send for a pasted newline. The draft
/// takes it as one edit at the cursor, like `Ctrl+A ]`; the `:` bar takes
/// it flattened onto one line at its end; an open editor session takes it
/// as one `editorInsert` call, queued on the editor outbox behind the keys
/// before it (`kaijutsu_client::editor_outbox`).
pub(super) fn paste_text(app: &mut App, text: String) {
    let text = normalize_paste(&text);
    // A paste is typing: past a disarmed ask card it lands in the draft and
    // pushes arming out, as a key would.
    app.ask_card_takes_key(Instant::now());
    match paste_target(app) {
        PasteTarget::Refused(why) => app.note(why),
        PasteTarget::CommandLine => {
            let body = app.compose.command_line().map(|line| line[1..].to_string()).unwrap_or_default();
            let flat = text.lines().collect::<Vec<_>>().join(" ");
            app.compose.set_command_body(&format!("{body}{flat}"));
        }
        PasteTarget::Editor(session) => {
            if let Err(e) = app.editor_keys.insert(session, &text) {
                app.note(format!("paste not sent: {e:#}"));
            }
        }
        PasteTarget::Draft => {
            let Some(ctx) = app.current else {
                app.note("no context attached");
                return;
            };
            let ops = app.compose.paste(&text);
            mirror_ops(app, ctx, &ops);
        }
    }
}

/// One keystroke on the compose surface: mirror what the vi engine did onto
/// the context's draft block, and submit when it asks.
async fn compose_key(
    bridge: &KernelBridge,
    app: &mut App,
    key: crossterm::event::KeyEvent,
    feeds: &mut Feeds,
    term_lock: &TermLock,
) -> Result<()> {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return Ok(());
    };
    let action = app.compose.press(key);
    mirror_ops(app, ctx, &action.ops);
    // A yank in the draft goes to the system clipboard as copy mode's does.
    // The registers already hold it, so a terminal that drops OSC 52 loses
    // only the copy out.
    if let Some(text) = &action.yanked
        && let Err(e) = write_osc52(term_lock, text)
    {
        app.note(format!("clipboard write failed: {e}"));
    }
    if let Some(message) = action.notice {
        app.note(message);
    }
    if let Some(line) = action.command {
        handle_colon_line(bridge, app, ctx, line, feeds).await;
        return Ok(());
    }
    if action.submit {
        if app.compose.text().trim().is_empty() {
            return Ok(());
        }
        tracing::debug!(context = %ctx.short(), "submitting the draft");
        // The edge as it stood when Enter was pressed — computed before the
        // submit call, not after `mark_submitted`, which would read the
        // view after the kernel has already moved it (`docs/prompts.md`,
        // "The submit verb").
        let edge = app.views.get(&ctx).and_then(|view| view.edge());
        // A person's context runs its draft as shell and starts no turn.
        let is_shell = app.current_info().is_some_and(|info| info.performed_by_person);
        // The kernel snapshots its own copy of the draft, so the submit queues
        // behind every keystroke still on the wire.
        match app.drafts.submit(ctx, edge).await {
            Ok(block_id) => {
                app.mark_submitted(block_id);
                app.compose.reset();
                app.clear_notice();
                // The partial turn-liveness signal (`docs/tui.md`, "Ctrl+C
                // reclaimed"): our own submit is one of the two ways this
                // client learns a turn started, the other being
                // `ServerEvent::TurnStarted` (`mark_turn_liveness`). A shell
                // context's submit runs as shell and starts no turn.
                if !is_shell {
                    app.mark_turn_running(ctx);
                }
            }
            Err(e) => {
                tracing::warn!(context = %ctx.short(), error = %format!("{e:#}"), "submit failed");
                app.note(format!("submit failed: {e:#}"));
            }
        }
    }
    Ok(())
}

/// What a prefix chord does over the full-screen editor or diff viewer.
#[derive(Debug, PartialEq, Eq)]
enum OverFullScreen {
    /// Arming, and the chords that move between seats.
    Run,
    /// `Ctrl+A a`: the literal `Ctrl+A` belongs to the surface, not the draft.
    ToSurface(crossterm::event::KeyEvent),
    /// A chord for a surface that is not drawn here.
    Hold,
}

fn chord_over_full_screen(intent: &Intent) -> OverFullScreen {
    match intent {
        Intent::Ignored | Intent::LegendChanged | Intent::SwitchSeat(_) | Intent::StepSeat(_)
        | Intent::LastContext => OverFullScreen::Run,
        Intent::InputKey(key) => OverFullScreen::ToSurface(*key),
        _ => OverFullScreen::Hold,
    }
}

/// What a prefix chord does to the picker or ledger it fired over.
#[derive(Debug, PartialEq, Eq)]
enum OverOverlay {
    /// The overlay stays and the intent runs: arming, a notice.
    Keep,
    /// The overlay stays and the intent is dropped: it was draft text.
    Hold,
    /// The chord is the overlay's own toggle: it comes down, nothing else.
    Close,
    /// The chord goes somewhere else: the overlay comes down first.
    CloseAndRun,
}

/// The prefix pops over the picker and the ledger (`docs/tui.md`, "Keys").
/// A chord that leaves for another surface takes the overlay down on the way;
/// one that only changes the legend or posts a notice leaves it up.
fn chord_over_overlay(intent: &Intent, picker: bool, ledger: bool) -> OverOverlay {
    match intent {
        Intent::Ignored | Intent::LegendChanged | Intent::NotYet(_) | Intent::Unbound(_)
        | Intent::Interrupt => OverOverlay::Keep,
        Intent::InputKey(_) | Intent::Tab | Intent::Paste => OverOverlay::Hold,
        Intent::TogglePicker if picker => OverOverlay::Close,
        Intent::OpenLedger if ledger => OverOverlay::Close,
        _ => OverOverlay::CloseAndRun,
    }
}

/// A key while a full-screen surface has the screen.
fn act_full_screen(app: &mut App, key: KeyEvent, term_lock: &TermLock) {
    if let Some(session) = app.screen.editor().map(|s| s.session) {
        // A key with no notation is refused rather than sent: the kernel's
        // `parse_keys` drops an unknown `<...>` token silently, so forwarding
        // one would look like a working key that does nothing.
        let Some(notation) = editor::key_notation(&key) else {
            return;
        };
        // Queued on the editor outbox: one call in flight, keys behind it
        // batched, and the answer lands through `land_editor_answer`.
        if let Err(e) = app.editor_keys.keys(session, &notation) {
            app.note(format!("editor key not sent: {e:#}"));
        }
        return;
    }

    if let ScreenMode::Diff(screen) = &mut app.screen {
        let body_h = screen.body_h();
        match diff::handle_key(screen, &key, body_h) {
            DiffKey::Close => app.screen = ScreenMode::Conversation,
            // The registers take the hunk as whole lines, and the clipboard
            // as copy mode's yank does.
            DiffKey::Yank(Some(text)) => {
                match write_osc52(term_lock, &text) {
                    Ok(()) => app.note(format!("yanked {} lines — p or Ctrl+A ] puts", text.lines().count())),
                    Err(e) => app.note(format!("clipboard write failed: {e}")),
                }
                app.compose.yank_lines(&text);
            }
            DiffKey::Yank(None) => app.note("no hunk to yank"),
            DiffKey::Scrolled | DiffKey::Ignored => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::EditorOpen;
    use kaijutsu_types::ContextId;
    use crate::run::feeds::tests::{feeds_for_test, watched};

    /// A writer the test reads back: what a subscriber logged.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A key the picker reads never reaches `act`'s key line, so the picker
    /// logs its own on the same target, with what the key resolved to. An
    /// Enter on a picker with no rows resolves to nothing, and says so.
    #[test]
    fn a_picker_key_is_on_the_key_trace_with_its_outcome() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("build a runtime");
        let bridge = KernelBridge::never_answers_for_test();
        let mut app = App::new("amy");
        app.open_picker(0);
        let (mut feeds, _hydrated) = feeds_for_test();
        let enter = KeyEvent::new(crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(handle_picker_key(&bridge, &mut app, enter, false, &mut feeds)).expect("the key is handled");
        });
        let log = String::from_utf8(captured.0.lock().clone()).expect("utf-8");
        let line = log.lines().find(|l| l.contains("kaijutsu_tui::keys")).unwrap_or_else(|| panic!("no key line: {log:?}"));
        for field in ["code=Enter", "armed=false", "surface=\"picker\"", "outcome=None"] {
            assert!(line.contains(field), "{field} missing from {line:?}");
        }
        assert!(app.picker.is_some(), "an Enter that resolved to nothing leaves the picker up");
    }

    /// A refused submit leaves the draft as typed and marks nothing, so the
    /// kernel's copy is still the draft to read.
    #[test]
    fn a_refused_submit_leaves_the_draft_as_typed() {
        use kaijutsu_types::{BlockId, BlockSnapshot, PrincipalId, Role};
        let id = ContextId::new();
        let principal = PrincipalId::new();
        let mut app = App::new("amy");
        app.principal = Some(principal);
        app.current = Some(id);
        let mut draft = BlockSnapshot::text(BlockId::new(id, principal, 1), None, Role::User, "echo hi");
        draft.status = kaijutsu_types::Status::Draft;
        let mut mirror = kaijutsu_client::ContextMirror::new(id);
        mirror.apply_snapshot(vec![draft], 3).unwrap();
        app.views.insert(id, crate::app::ContextView::new(mirror));
        app.compose = crate::compose::Compose::over("echo hi");
        // `App::new`'s outbox has no writer, so the submit is refused.
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("build a runtime");
        let bridge = KernelBridge::never_answers_for_test();
        let (mut feeds, _hydrated) = feeds_for_test();
        let enter = KeyEvent::new(crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        let term_lock = TermLock::default();
        runtime.block_on(compose_key(&bridge, &mut app, enter, &mut feeds, &term_lock)).expect("the key is handled");
        assert_eq!(app.compose.text(), "echo hi");
        assert!(app.notice().is_some_and(|n| n.starts_with("submit failed")), "{:?}", app.notice());
        assert_eq!(app.current_draft().map(|(text, _)| text).as_deref(), Some("echo hi"));
    }

    #[test]
    fn over_a_full_screen_surface_the_prefix_moves_seats_and_feeds_vim() {
        let literal = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(chord_over_full_screen(&Intent::LegendChanged), OverFullScreen::Run);
        assert_eq!(chord_over_full_screen(&Intent::SwitchSeat(3)), OverFullScreen::Run);
        assert_eq!(chord_over_full_screen(&Intent::LastContext), OverFullScreen::Run);
        assert_eq!(chord_over_full_screen(&Intent::InputKey(literal)), OverFullScreen::ToSurface(literal));
        assert_eq!(chord_over_full_screen(&Intent::TogglePicker), OverFullScreen::Hold);
        assert_eq!(chord_over_full_screen(&Intent::PrefillKj("kj context rotate ")), OverFullScreen::Hold);
    }

    /// Arming keeps the overlay, a seat switch takes it down, and an
    /// overlay's own chord only closes it.
    #[test]
    fn a_chord_over_an_overlay_closes_it_only_when_it_leaves() {
        assert_eq!(chord_over_overlay(&Intent::LegendChanged, true, false), OverOverlay::Keep);
        assert_eq!(chord_over_overlay(&Intent::SwitchSeat(2), true, false), OverOverlay::CloseAndRun);
        assert_eq!(chord_over_overlay(&Intent::LastContext, false, true), OverOverlay::CloseAndRun);
        assert_eq!(chord_over_overlay(&Intent::TogglePicker, true, false), OverOverlay::Close);
        assert_eq!(chord_over_overlay(&Intent::TogglePicker, false, true), OverOverlay::CloseAndRun);
        assert_eq!(chord_over_overlay(&Intent::OpenLedger, false, true), OverOverlay::Close);
        assert_eq!(chord_over_overlay(&Intent::Paste, true, false), OverOverlay::Hold);
        assert_eq!(chord_over_overlay(&Intent::Unbound(crossterm::event::KeyCode::Char('x')), true, false), OverOverlay::Keep);
    }

    /// `Ctrl+O` is never one of copy mode's keys: it folds where the reader
    /// is, so the scrolled view must not take it and snap to the tail. A
    /// motion still belongs to the view.
    #[test]
    fn the_scrolled_transcript_leaves_ctrl_o_to_the_fold_toggle() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let id = ContextId::new();
        let mut app = App::new("amy");
        watched(&mut app, id);
        app.switch_to(id);
        assert!(app.set_scrolled(copy::Scrolled::entering(10)));
        let keys = Keys::new();
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!scrolled_takes_key(&app, false, &keys, &ctrl_o), "Ctrl+O folds in place");
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(scrolled_takes_key(&app, false, &keys, &j), "a motion is the view's");
    }

    /// `Up` at the top edge of the draft scrolls the transcript and `Down`
    /// at the live tail does nothing — one rule for a typed arrow and for
    /// the wheel (`docs/tui.md`, "The mouse stays the terminal's").
    #[test]
    fn up_scrolls_only_once_the_drafts_cursor_is_on_its_first_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut app = App::new("amy");

        // A one-row draft is the common case, so every `Up` scrolls.
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Nothing, "the tail is already on screen");

        // Inside a taller draft `Up` moves the draft's cursor first, as vim
        // does, and scrolls once it is on the first row.
        app.compose.load_draft("one\ntwo");
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::Draft);
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Nothing, "the cursor is on the last row");
        app.compose.press(up);
        assert!(app.compose.cursor_on_first_row(80), "the draft's own cursor moved up");
        assert_eq!(tail_key(&mut app, &up, 80), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 80), TailKey::Draft, "there is a draft row below");
    }

    /// The rows are the drawn ones, not the logical lines: one long line
    /// wraps, and an `Up` from a continuation row belongs to the draft.
    #[test]
    fn a_wrapped_one_line_draft_keeps_its_own_arrows() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let mut app = App::new("amy");
        // One logical line, three rows at twenty columns; `load_draft` rests
        // the cursor on the last character.
        app.compose.load_draft(&"x".repeat(50));
        assert_eq!(app.compose.cursor_visual_row(20).1, 3, "three drawn rows");
        assert_eq!(tail_key(&mut app, &up, 20), TailKey::Draft, "a continuation row is the draft's");
        assert_eq!(tail_key(&mut app, &down, 20), TailKey::Nothing, "the last row");

        // `0` is what reaches the first drawn row: the draft's own `Up` is
        // modalkit's logical-line motion, which has nowhere to go inside one
        // wrapped line — vim's `k` behaves the same, and `gk` is the motion
        // that would not (`docs/issues.md`).
        app.compose.press(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
        assert!(app.compose.cursor_on_first_row(20), "on the first drawn row now");
        assert_eq!(tail_key(&mut app, &up, 20), TailKey::LeaveTail(Step::Line));
        assert_eq!(tail_key(&mut app, &down, 20), TailKey::Draft, "rows below it in the draft");
    }

    #[test]
    fn the_page_keys_leave_the_tail_upward_and_do_nothing_downward() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        let page_down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(tail_key(&mut app, &page_up, 80), TailKey::LeaveTail(Step::Page));
        assert_eq!(tail_key(&mut app, &page_down, 80), TailKey::Nothing);
    }

    /// The `:` bar keeps `Up` and `Down` for its own history.
    #[test]
    fn the_colon_bar_keeps_the_arrow_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        app.compose.press(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert_eq!(tail_key(&mut app, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 80), TailKey::Draft);
        assert_eq!(tail_key(&mut app, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 80), TailKey::Draft);
    }

    /// Leaving the tail anchors the view where it already is, so the screen
    /// does not move, and a page step is the transcript's own height.
    #[test]
    fn leaving_the_tail_takes_the_transcripts_height_for_a_page() {
        let mut app = App::new("amy");
        let id = ContextId::new();
        watched(&mut app, id);
        app.switch_to(id);
        app.screen_rows = 24;
        leave_tail(&mut app, 80, Step::Page);
        let view = app.scrolled().expect("off the tail");
        assert_eq!(view.page(), 20, "24 rows less the band's four");
        assert!(!app.following());
    }

    #[test]
    fn a_paste_goes_to_the_draft_or_the_colon_bar_and_nowhere_else() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new("amy");
        assert_eq!(paste_target(&app), PasteTarget::Draft);

        app.compose.press(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert_eq!(paste_target(&app), PasteTarget::CommandLine);
        app.compose.press(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.open_picker(0);
        assert!(matches!(paste_target(&app), PasteTarget::Refused(_)), "the picker has no text field");
        app.picker = None;

        app.screen = ScreenMode::Diff(crate::diff::DiffScreen::unparsed(
            "probe",
            "not a diff",
            "one\ntwo",
            &app.palette,
        ));
        assert!(
            matches!(paste_target(&app), PasteTarget::Refused(_)),
            "the diff viewer has no editor session — still refused"
        );

        editor::enter_editor(
            &mut app,
            EditorOpen {
                state: kaijutsu_client::EditorState {
                    session: 7,
                    text: "hello".to_string(),
                    cursor: 0,
                    mode: None,
                    dirty: false,
                    command_line: None,
                    message: None,
                    selection: None,
                    path: "notes.kai".to_string(),
                    arg_index: 0,
                    arg_count: 1,
                },
            },
        );
        assert_eq!(
            paste_target(&app),
            PasteTarget::Editor(7),
            "an open editor session takes a paste at the cursor"
        );

        // With the editor's `:` line open the strip draws that line, not
        // `EditorState.message`, so the kernel's refusal message would never
        // be seen; the tui refuses first and says so.
        app.screen.editor_mut().expect("editor open").state.command_line = Some(":w".to_string());
        assert!(
            matches!(paste_target(&app), PasteTarget::Refused(_)),
            "a paste while the editor's ':' line is open is refused with a notice"
        );
    }

    #[test]
    fn a_pasted_newline_is_one_newline_whatever_the_terminal_sent() {
        assert_eq!(normalize_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }
}

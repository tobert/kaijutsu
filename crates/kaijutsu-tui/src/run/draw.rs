//! Drawing a frame, and the beat timer that wakes the loop for the next
//! onset.

use std::io::{self, Stdout};
use std::time::Instant;

use anyhow::{Context, Result};
use crossterm::terminal::{

    BeginSynchronizedUpdate, EndSynchronizedUpdate,
};
use kaijutsu_audio::RefDisposition;
use kaijutsu_types::ContextId;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::compose::CursorShape;
use crate::picker::{self};
use crate::render;

use crate::editor::{self, ScreenMode};

use super::terminal::{TermLock, set_cursor_shape, set_title, wanted_cursor_shape, wanted_title};

/// What the terminal was last told beyond the frame itself.
#[derive(Default)]
pub(super) struct Presented {
    /// The cursor shape last sent; `None` until the first frame and again
    /// after a suspend, since the host shell may have set its own.
    pub(super) cursor_shape: Option<CursorShape>,
    /// The window title last sent; `None` until the first context is on
    /// screen and again after a suspend, since the host shell titled the
    /// window while it had it.
    pub(super) title: Option<String>,
    /// The screen mode the last frame drew, so crossing between the
    /// conversation and a full-screen surface can say the cursor shape
    /// again.
    full_screen: bool,
}

/// Draw a frame, then bring the cursor shape and title up to date.
pub(super) fn present(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    app: &mut App,
    armed: bool,
    panic_in_frame: bool,
    presented: &mut Presented,
) -> Result<()> {
    draw(terminal, term_lock, app, armed, panic_in_frame)?;
    // A terminal may keep a cursor shape per screen buffer, and a
    // full-screen surface owns its own, so crossing either way forgets what
    // was sent.
    if app.screen.is_full_screen() != presented.full_screen {
        presented.full_screen = app.screen.is_full_screen();
        presented.cursor_shape = None;
    }
    let _guard = term_lock.lock();
    set_cursor_shape(&mut presented.cursor_shape, wanted_cursor_shape(app))?;
    set_title(&mut presented.title, wanted_title(app))?;
    Ok(())
}

/// Fold one `ServerEvent::BeatSync` into `app.beats`, the same
/// fold/touch/drop routing as the app's `time_well::live::ingest_live_events`
/// (`docs/tui.md`, "TRACKS + beat"). `now_epoch_ns` is the kernel-domain
/// wallclock (`ActorHandle::kernel_now_ns`), so the age ladder runs on the
/// one timebase (`docs/midi.md`, "The one timebase").
pub(super) fn observe_beat_sync(
    app: &mut App,
    context_id: ContextId,
    beat_ref: kaijutsu_audio::BeatRef,
    now_epoch_ns: u64,
) {
    let now_inst = Instant::now();
    match beat_ref.disposition(now_inst, now_epoch_ns) {
        RefDisposition::Fold(at) => {
            app.beats.observe(context_id, beat_ref, at, now_inst);
        }
        RefDisposition::Touch | RefDisposition::Drop => {
            app.beats.touch(&context_id, now_inst);
        }
    }
}

/// The beat timer's state: when the loop next wakes for the playing track's
/// onset, and the tempo that wake is re-armed with.
pub(super) struct Beat {
    /// `None` while the timer is off.
    pub(super) wake: Option<Instant>,
    pub(super) tempo_bps: f64,
}

/// Whether the beat timer should be armed at all: a track is playing and
/// someone is looking. Every [`rearm_beat_wake`] goes through it, so the
/// refresh round cannot re-arm what a `FocusLost` disarmed
/// (`docs/tui.md`, "What owning the screen lets us use").
fn beat_wake_armed(app: &App) -> bool {
    app.focused && app.playing_track().is_some()
}

/// Re-arm the beat timer from the playing track's live phasor position and
/// its last-polled tempo. `None` while nothing is playing or its phasor
/// hasn't anchored yet — the timer arm's `if ls.beat.wake.is_some()` guard
/// then simply stays off until the next refresh finds one.
pub(super) fn rearm_beat_wake(app: &App, beat: &mut Beat) {
    if !beat_wake_armed(app) {
        beat.wake = None;
        return;
    }
    let Some(track) = app.playing_track() else {
        beat.wake = None;
        return;
    };
    let now = Instant::now();
    let Some(position) = app.beats.beat_position(&track.score_context_id, now) else {
        beat.wake = None;
        return;
    };
    beat.tempo_bps = track.tempo_bps();
    beat.wake = Some(picker::next_onset(position, beat.tempo_bps, now));
}

/// One frame of the owned screen, inside one synchronized update.
///
/// `?2026` brackets the frame so a terminal that supports it shows the whole
/// redraw at once; one that does not ignores the two sequences.
fn draw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    term_lock: &TermLock,
    app: &mut App,
    armed: bool,
    probe_panic: bool,
) -> Result<()> {
    // The status line's live pulse dot — the one place this crate samples
    // the phasor's envelope against a real clock; `App::track_figure` only
    // projects the value stamped here.
    app.track_pulse = app
        .playing_track()
        .is_some_and(|t| app.beats.envelope(&t.score_context_id, Instant::now()) > 0.5);

    let _guard = term_lock.lock();
    crossterm::execute!(io::stdout(), BeginSynchronizedUpdate).context("begin a frame")?;
    let drawn = draw_frame(terminal, app, armed, probe_panic);
    // The update is ended whatever the frame did: a terminal left inside a
    // synchronized update shows nothing at all. A panic unwinds past this
    // one, which is why `restore_terminal` ends it too.
    let ended = crossterm::execute!(io::stdout(), EndSynchronizedUpdate).context("end a frame");
    // The frame's own error is the one that says what went wrong.
    drawn?;
    ended
}

/// The frame itself: a full-screen surface when one has the screen, the
/// conversation otherwise.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    armed: bool,
    probe_panic: bool,
) -> Result<()> {
    assert!(!probe_panic, "KAIJUTSU_TUI_PROBE_PANIC: panicking inside a frame");
    let size = terminal.size()?;
    let palette = app.palette;
    match &mut app.screen {
        ScreenMode::Conversation => {
            render::draw_screen(terminal, app, kaijutsu_types::now_millis(), armed)?;
        }
        ScreenMode::Editor(screen) => {
            let frame = editor::editor_frame(screen, size.width, size.height, &palette);
            render::draw_surface(terminal, frame.lines, Some(frame.cursor))?;
        }
        ScreenMode::Diff(screen) => {
            let lines = screen.frame(size.height, &palette);
            render::draw_surface(terminal, lines, None)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The beat timer is armed only while someone is looking: the refresh
    /// round re-arms it on its own cadence, and an unfocused client that
    /// re-armed there would animate the pulse again a round after the
    /// `FocusLost` that stopped it.
    #[test]
    fn the_beat_timer_stays_disarmed_while_unfocused() {
        let mut app = App::new("amy");
        app.tracks = vec![crate::picker::TrackRow {
            id: "t1".to_string(),
            score_context_id: ContextId::new(),
            playing: true,
            bpm: 120,
            bar: 1,
            beat: 1,
        }];
        assert!(beat_wake_armed(&app), "a playing track arms the timer while focused");
        app.focused = false;
        assert!(!beat_wake_armed(&app), "nothing animates for a terminal nobody is looking at");

        let mut beat = Beat { wake: Some(Instant::now()), tempo_bps: 2.0 };
        rearm_beat_wake(&app, &mut beat);
        assert_eq!(beat.wake, None, "an unfocused re-arm disarms instead");
    }
}

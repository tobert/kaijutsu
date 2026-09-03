//! Terminal-fit integration probes: the real `kaijutsu-tui` binary, in a
//! pty, against an in-process ephemeral kernel (`support::EphemeralServer`).
//!
//! The crate's 207 unit tests render pure functions onto a
//! `ratatui::TestBackend`; nothing there exercises the inline viewport
//! itself — `insert_before` into scrollback, viewport regrow, resize, or
//! terminal restore. That is what these probes cover.
//!
//! Each probe gets its own server (its own kernel, its own tempdir) and its
//! own pty, and the probes run one at a time (`support::serial`). Slow
//! relative to the unit suite: a real SSH handshake and a real child
//! process per probe.
//!
//! ```bash
//! cargo test -p kaijutsu-tui --test terminal_fit
//! ```
//!
//! A first pass of these probes assumed a freshly created context draws a
//! blank scrollback above the viewport. That is false: a `coder` context's
//! create-time rc lifecycle (`assets/defaults/rc/coder/create/`) prints its
//! own trace/tool blocks into scrollback before the live view ever draws,
//! same as any other completed block (`docs/tui.md`, the transcript flows
//! into the terminal's own scrollback). So "fits" here means the live
//! region renders exactly once, inside its reserved band, and never leaks
//! into — or is contaminated by — the scrollback area above it, not that
//! the scrollback is empty.

mod support;

use std::time::Duration;

use kaijutsu_tui::render::VIEWPORT_LINES;
use support::{EphemeralServer, TuiSession, serial, write_ephemeral_key};

/// How long to wait for the client to connect and render its first frame.
/// The ephemeral server boots in well under a second; this is generous for
/// a loaded CI box, not a measurement of steady-state latency.
const CONNECT: Duration = Duration::from_secs(15);

fn spawn_session(rows: u16, cols: u16) -> (EphemeralServer, tempfile::TempDir, TuiSession) {
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("tempdir for the ephemeral key");
    let key_path = write_ephemeral_key(key_dir.path());
    let session = TuiSession::spawn(server.addr, &key_path, rows, cols);
    (server, key_dir, session)
}

/// Wait for the compose prompt (`crate::compose::PROMPT`, `❯ `) to appear
/// anywhere on screen — the one thing that only renders once the client has
/// attached to a context and drawn its first live frame.
fn wait_for_attach(session: &TuiSession) {
    let attached = session.wait_until(CONNECT, |screen| screen_contains(screen, '❯'));
    assert!(attached, "{}", session.dump("never attached"));
}

fn screen_contains(screen: &vt100::Screen, needle: char) -> bool {
    screen.rows(0, screen.size().1).any(|line| line.contains(needle))
}

fn screen_contains_str(screen: &vt100::Screen, needle: &str) -> bool {
    screen.rows(0, screen.size().1).any(|line| line.contains(needle))
}

/// Row index of the compose prompt (`❯`) — the first row of the live region
/// whenever the picker is not open.
fn compose_row(rows: &[String]) -> Option<usize> {
    rows.iter().position(|l| l.contains('❯'))
}

/// Row index of the picker's `ACTIVE` header — always the first line the
/// picker renders (`crates/kaijutsu-tui/src/picker.rs`'s `render`), so this
/// is the first row of the picker's own reserved band.
fn picker_row(rows: &[String]) -> Option<usize> {
    rows.iter().position(|l| l.contains("ACTIVE"))
}

// ────────────────────────────────────────────────────────────────────────────
// a. Startup fits inside the viewport at 80x24
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn startup_fits_inside_the_viewport_at_80x24() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    let rows = session.screen_text();
    assert_eq!(rows.len(), 24, "{}", session.dump("startup"));

    // With nothing streaming, the live region is just compose + status: two
    // lines, bottom-aligned inside the reserved `VIEWPORT_LINES`-row band so
    // the status line is the terminal's last row (`docs/tui.md`, ruling 1:
    // "a viewport at the bottom"). The band's unused rows are the blank gap
    // above compose, never a gap under the status line.
    let band_start = 24 - usize::from(VIEWPORT_LINES);
    let band = &rows[band_start..];
    assert_eq!(
        compose_row(&rows),
        Some(22),
        "expected compose on the second-to-last row, directly above the status line:\n{}",
        session.dump("startup")
    );
    assert!(!rows[23].trim().is_empty(), "expected the status line on the last row:\n{}", session.dump("startup"));
    for (offset, line) in band.iter().enumerate().take(band.len() - 2) {
        assert!(
            line.trim().is_empty(),
            "row {} in the live band was unexpectedly non-blank: {line:?}\n{}",
            band_start + offset,
            session.dump("startup")
        );
    }

    // The live region renders exactly once: it must never leak into the
    // scrollback area above its reserved band.
    for (i, line) in rows.iter().enumerate().take(band_start) {
        assert!(!line.contains('❯'), "the compose prompt leaked into scrollback at row {i}: {line:?}\n{}", session.dump("startup"));
    }

    // No row's rendered content is wider than the terminal.
    for line in &rows {
        assert!(line.chars().count() <= 80, "row spilled past 80 cols: {line:?}");
    }
}

// ────────────────────────────────────────────────────────────────────────────
// b. Typing lands in compose
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn typing_lands_on_the_compose_row() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("hello");

    let ok = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains('❯') && line.contains("hello"))
    });
    assert!(ok, "{}", session.dump("after typing hello"));
}

// ────────────────────────────────────────────────────────────────────────────
// c. Picker grows and shrinks cleanly
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn the_picker_grows_and_shrinks_the_viewport_cleanly() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    let baseline_rows = session.screen_text();
    let baseline_top = compose_row(&baseline_rows).expect("compose row is drawn at startup");

    // Ctrl+A, then `"` — opens the picker (`crates/kaijutsu-tui/src/keys.rs`
    // `ctrl_a_quote_opens_the_picker`).
    session.send("\x01\"");
    let opened = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "ACTIVE"));
    assert!(opened, "picker never opened: {}", session.dump("picker open"));

    let grown_rows = session.screen_text();
    let grown_top = picker_row(&grown_rows).expect("picker's ACTIVE header is drawn");
    assert!(
        grown_top <= baseline_top,
        "the picker's reserved band starts lower than the base viewport's, i.e. it did not grow \
         (grown_top={grown_top}, baseline_top={baseline_top}):\n{}",
        session.dump("picker open")
    );

    // Esc closes it (`picker.rs`: `KeyCode::Esc => Outcome::Dismiss`).
    session.send("\x1b");
    let closed = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "ACTIVE") && screen_contains(screen, '❯')
    });
    assert!(closed, "picker never closed: {}", session.dump("picker close"));

    let closed_rows = session.screen_text();
    let closed_top = compose_row(&closed_rows).expect("compose row is drawn again after closing");
    assert_eq!(
        closed_top, baseline_top,
        "the viewport did not return to its original height after closing the picker:\n{}",
        session.dump("after closing picker")
    );

    // Nothing of the picker's own UI is left on screen once it is closed.
    //
    // This probe originally tried to diff the *entire* printed transcript
    // above the viewport before and after — it does not hold up. Two
    // sources of noise land there independent of the picker: connection-time
    // stderr diagnostics (`--insecure`'s key-acceptance warning; `tracing`
    // writes straight to the shared pty stream, not through `insert_before`,
    // so it is not cursor-tracked the way ratatui's own content is) and the
    // app's own housekeeping (the ledger poll on `REFRESH`, `run.rs`, which
    // settles and prints a real block in the wall-clock gap this probe's
    // waits leave). Both can shift or displace already-printed lines by the
    // time a second snapshot is taken, for reasons that have nothing to do
    // with the picker. A marker check on the picker's own vocabulary is what
    // survives that noise.
    for marker in ["ACTIVE", "RECENT", "TRACKS"] {
        assert!(
            !closed_rows.iter().any(|l| l.contains(marker)),
            "the picker's own UI (\"{marker}\") is still on screen after closing it:\n{}",
            session.dump("after closing picker")
        );
    }
    let closed_scrollback = session.scrollback_text();
    for marker in ["ACTIVE", "RECENT", "TRACKS"] {
        assert!(
            !closed_scrollback.iter().any(|l| l.contains(marker)),
            "the picker's own UI (\"{marker}\") leaked into scrollback: {closed_scrollback:?}"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// d. Resize keeps the live region intact and on screen
// ────────────────────────────────────────────────────────────────────────────

/// An inline viewport is not pinned to the bottom: like a shell prompt it
/// sits right after the transcript, and growing the terminal adds blank
/// rows below it rather than moving it down. What a resize must preserve
/// is the live region itself — drawn exactly once, at the new width, fully
/// on screen — and a shrink must pull it up rather than let it fall off the
/// bottom.
#[test]
fn resize_keeps_the_live_region_intact_and_on_screen() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    for (rows, cols) in [(30u16, 100u16), (20u16, 60u16)] {
        session.resize(rows, cols);
        // The parser's size changes immediately and `❯` is already on
        // screen, so wait for the app's own redraw at the new width: the
        // status line is the widest live row and is padded to `cols`.
        let settled = session.wait_until(Duration::from_secs(5), |screen| {
            screen.size() == (rows, cols)
                && screen.rows(0, cols).any(|line| line.contains('❯'))
        });
        assert!(settled, "never settled at {rows}x{cols}: {}", session.dump(&format!("after resize to {rows}x{cols}")));
        std::thread::sleep(Duration::from_millis(250));

        let (scrollback, text) = session.history_snapshot();
        let label = format!("after resize to {rows}x{cols}");
        assert_eq!(text.len(), rows as usize);

        let compose_rows: Vec<usize> = text
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains('❯'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(compose_rows.len(), 1, "the live region must be on screen exactly once:\n{}", session.dump(&label));
        let top = compose_rows[0];
        assert!(
            top + 1 < rows as usize,
            "the status line fell off the bottom of a {rows}-row terminal:\n{}",
            session.dump(&label)
        );
        assert!(
            !text[top + 1].trim().is_empty(),
            "expected the status line directly under compose:\n{}",
            session.dump(&label)
        );
        // A grow leaves the viewport where it was, like a shell prompt. Where
        // a shrink lands it is not asserted: `vt100` drops rows from the
        // bottom on a shrink, where a real terminal scrolls the top rows into
        // scrollback, so the re-anchor here does not match a terminal's.
        for line in &text {
            assert!(line.chars().count() <= cols as usize, "row spilled past {cols} cols: {line:?}");
        }
        assert!(
            !scrollback.iter().any(|l| l.contains('❯')),
            "a stale copy of the live region was left in scrollback by the resize: {scrollback:?}"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// e. Quit restores the terminal
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ctrl_c_twice_exits_cleanly_and_leaves_the_last_frame() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x03\x03");
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit: {}", session.dump("still running")));
    assert!(status.success(), "kaijutsu-tui exited with {status:?}: {}", session.dump("after Ctrl+C Ctrl+C"));

    // `leave_terminal` (`crates/kaijutsu-tui/src/run.rs:1051`) does not
    // clear the viewport on the way out — its last frame is left exactly
    // where it was drawn. A background refresh (the ledger poll on
    // `REFRESH`, `run.rs`) can settle one more block between our last
    // observation before quitting and the actual exit, so this checks the
    // documented contract (something real is left on screen) rather than a
    // byte-identical pre/post snapshot, which would race that poll.
    let after = session.screen_text();
    let non_blank = after.iter().filter(|l| !l.trim().is_empty()).count();
    assert!(
        non_blank > 0,
        "the screen was fully cleared on exit; leave_terminal's contract is to leave the last frame:\n{}",
        session.dump("after exit")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// f. Starting from a prompt mid-screen still reaches the bottom
// ────────────────────────────────────────────────────────────────────────────

/// A terminal whose shell prompt sat mid-screen when the binary started:
/// the viewport anchors at that row, and the create-lifecycle's trace
/// blocks — more lines than the rows left below it — must push it down to
/// the bottom band through `insert_before`'s not-yet-at-the-bottom path
/// (`ratatui-core`'s `insert_before_scrolling_regions`). A viewport that
/// stalls partway up the screen is the bug this probe is for.
#[test]
fn starting_from_a_prompt_mid_screen_reaches_the_bottom_band() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("tempdir for the ephemeral key");
    let key_path = write_ephemeral_key(key_dir.path());
    let session = TuiSession::spawn_after_newlines(server.addr, &key_path, 24, 80, 10);
    wait_for_attach(&session);

    // The rc trace blocks print as they settle; give the last one a moment.
    let reached = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        compose_row(&rows) == Some(22)
    });
    assert!(
        reached,
        "the viewport never reached the bottom (compose on row 22) after starting mid-screen:\n{}",
        session.dump("mid-screen start")
    );

    let (scrollback, text) = session.history_snapshot();
    assert_eq!(text.iter().filter(|l| l.contains('❯')).count(), 1, "{}", session.dump("mid-screen start"));
    assert!(
        !scrollback.iter().any(|l| l.contains('❯')),
        "a copy of the live region was left behind on the way down: {scrollback:?}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// g. A partial shell line never repeats into the transcript
// ────────────────────────────────────────────────────────────────────────────

/// The `Ctrl+Z` shell prompt is live-region content and nothing else: a
/// line being typed there must appear exactly once, on the prompt row, no
/// matter how many blocks land in the transcript while it is being typed.
/// The prompt is `<label> $ ` (`shell::prompt`; a fresh context has no
/// recorded cwd, so no path segment renders), which no printed block
/// carries, so a second row carrying it is a ghost.
#[test]
fn a_partial_shell_line_never_repeats_into_the_transcript() {
    let _serial = serial();
    const SEPARATOR: &str = "probe $ ";
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1a");
    let shell_up = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, SEPARATOR));
    assert!(shell_up, "the shell surface never came up: {}", session.dump("after Ctrl+Z"));

    // A whole command lands blocks in the transcript; the partial line typed
    // right behind it is on the prompt while they arrive.
    session.send("kj context list\r");
    session.send("zz partial");
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        rows.iter().any(|l| l.contains(SEPARATOR) && l.contains("zz partial"))
            && rows.iter().any(|l| l.contains("kj context list") && !l.contains(SEPARATOR))
    });
    assert!(landed, "{}", session.dump("after the command's blocks landed"));
    std::thread::sleep(Duration::from_millis(500));

    let (scrollback, text) = session.history_snapshot();
    let prompt_rows: Vec<&String> = text.iter().filter(|l| l.contains(SEPARATOR)).collect();
    assert_eq!(
        prompt_rows.len(),
        1,
        "the shell prompt must be on screen exactly once:\n{}",
        session.dump("after the command's blocks landed")
    );
    assert!(prompt_rows[0].contains("zz partial"), "the partial line left the prompt: {}", session.dump("prompt"));
    let ghosts: Vec<&String> = scrollback.iter().filter(|l| l.contains(SEPARATOR)).collect();
    assert!(ghosts.is_empty(), "shell prompt rows leaked into the transcript: {ghosts:?}");
    assert!(
        !scrollback.iter().chain(text.iter()).any(|l| l.contains("zz partial") && !l.contains(SEPARATOR)),
        "the partial line was echoed as transcript: {}",
        session.dump("partial echoed")
    );
}

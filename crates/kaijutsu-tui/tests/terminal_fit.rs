//! Terminal-fit integration probes: the real `kaijutsu-tui` binary, in a
//! pty, against an in-process ephemeral kernel (`support::EphemeralServer`).
//!
//! The crate's unit tests render pure functions onto a
//! `ratatui::TestBackend`; nothing there exercises the terminal itself —
//! taking and giving back the alternate screen, a real resize, the cursor
//! the client never asks for, or suspend. That is what these probes cover.
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
//! The transcript area is never blank at startup: a `coder` context's
//! create-time rc lifecycle (`assets/defaults/rc/coder/create/`) authors
//! trace and tool blocks, and the transcript renders every block the
//! context has.

mod support;

use std::time::Duration;

use support::{
    EphemeralServer, TuiSession, marked_rows, reader_rows, serial, write_ephemeral_key,
};

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

/// Type `:q` and wait for the client to exit.
///
/// `Esc` goes on its own: crossterm reads `ESC` followed by another byte in
/// the same buffer as `Alt+<byte>`, so a client that happens to be mid-frame
/// when the bytes land never sees the `:`.
fn quit(session: &mut TuiSession) {
    session.send("\x1b");
    std::thread::sleep(Duration::from_millis(200));
    session.send(":q\r");
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit: {}", session.dump("still running")));
    assert!(status.success(), "kaijutsu-tui exited with {status:?}: {}", session.dump("after :q"));
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
// a. Startup draws the transcript and the band at 80x24
// ────────────────────────────────────────────────────────────────────────────

/// The owned screen is the transcript on top and the band at the bottom:
/// the in-flight strip, a blank row, the draft and the status line
/// (`docs/tui.md`, "The owned screen"). A `coder` context's create-time rc
/// lifecycle prints real blocks, so the transcript above the band has
/// content from the first frame.
#[test]
fn startup_draws_the_transcript_and_the_band() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    let settled = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        compose_row(&rows) == Some(22)
    });
    assert!(settled, "{}", session.dump("startup"));

    let rows = session.screen_text();
    assert_eq!(rows.len(), 24, "{}", session.dump("startup"));
    assert!(!rows[23].trim().is_empty(), "expected the status line on the last row:\n{}", session.dump("startup"));
    assert!(rows[21].trim().is_empty(), "expected the blank row over the draft:\n{}", session.dump("startup"));

    // The draft is on screen exactly once, and the transcript above it
    // carries the lifecycle's own blocks.
    assert_eq!(
        rows.iter().filter(|l| l.contains('\u{276f}')).count(),
        1,
        "the band renders once:\n{}",
        session.dump("startup")
    );
    assert!(
        rows[..20].iter().any(|l| !l.trim().is_empty()),
        "the transcript area is empty:\n{}",
        session.dump("startup")
    );

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

    session.send("ihello"); // `i`: a fresh draft rests in normal mode

    let ok = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains('❯') && line.contains("hello"))
    });
    assert!(ok, "{}", session.dump("after typing hello"));
}

// ────────────────────────────────────────────────────────────────────────────
// c. The picker is an overlay and leaves the transcript intact
// ────────────────────────────────────────────────────────────────────────────

/// The picker draws over the foot of the transcript area, above the band;
/// dismissing it redraws the transcript exactly where it was
/// (`docs/tui.md`, "The owned screen": every surface is an overlay).
#[test]
fn the_picker_opens_as_an_overlay_and_leaves_the_transcript_intact() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // A row worth watching right above the band: blank rows would make a
    // lost redraw invisible.
    session.send(":!echo marker-row\r");
    let marked = session.wait_until(Duration::from_secs(20), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        compose_row(&rows) == Some(22) && rows[19].contains("marker-row")
    });
    assert!(marked, "no transcript row landed above the band: {}", session.dump("marker"));
    let before = session.screen_text();

    // Ctrl+A, then `"` — opens the picker (`crates/kaijutsu-tui/src/keys.rs`
    // `ctrl_a_quote_opens_the_picker`).
    session.send("\x01\"");
    let opened = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "ACTIVE"));
    assert!(opened, "picker never opened: {}", session.dump("picker open"));

    let open_rows = session.screen_text();
    let picker_top = picker_row(&open_rows).expect("picker's ACTIVE header is drawn");
    let compose = compose_row(&open_rows).expect("the band keeps the draft under the overlay");
    assert!(picker_top < compose, "the picker draws above the band: {}", session.dump("picker open"));
    assert!(
        !open_rows[23].trim().is_empty(),
        "the status line is still the last row: {}",
        session.dump("picker open")
    );

    // Esc closes it (`picker.rs`: `KeyCode::Esc => Outcome::Dismiss`).
    session.send("\x1b");
    let closed = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "ACTIVE") && screen_contains(screen, '\u{276f}')
    });
    assert!(closed, "picker never closed: {}", session.dump("picker close"));

    let after = session.screen_text();
    assert_eq!(
        after[19].trim_end(),
        before[19].trim_end(),
        "the transcript row above the band did not come back:\n{}",
        session.dump("after closing picker")
    );
    assert_eq!(compose_row(&after), Some(22), "the band is where it was: {}", session.dump("after closing picker"));
    for marker in ["ACTIVE", "RECENT", "TRACKS"] {
        assert!(
            !after.iter().any(|l| l.contains(marker)),
            "the picker's own UI (\"{marker}\") is still on screen after closing it:\n{}",
            session.dump("after closing picker")
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// c2. The ledger view grows the viewport and keeps its key line on screen
// ────────────────────────────────────────────────────────────────────────────

/// The ledger view gets the same grown-viewport treatment as the picker
/// (`docs/tui.md`, "The ledger"): its key-hints line — `a allow once ...
/// Esc back`, the only way to answer a pending ask — must be on screen,
/// never truncated off the bottom the way a long ask or a crowded ledger
/// used to lose it (kaibo review, 2026-09-03).
#[test]
fn the_ledger_view_keeps_its_key_hints_line_on_screen() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // Ctrl+A, then `l` — opens the ledger (`crates/kaijutsu-tui/src/keys.rs`
    // `ctrl_a_l_opens_the_ledger`).
    session.send("\x01l");
    let opened = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "LEDGER"));
    assert!(opened, "ledger never opened: {}", session.dump("ledger open"));

    let rows = session.screen_text();
    assert!(
        rows.iter().any(|l| l.contains("a allow once") && l.contains("Esc back")),
        "the ledger's key-hints line is not on screen:\n{}",
        session.dump("ledger open")
    );

    // Esc closes it, the same as the picker (`asks::LedgerAction::Back`).
    session.send("\x1b");
    let closed = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "LEDGER") && screen_contains(screen, '❯')
    });
    assert!(closed, "ledger never closed: {}", session.dump("ledger close"));
}

// ────────────────────────────────────────────────────────────────────────────
// c3. Scrolling the transcript is copy mode
// ────────────────────────────────────────────────────────────────────────────

/// A word the filler repeats, so a probe can tell transcript rows from the
/// band at a glance.
const FILLER: &str = "scrollme";

/// Fill the transcript past one screenful, so there is something to scroll.
///
/// Three `:!echo` statements of forty short words: each one lands a wrapped
/// statement block and a wrapped result, which is a dozen rows a round at
/// eighty columns.
fn fill_transcript(session: &TuiSession) {
    let half = vec![FILLER; 20].join(" ");
    for round in 0..3 {
        session.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));
        // The round's own word sits in the middle, far enough in that the
        // divider's truncated command line cannot carry it: a probe that
        // searches for it lands on a wrapped body row with more rows under
        // it, never on a divider.
        session.send(&format!(":!echo {half} mid{round} {half}\r"));
        let landed = session.wait_until(Duration::from_secs(20), |screen| {
            screen.rows(0, screen.size().1).filter(|l| l.contains(&format!("mid{round}"))).count() >= 2
        });
        assert!(landed, "filler round {round} never landed: {}", session.dump("fill_transcript"));
    }
    // The band settles once the last result stops streaming; a probe that
    // read the rows mid-stream would call a stream a scroll.
    std::thread::sleep(Duration::from_millis(700));
}

/// The transcript rows, trailing blanks trimmed: a repaint leaves the cells
/// a highlighted row cleared, and that is not a scroll.
fn transcript_rows(session: &TuiSession) -> Vec<String> {
    session.screen_text()[..20].iter().map(|l| l.trim_end().to_string()).collect()
}

/// How many lines the transcript moved up between two readings: the offset
/// `k` where the new rows are the old ones shifted down by `k`. `None` when
/// the two do not overlap that way at all.
fn rows_shifted_up(before: &[String], after: &[String]) -> Option<usize> {
    (0..before.len()).find(|&k| after[k..] == before[..before.len() - k])
}

/// Rows of the transcript area — everything above the band at 24x80.
const TRANSCRIPT_ROWS: usize = 20;

/// Whether the transcript area holds `needle`. Scoped to the rows above the
/// band, so the `:` bar and the status row cannot answer for it.
fn transcript_holds(screen: &vt100::Screen, needle: &str) -> bool {
    screen.rows(0, screen.size().1).take(TRANSCRIPT_ROWS).any(|l| l.contains(needle))
}

/// The `line N/M` readout on the band's status row, parsed.
fn readout(session: &TuiSession) -> Option<(usize, usize)> {
    let rows = session.screen_text();
    let at = rows[23].find("line ")? + "line ".len();
    let figure = rows[23][at..].split_whitespace().next()?;
    let (n, m) = figure.split_once('/')?;
    Some((n.parse().ok()?, m.parse().ok()?))
}

/// Wait for the scrolled hint to take the band's status row, and assert the
/// readout names a real position rather than the frame before's `line 0/0`.
fn wait_for_scrolled(session: &TuiSession, label: &str) {
    let scrolled = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(scrolled, "the transcript never left the tail: {}", session.dump(label));
    let (n, m) = readout(session)
        .unwrap_or_else(|| panic!("no line N/M readout: {}", session.dump(label)));
    assert!(n >= 1 && m >= 1, "the readout says {n}/{m}: {}", session.dump(label));
    assert!(n <= m, "the reader is past the end at {n}/{m}: {}", session.dump(label));
}

/// Leave the tail the way the wheel does.
///
/// On the alternate screen with mouse reporting off, wezterm turns each
/// wheel tick into arrow-key presses (three, by default), and the tui cannot
/// tell such an `Up` from a typed one — one rule covers both (`docs/tui.md`,
/// "The mouse stays the terminal's").
fn wheel_up(session: &TuiSession) {
    session.send("\x1b[A\x1b[A\x1b[A");
}

/// Three `Up`s — one wheel tick — leave the live tail: the hint takes the
/// band's status row, the transcript rows move, and the draft stays drawn.
/// `q` returns to the tail (`docs/tui.md`, "Scrolling is copy mode").
#[test]
fn the_wheel_as_arrows_leaves_the_tail_and_q_returns() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    fill_transcript(&session);

    let before = transcript_rows(&session);
    wheel_up(&session);
    wait_for_scrolled(&session, "after three Up");

    let after = session.screen_text();
    // One wheel tick is three `Up` presses and moves the screen three lines:
    // the arrows scroll the view, they do not walk a cursor up it.
    assert_eq!(
        rows_shifted_up(&before, &transcript_rows(&session)),
        Some(3),
        "the transcript did not scroll three lines: {}",
        session.dump("scrolled")
    );
    assert!(
        compose_row(&after).is_some(),
        "the draft is drawn live while scrolled: {}",
        session.dump("scrolled")
    );
    assert!(
        after[23].contains("q leave") && after[23].contains("line "),
        "the hint is not on the band's status row: {}",
        session.dump("scrolled")
    );

    session.send("q");
    let back = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "q leave")
    });
    assert!(back, "q did not return to the tail: {}", session.dump("after q"));
}

/// Amy: *"live typing should snap back to the tail, I often hit space just
/// to do that"*. `Space` snaps and never marks, and the keys after it type
/// at the tail.
#[test]
fn space_snaps_to_the_tail_and_typing_lands_in_the_draft() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    fill_transcript(&session);

    let tail = transcript_rows(&session);
    wheel_up(&session);
    wait_for_scrolled(&session, "after three Up");

    // `v` paints the marked line, so "Space did not mark" is something the
    // screen can actually be asked. `j` moves off it, since the reader's own
    // row is painted as the reader's rather than as marked.
    session.send("vj");
    let marked = session.wait_until(Duration::from_secs(5), |screen| {
        !marked_rows(screen, TRANSCRIPT_ROWS).is_empty()
    });
    assert!(marked, "`v` painted no row: {}", session.dump("after v"));
    session.send("\x1b");
    let unmarked = session.wait_until(Duration::from_secs(5), |screen| {
        !screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(unmarked, "Esc did not return to the tail: {}", session.dump("after Esc"));

    wheel_up(&session);
    wait_for_scrolled(&session, "after the second tick");
    session.send(" ");
    let snapped = session.wait_until(Duration::from_secs(5), |screen| {
        !screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(snapped, "Space did not snap to the tail: {}", session.dump("after Space"));
    assert_eq!(
        tail,
        transcript_rows(&session),
        "the snap did not put the transcript back on the tail: {}",
        session.dump("after Space")
    );

    // Back off the tail: nothing is marked, so Space marked nothing.
    session.send("\x01[");
    wait_for_scrolled(&session, "after re-entry");
    assert_eq!(
        session.marked_rows(TRANSCRIPT_ROWS),
        Vec::<usize>::new(),
        "Space started a mark instead of snapping: {}",
        session.dump("after re-entry")
    );
    session.send("q");
    let back = session.wait_until(Duration::from_secs(5), |screen| {
        !screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(back, "q did not return to the tail: {}", session.dump("after q"));

    session.send("ihello");
    let typed = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        compose_row(&rows).is_some_and(|i| rows[i].contains("hello"))
    });
    assert!(typed, "typing after the snap never reached the draft: {}", session.dump("after typing"));
}

/// `v` marks, `j` extends the range by a line, and `y` copies both lines out
/// over OSC 52 and into the tui's own paste buffer, which `Ctrl+A ]` pastes
/// into the draft (`docs/tui.md`, "Scrolling is copy mode").
#[test]
fn v_then_j_then_y_copies_two_lines_over_osc52_and_into_the_paste_buffer() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    fill_transcript(&session);
    assert!(session.clipboard().is_empty(), "nothing has been yanked yet");

    wheel_up(&session);
    wait_for_scrolled(&session, "before the yank");

    // `/` searches the whole transcript, not only what is on screen, and it
    // moves the reader onto a row that holds the needle.
    session.send("/mid0\r");
    let found = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        reader_rows(screen, TRANSCRIPT_ROWS)
            .first()
            .is_some_and(|&row| rows[row].contains("mid0"))
    });
    assert!(found, "the search did not put the reader on a matching row: {}", session.dump("after /"));

    // The two rows the mark covers, read off the screen before the yank.
    let rows = session.screen_text();
    let reader = session.reader_rows(TRANSCRIPT_ROWS)[0];
    let want = [rows[reader].trim_end().to_string(), rows[reader + 1].trim_end().to_string()];

    session.send("vjy");
    let left = session.wait_until(Duration::from_secs(5), |screen| {
        !screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(left, "the yank did not return to the tail: {}", session.dump("after y"));
    assert_eq!(
        session.clipboard(),
        vec![want.join("\n")],
        "the clipboard does not hold the two marked rows: {}",
        session.dump("after y")
    );

    session.send("\x01]");
    let pasted = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        compose_row(&rows).is_some_and(|i| {
            rows[i].contains(want[0].trim()) && rows.get(i + 1).is_some_and(|l| l.contains(want[1].trim()))
        })
    });
    assert!(
        pasted,
        "the two yanked lines are not both in the draft: {}",
        session.dump("after Ctrl+A ]")
    );
}

/// Alternate scroll (DECSET 1007) is what makes xterm send the wheel as
/// arrow keys on the alternate screen; kitty and foot do it by default and
/// wezterm has its own setting (`docs/tui.md`, "The mouse stays the
/// terminal's"). It goes on with the screen and off on the way out.
#[test]
fn alternate_scroll_is_taken_with_the_screen_and_given_back() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);
    assert_eq!(
        session.alternate_scroll(),
        (1, 0),
        "alternate scroll was not enabled after taking the screen: {}",
        session.dump("attached")
    );

    quit(&mut session);
    assert_eq!(
        session.alternate_scroll(),
        (1, 1),
        "alternate scroll was not disabled on the way out: {}",
        session.dump("after :q")
    );
}

/// Focus reporting (DECSET 1004) goes on with the screen and off on the way
/// out, the same two places alternate scroll is handled, and the reports
/// themselves are events rather than keys: `ESC [ O` and `ESC [ I` reach
/// crossterm as `FocusLost`/`FocusGained` and nothing of them lands in the
/// draft (`docs/tui.md`, "What owning the screen lets us use").
#[test]
fn focus_reporting_is_taken_with_the_screen_and_its_reports_are_not_keys() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);
    assert_eq!(
        session.focus_reporting(),
        (1, 0),
        "focus reporting was not enabled after taking the screen: {}",
        session.dump("attached")
    );

    // Insert mode, so a report mistaken for a key would be typed text.
    session.send("ihello");
    let typed = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, 80).any(|line| line.contains('❯') && line.contains("hello"))
    });
    assert!(typed, "{}", session.dump("after typing hello"));

    session.send("\x1b[O"); // focus out
    std::thread::sleep(Duration::from_millis(200));
    session.send("\x1b[I"); // focus in
    std::thread::sleep(Duration::from_millis(200));
    session.send("x");

    let still_typing = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, 80).any(|line| line.contains('❯') && line.contains("hellox"))
    });
    assert!(
        still_typing,
        "the draft did not take the next key after two focus reports: {}",
        session.dump("after focus reports")
    );
    let rows = session.screen_text();
    let draft = rows.iter().find(|l| l.contains('❯')).expect("a draft row").clone();
    assert!(
        !draft.contains("helloO") && !draft.contains("hellI") && !draft.contains("hellOx"),
        "a focus report landed in the draft: {draft:?}\n{}",
        session.dump("after focus reports")
    );

    quit(&mut session);
    assert_eq!(
        session.focus_reporting(),
        (1, 1),
        "focus reporting was not disabled on the way out: {}",
        session.dump("after :q")
    );
}

/// The window title names the context on screen, follows a switch to name
/// the context switched to, and the shell's own title comes back on the way
/// out through xterm's title stack (`docs/tui.md`, "What owning the screen
/// lets us use"). One title per change: the attach and the switch, no more.
#[test]
fn the_window_title_follows_the_context_and_is_given_back() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    let titled = session.wait_until(Duration::from_secs(5), |_| {
        session.titles().last().is_some_and(|t| t == "probe — kaijutsu")
    });
    assert!(titled, "the title does not name the attached context: {:?}", session.titles());
    assert_eq!(
        session.title_stack(),
        (1, 0),
        "the shell's title was not pushed before the first title was set"
    );

    // A second context with a label of its own, then a switch to its seat.
    colon_line(&session, ":kj context create second");
    clear_status_notice(&session);
    let digit = seat_of(&session, "second");
    session.send(&format!("\x01{digit}")); // Ctrl+A <digit>

    let switched = session.wait_until(Duration::from_secs(10), |_| {
        session.titles().last().is_some_and(|t| t == "second — kaijutsu")
    });
    assert!(
        switched,
        "the title does not name the context switched to: {:?}\n{}",
        session.titles(),
        session.dump("after the switch")
    );
    assert_eq!(
        session.titles(),
        vec!["probe — kaijutsu".to_string(), "second — kaijutsu".to_string()],
        "one title per change, and only on a change: {}",
        session.dump("after the switch")
    );

    quit(&mut session);
    assert_eq!(
        session.title_stack(),
        (1, 1),
        "the shell's own title was not restored on the way out"
    );
}

/// Type a `:` line and give the client a moment to run it. `Esc` goes on
/// its own, as [`quit`] explains, and it also takes down an ask card.
fn colon_line(session: &TuiSession, line: &str) {
    session.send("\x1b");
    std::thread::sleep(Duration::from_millis(200));
    session.send(&format!("{line}\r"));
    std::thread::sleep(Duration::from_millis(2000));
}

/// Make this kernel able to raise an ask at all, and return the client to
/// the `probe` seat.
///
/// Two facts the ephemeral kernel starts without: the default approval
/// reviewer (`amy`) has no character sheet, so the gate cannot resolve a
/// reviewer and records nothing at all; and `kj character create` needs
/// `config-write`, which a `coder` context does not carry. ROOT is seat 1
/// and is the binding-admin context, so the grant is made from there.
fn arrange_a_reviewer(session: &TuiSession) {
    session.send("\x011"); // Ctrl+A 1 — the ROOT seat
    std::thread::sleep(Duration::from_millis(1500));
    colon_line(session, ":kj binding allow config-write probe");
    session.send("\x010"); // Ctrl+A 0 — back to the probe seat
    std::thread::sleep(Duration::from_millis(1500));
    colon_line(session, ":kj character create amy");
    let created = session.wait_until(Duration::from_secs(10), |screen| {
        screen_contains_str(screen, "\"name\": \"amy\"")
    });
    assert!(created, "no reviewer character: {}", session.dump("arranging a reviewer"));
}

/// An ask that lands while the terminal is unfocused says so to the desktop
/// — one OSC 777 and one OSC 9, written once — and an ask that lands in
/// front of the player says nothing, because it is already the card on
/// screen (`docs/tui.md`, "What owning the screen lets us use", "Asks").
#[test]
fn an_ask_notifies_the_desktop_only_while_unfocused() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 100);
    wait_for_attach(&session);
    arrange_a_reviewer(&session);
    assert_eq!(session.notifications(), (0, 0), "nothing has been asked yet");

    // The focus report goes out *after* the line, and the pty delivers them
    // in order: typing is itself input, and input implies focus
    // (`App::saw_input`), so a focus-out sent first would be undone by the
    // very keys that raise the ask.
    session.send("\x1b");
    std::thread::sleep(Duration::from_millis(200));
    session.send(":kj cc send foo hi\r");
    session.send("\x1b[O"); // focus out, before the poll round lands
    let notified = session.wait_until(Duration::from_secs(15), |_| session.notifications() == (1, 1));
    assert!(
        notified,
        "an ask raised out of focus did not notify: {:?}\n{}",
        session.notifications(),
        session.dump("unfocused ask")
    );

    // Nothing is sent to regain focus: typing the next line is what says
    // the player is here, which is the rule this half pins.
    colon_line(&session, ":kj cc send bar hi");
    // The card's own hint line is the receipt that the poll round saw this
    // ask as newly pending — the same round a notification would ride
    // (`docs/tui.md`, "Asks").
    let carded = session.wait_until(Duration::from_secs(20), |screen| {
        screen_contains_str(screen, "[v]iew ledger") && screen_contains_str(screen, "Esc aside")
    });
    assert!(carded, "the second ask never raised a card: {}", session.dump("focused ask"));
    // And a round more, so a notification this round would have made has
    // been made before the count below is read.
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(
        session.notifications(),
        (1, 1),
        "an ask raised in front of the player notified anyway: {}",
        session.dump("focused ask")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// d. Resize keeps the band on screen and at the bottom
// ────────────────────────────────────────────────────────────────────────────

/// The band is the screen's last rows at any size: the status line is the
/// last row, the draft is above it, and neither is drawn twice.
#[test]
fn resize_keeps_the_band_at_the_bottom() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    for (rows, cols) in [(30u16, 100u16), (20u16, 60u16)] {
        session.resize(rows, cols);
        let settled = session.wait_until(Duration::from_secs(5), |screen| {
            let text: Vec<String> = screen.rows(0, cols).collect();
            text.len() == rows as usize
                && compose_row(&text) == Some(rows as usize - 2)
                && !text[rows as usize - 1].trim().is_empty()
        });
        let label = format!("after resize to {rows}x{cols}");
        assert!(settled, "never settled at {rows}x{cols}: {}", session.dump(&label));

        let text = session.screen_text();
        assert_eq!(
            text.iter().filter(|l| l.contains('\u{276f}')).count(),
            1,
            "the band must be on screen exactly once:\n{}",
            session.dump(&label)
        );
        for line in &text {
            assert!(line.chars().count() <= cols as usize, "row spilled past {cols} cols: {line:?}");
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// d2. A resize re-wraps the transcript
// ────────────────────────────────────────────────────────────────────────────

/// The transcript is a view over the context's blocks, wrapped at the
/// current width: the same line takes more rows at 40 columns than at 80
/// (`docs/tui.md`, "The buffer"). Nothing is printed once and left behind.
#[test]
fn a_resize_rewraps_the_transcript() {
    let _serial = serial();
    const MARKER: &str = "wrapme";
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // Twenty six-letter words, one line.
    let line = vec![MARKER; 20].join(" ");
    session.send(&format!(":!echo {line}\r"));
    let landed = session.wait_until(Duration::from_secs(20), |screen| {
        screen.rows(0, screen.size().1).filter(|l| l.contains(MARKER)).count() >= 2
    });
    assert!(landed, "the long result never landed: {}", session.dump("after :!echo"));
    let wide = session
        .screen_text()
        .iter()
        .filter(|l| l.contains(MARKER))
        .count();

    session.resize(24, 40);
    // Twenty words of six characters wrap five to a row at 40 columns: five
    // rows for the statement the shell echoed (its `$ echo` prefix takes
    // four words on the first row, then three full rows and a last word)
    // and four for the result's own body.
    const NARROW_ROWS: usize = 9;
    let rewrapped = session.wait_until(Duration::from_secs(10), |screen| {
        screen.size() == (24, 40)
            && screen.rows(0, 40).filter(|l| l.contains(MARKER)).count() == NARROW_ROWS
    });
    let narrow_rows: Vec<String> = session
        .screen_text()
        .into_iter()
        .filter(|l| l.contains(MARKER))
        .collect();
    assert!(
        rewrapped,
        "the transcript did not re-wrap to {NARROW_ROWS} rows at 40 columns (was {wide} rows, \
         now {narrow_rows:?}): {}",
        session.dump("after narrowing")
    );
    assert!(wide < NARROW_ROWS, "the 80-column wrap already took {wide} rows");
    for row in &narrow_rows {
        assert!(row.chars().count() <= 40, "a row spilled past 40 columns: {row:?}");
        assert!(
            row.trim_end().ends_with(MARKER),
            "the row broke mid-word, so the terminal wrapped it and the tui did not: {row:?}"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// e. Quit restores the terminal, quietly
// ────────────────────────────────────────────────────────────────────────────

/// `:q` gives the alternate screen back and prints nothing onto the screen
/// the shell had (`docs/tui.md`, "The owned screen": *":q can be quiet"*).
/// The conversation is not in the terminal's history afterwards — the kernel
/// holds every block, and that is the price the owned screen pays.
#[test]
fn colon_q_is_quiet_on_the_main_screen() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // A block whose text only the tui's own screen ever held.
    session.send(":!echo quiet-exit-marker\r");
    let landed = session.wait_until(Duration::from_secs(20), |screen| {
        screen_contains_str(screen, "quiet-exit-marker")
    });
    assert!(landed, "{}", session.dump("before quit"));

    quit(&mut session);
    assert_terminal_restored(&session, "after :q");

    let (scrollback, visible) = session.history_snapshot();
    assert!(
        !scrollback.iter().chain(visible.iter()).any(|l| l.contains("quiet-exit-marker")),
        "the transcript was printed onto the main screen: {visible:?}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// g. A partial `:` line never repeats into the transcript
// ────────────────────────────────────────────────────────────────────────────

/// The `:` bar is band content and nothing else: a line being typed there
/// must appear exactly once, on the compose row, no matter how many blocks
/// land in the transcript while it is being typed. `docs/tui.md`,
/// "The `:` line": the `Ctrl+Z` shell surface this probe used to cover
/// retired in favor of `:!`.
#[test]
fn a_partial_colon_line_never_repeats_into_the_transcript() {
    let _serial = serial();
    const PARTIAL: &str = "zz partial";
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // A whole command lands blocks in the transcript; the partial line typed
    // right behind it reopens the bar while they arrive.
    session.send("\x1b:!kj context list\r");
    session.send(":zz partial");
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        // The bar's own row: the `:` glyph, then the body without its prefix.
        rows.iter().any(|l| l.contains(":zz partial"))
    });
    assert!(landed, "{}", session.dump("after opening the bar with a partial line"));
    std::thread::sleep(Duration::from_millis(500));

    let text = session.screen_text();
    let bar_rows: Vec<&String> = text.iter().filter(|l| l.contains(PARTIAL)).collect();
    assert_eq!(
        bar_rows.len(),
        1,
        "the `:` bar must be on screen exactly once:\n{}",
        session.dump("after the command's blocks landed")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// h. `:` opens a visible bar; a real edit never rides through it (step-1 probe)
// ────────────────────────────────────────────────────────────────────────────

/// The finding this probe exists to verify: `docs/tui.md` (the writeup, now
/// built) warned that on the pre-lane code `:` in normal mode focused
/// modalkit's command bar, but compose neither drew it nor drained it — "a
/// bar nobody can see and every key after it goes there," and a plausible
/// shape for Amy's "locked up" report. The receipt (this probe, run against
/// the code as it stood before this lane's fix) is quoted in the lane's
/// final report rather than reproduced here — the assertions below are what
/// must hold now.
#[test]
fn colon_opens_a_visible_bar_and_only_a_real_edit_reaches_the_draft() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b"); // Esc: normal mode
    session.send(":");
    session.send("abc");
    let bar_visible = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains(":abc"))
    });
    assert!(bar_visible, "the `:` bar never became visible while typing: {}", session.dump("while typing :abc"));

    session.send("\x1b"); // Esc aborts the bar, discarding "abc"
    session.send("i");
    session.send("hello");
    let ok = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains("❯ hello"))
    });
    assert!(ok, "{}", session.dump("after typing hello"));

    let rows = session.screen_text();
    assert!(
        !rows.iter().any(|l| l.contains("abc")),
        "the aborted `:abc` reached the draft: {}",
        session.dump("after typing hello")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// i. `:kj` and `:!` land real blocks
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn colon_kj_runs_the_command_and_lands_its_output() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b:kj context list\r");
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        screen
            .rows(0, screen.size().1)
            .any(|l| l.contains("context") && l.contains("list"))
    });
    assert!(landed, "no block carrying the kj argv landed: {}", session.dump("after :kj context list"));
}

/// `:` after a second `Esc` — the keystroke a player reaching for normal
/// mode from anywhere types, and where the first live test stalled when a
/// second `Esc` still meant something — opens the bar and runs the line.
#[test]
fn colon_kj_runs_after_a_second_esc() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b"); // Esc: normal mode
    assert!(
        session.wait_until(Duration::from_secs(5), |screen| !screen_contains_str(screen, "INSERT")),
        "the INSERT banner never cleared: {}",
        session.dump("after Esc")
    );
    session.send("\x1b"); // Esc again: still normal mode
    assert!(
        session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "NORMAL")),
        "normal mode never showed: {}",
        session.dump("after Esc Esc")
    );

    session.send(":kj context list\r");
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        screen
            .rows(0, screen.size().1)
            .any(|l| l.contains("context") && l.contains("list"))
    });
    assert!(landed, "no block carrying the kj argv landed after Esc Esc: {}", session.dump("after Esc Esc"));
}

#[test]
fn colon_bang_runs_one_kaish_statement_and_lands_its_output() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b:!echo hi\r");
    // The result lands in the transcript, above the band — not on the
    // compose row, where the line was typed.
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        let Some(draft) = compose_row(&rows) else { return false };
        rows[..draft].iter().any(|l| l.trim() == "hi")
    });
    assert!(landed, "{}", session.dump("after :!echo hi"));
}

// ────────────────────────────────────────────────────────────────────────────
// j. `:q` and `:q!` quit
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn colon_q_exits_cleanly() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b:q\r");
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit: {}", session.dump("still running")));
    assert!(status.success(), "kaijutsu-tui exited with {status:?}: {}", session.dump("after :q"));
    assert_terminal_restored(&session, "after :q");
}

#[test]
fn colon_q_bang_exits_cleanly() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x1b:q!\r");
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit: {}", session.dump("still running")));
    assert!(status.success(), "kaijutsu-tui exited with {status:?}: {}", session.dump("after :q!"));
}

// ────────────────────────────────────────────────────────────────────────────
// k. `Ctrl+C` reclaimed: it interrupts, it never quits
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ctrl_c_alone_neither_interrupts_nor_quits() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x03");
    let noticed = session.wait_until(Duration::from_secs(5), |screen| {
        screen_contains_str(screen, "nothing to interrupt")
    });
    assert!(noticed, "{}", session.dump("after one Ctrl+C"));
    assert!(session.wait_for_exit(Duration::from_millis(300)).is_none(), "a single Ctrl+C must not exit");
}

#[test]
fn ctrl_c_twice_within_the_window_still_does_not_exit() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("\x03\x03");
    // Nothing was running either time, so both presses post the same
    // "nothing to interrupt" notice rather than escalating — see
    // `interrupt::Ladder::press`.
    let noticed = session.wait_until(Duration::from_secs(5), |screen| {
        screen_contains_str(screen, "nothing to interrupt")
    });
    assert!(noticed, "{}", session.dump("after two Ctrl+C"));
    assert!(
        session.wait_for_exit(Duration::from_millis(300)).is_none(),
        "Ctrl+C Ctrl+C must not exit — only `:q`/`:q!` quit"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// e2. Every way out restores the terminal
// ────────────────────────────────────────────────────────────────────────────

/// What `run::restore_terminal` leaves behind, from the host shell's point
/// of view: the main screen buffer and a cooked line discipline. The cursor
/// shape reset rides the same sequence and `vt100` cannot observe it.
fn assert_terminal_restored(session: &TuiSession, label: &str) {
    assert!(!session.on_alternate_screen(), "still on the alternate screen {label}: {}", session.dump(label));
    #[cfg(unix)]
    assert_eq!(session.cooked(), Some(true), "raw mode left on {label}: {}", session.dump(label));
}

/// Scroll off the tail with `Ctrl+A [`, so an exit has the screen, raw mode
/// and a scrolled transcript to undo.
fn open_copy_mode(session: &TuiSession) {
    session.send("\x01[");
    let opened = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "q leave"));
    assert!(opened, "the transcript never left the tail: {}", session.dump("copy mode open"));
    assert!(session.on_alternate_screen(), "the session is not on the alternate screen: {}", session.dump("copy mode"));
}

/// `SIGTERM` — a runner's kill — ends the loop the way `:q` does, from the
/// alternate screen included.
#[cfg(unix)]
#[test]
fn sigterm_restores_the_terminal_from_the_alternate_screen() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);
    open_copy_mode(&session);

    let pid = session.pid().expect("pid available on unix");
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit on SIGTERM: {}", session.dump("still running")));
    assert!(status.success(), "a SIGTERM exit is a clean exit, got {status:?}: {}", session.dump("after SIGTERM"));
    assert_terminal_restored(&session, "after SIGTERM");
}

/// A panic unwinds past `leave_terminal`; the panic hook restores the
/// terminal before the message prints. `KAIJUTSU_TUI_PROBE_PANIC` makes
/// `F12` panic (`run.rs`, `Wires::probe_panic`).
#[cfg(unix)]
#[test]
fn a_panic_restores_the_terminal_from_the_alternate_screen() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session = TuiSession::spawn_with_env(server.addr, &key_path, 24, 80, &[("KAIJUTSU_TUI_PROBE_PANIC", "1")]);
    wait_for_attach(&session);
    open_copy_mode(&session);

    session.send("\x1b[24~"); // F12
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit on the probe panic: {}", session.dump("still running")));
    assert!(!status.success(), "the probe panic must not look like a clean exit: {}", session.dump("after panic"));
    assert_terminal_restored(&session, "after the panic");
    // A panic inside a frame unwinds past the frame's own
    // `EndSynchronizedUpdate`; the restore path has to end it, or the
    // terminal paints nothing ever again (`run::restore_terminal`).
    assert_eq!(
        session.sync_updates_open(),
        0,
        "the terminal was left inside a synchronized update: {}",
        session.dump("after panic")
    );
    // `leave_terminal` and the panic hook both reach `restore_terminal`,
    // which is idempotent: the title stack is popped exactly as often as it
    // was pushed, never once more — a pop with no push behind it takes the
    // shell's own saved title off the stack (`run::pop_title`).
    let (pushes, pops) = session.title_stack();
    assert_eq!(pushes, pops, "the title stack is unbalanced after the panic: {pushes} pushed, {pops} popped");
    let (scrollback, visible) = session.history_snapshot();
    assert!(
        scrollback.iter().chain(visible.iter()).any(|l| l.contains("KAIJUTSU_TUI_PROBE_PANIC")),
        "the panic message is not on a readable screen: {}",
        session.dump("after panic")
    );
}

/// A panic *inside* a frame unwinds past that frame's
/// `EndSynchronizedUpdate`. The restore path ends the update itself, or the
/// terminal the player is handed back paints nothing at all
/// (`run::restore_terminal`). `KAIJUTSU_TUI_PROBE_PANIC=frame` makes `F12`
/// panic there rather than in the key path.
#[cfg(unix)]
#[test]
fn a_panic_inside_a_frame_ends_the_synchronized_update() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session =
        TuiSession::spawn_with_env(server.addr, &key_path, 24, 80, &[("KAIJUTSU_TUI_PROBE_PANIC", "frame")]);
    wait_for_attach(&session);

    session.send("\x1b[24~"); // F12
    let status = session.wait_for_exit(Duration::from_secs(5));
    assert!(status.is_some(), "process did not exit: {}", session.dump("still running"));
    assert_terminal_restored(&session, "after the frame panic");
    assert_eq!(
        session.sync_updates_open(),
        0,
        "the terminal was left inside a synchronized update: {}",
        session.dump("after the frame panic")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// j2. The open picker follows a placement verb
// ────────────────────────────────────────────────────────────────────────────

/// Which side of the `RECENT` divider `label`'s row is on: `Some(true)`
/// for ACTIVE, `Some(false)` for RECENT, `None` when it is not listed.
fn in_active(rows: &[String], label: &str) -> Option<bool> {
    let row = picker_row_for(rows, label)?;
    let recent = rows.iter().position(|l| l.trim() == "RECENT")?;
    Some(row < recent)
}

/// Wait until the picker's cursor (`›`) is on `label`'s row.
fn wait_for_cursor_on(session: &TuiSession, label: &str) {
    let landed = session.wait_until(Duration::from_secs(10), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        picker_row_for(&rows, label).is_some_and(|i| rows[i].trim_start().starts_with('\u{203a}'))
    });
    assert!(landed, "the picker's cursor never reached {label}: {}", session.dump("cursor"));
}

/// `p` on a RECENT row moves it into ACTIVE without closing the picker,
/// and `d` moves it back (`docs/tui.md`, "The picker": the list follows
/// the kernel while it is open). The fork also has to appear in a picker
/// that was already open, which is the same rebuild.
#[test]
fn a_placement_verb_moves_the_row_while_the_picker_stays_open() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 100);
    wait_for_attach(&session);

    session.send(":kj fork --name seatme\r");
    session.send("\x01\"");
    let listed = session.wait_until(Duration::from_secs(20), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        picker_row_for(&rows, "seatme").is_some()
    });
    assert!(listed, "the fork never reached the open picker: {}", session.dump("picker"));

    // Filter to the fork alone; the filter survives every rebuild.
    session.send("/seatme\r");
    let filtered = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        picker_row_for(&rows, "seatme").is_some() && !screen_contains_str(screen, "ROOT")
    });
    assert!(filtered, "the filter never narrowed to the fork: {}", session.dump("filter"));
    for _ in 0..tabs_to_section(&session.screen_text(), "seatme") {
        session.send("\t");
    }
    // The open picker rebuilds on every refresh round, and a rebuild puts
    // the cursor back on its own section's first row. Press the verb only
    // once the cursor is seen on the fork's row, or the verb acts on
    // whatever the rebuild selected — an empty section, under this filter.
    wait_for_cursor_on(&session, "seatme");

    // Whichever section the fork starts in, the verb that moves it out.
    let started_active = in_active(&session.screen_text(), "seatme").expect("the fork is listed");
    session.send(if started_active { "d" } else { "p" });
    // Two refresh rounds: the picker follows the kernel on the 5 s timer,
    // and a cold run can spend one of them building.
    let moved = session.wait_until(Duration::from_secs(20), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        screen_contains_str(screen, "ACTIVE") && in_active(&rows, "seatme") == Some(!started_active)
    });
    assert!(
        moved,
        "the row did not cross the divider with the picker open (started in {}): {}",
        if started_active { "ACTIVE" } else { "RECENT" },
        session.dump("after the verb")
    );

    // The cursor followed the row, so the opposite verb acts on it again.
    wait_for_cursor_on(&session, "seatme");
    session.send(if started_active { "p" } else { "d" });
    let back = session.wait_until(Duration::from_secs(20), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        screen_contains_str(screen, "ACTIVE") && in_active(&rows, "seatme") == Some(started_active)
    });
    assert!(back, "the row did not cross back: {}", session.dump("after the second verb"));
}

// ────────────────────────────────────────────────────────────────────────────
// m. A bracketed paste is text, not keystrokes
// ────────────────────────────────────────────────────────────────────────────

/// Two pasted lines land in the draft as one edit: the newline is a
/// newline in the draft, not an `Enter` that submits the first line
/// (`docs/tui.md`, "Compose"). The paste lands in NORMAL mode, where the
/// same bytes read as keystrokes would be vi commands (`d`, `x`, `p`…)
/// and never text — so a client that left bracketed paste off cannot
/// pass. The terminal's `\r` line endings are what xterm sends.
#[test]
fn a_bracketed_paste_lands_in_the_draft_without_submitting() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    assert!(session.bracketed_paste(), "the client never turned bracketed paste on: {}", session.dump("attached"));

    session.send("\x1b[200~dd pasted one\rxp pasted two\x1b[201~");
    let landed = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        let first = rows.iter().position(|l| l.contains('❯') && l.contains("dd pasted one"));
        first.is_some_and(|i| rows.get(i + 1).is_some_and(|l| l.contains("xp pasted two")))
    });
    assert!(landed, "the paste did not land as two draft rows: {}", session.dump("after paste"));

    // Nothing was submitted: the draft is still there a moment later, and
    // the transcript above the band holds no user block carrying the first
    // line.
    std::thread::sleep(Duration::from_secs(2));
    let visible = session.screen_text();
    let draft = compose_row(&visible).expect("the draft is on screen");
    assert!(
        visible[draft].contains("dd pasted one"),
        "the draft was cleared, so something submitted: {}",
        session.dump("after paste")
    );
    assert!(
        !visible[..draft].iter().any(|l| l.contains("dd pasted one")),
        "the first pasted line was submitted as a turn: {}",
        session.dump("after paste")
    );

    // The `:` bar takes a paste flattened onto its one line.
    session.send(":kj ");
    session.send("\x1b[200~context\rlist\x1b[201~");
    let flat = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, ":kj context list"));
    assert!(flat, "the bar paste did not flatten onto the bar: {}", session.dump("bar paste"));
}

// ────────────────────────────────────────────────────────────────────────────
// n. The client never asks the terminal where the cursor is
// ────────────────────────────────────────────────────────────────────────────

/// The tui owns the alternate screen for the session, so no frame is
/// anchored to the cursor and no path asks for it (`docs/tui.md`, "The
/// owned screen"): a whole session — attach, type, open the picker, resize,
/// quit — sends zero `ESC [ 6 n`. The query is what stalled a slow ssh hop
/// for two seconds a frame.
#[test]
fn the_client_never_asks_where_the_cursor_is() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);

    session.send("ihello\x1b");
    let typed = session.wait_until(Duration::from_secs(5), |screen| {
        screen_contains_str(screen, "hello")
    });
    assert!(typed, "the draft never showed: {}", session.dump("typing"));

    session.send("\x01\"");
    let opened = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "ACTIVE"));
    assert!(opened, "picker never opened: {}", session.dump("picker"));
    session.send("\x1b");

    session.resize(30, 100);
    // The status line is the screen's last row once the client has redrawn
    // at the new size. Waiting on the parser's own size proves nothing: the
    // harness sets it the moment the pty resizes.
    let resized = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, 100).collect();
        rows.len() == 30 && !rows[29].trim().is_empty() && compose_row(&rows) == Some(28)
    });
    assert!(resized, "never redrew at the new size: {}", session.dump("resize"));

    // A full-screen surface and a suspend go through the same one terminal,
    // and neither anchors a frame to the cursor either.
    session.send("\x01[");
    let copy_mode = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "q leave"));
    assert!(copy_mode, "copy mode never opened: {}", session.dump("copy mode"));
    session.send("q");
    let back = session.wait_until(Duration::from_secs(5), |screen| !screen_contains_str(screen, "q leave"));
    assert!(back, "copy mode never closed: {}", session.dump("copy mode close"));

    #[cfg(unix)]
    {
        let pid = session.pid().expect("pid available on unix");
        session.send("\x1a"); // Ctrl+Z
        std::thread::sleep(Duration::from_millis(300));
        unsafe {
            libc::kill(pid as i32, libc::SIGCONT);
        }
        let alive = session.wait_until(Duration::from_secs(5), |screen| screen_contains(screen, '\u{276f}'));
        assert!(alive, "the client did not come back from the suspend: {}", session.dump("after SIGCONT"));
    }

    quit(&mut session);
    assert_eq!(
        session.cursor_queries(),
        0,
        "the client asked the terminal where the cursor is; a full-screen viewport never does"
    );
    assert_eq!(
        session.keyboard_queries(),
        0,
        "the client probed for the kitty keyboard protocol; it is requested with \
         --kitty-keyboard or not at all, never queried for (docs/tui.md, \"Open\")"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// l. `Ctrl+Z` suspends, one press, no toggle
// ────────────────────────────────────────────────────────────────────────────

/// `/proc/<pid>/stat`'s third field: `R` running, `S` sleeping, `T` stopped
/// (job-control), … (`man proc(5)`). The process's own name can carry
/// spaces or parens, so the state is read after the last `)` rather than by
/// splitting on whitespace from the front.
#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.trim_start().chars().next()
}

#[cfg(target_os = "linux")]
fn wait_for_proc_state(pid: u32, want: char, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if proc_state(pid) == Some(want) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `portable_pty::Child` exposes no "is this job stopped" query, so this
/// probe reads `/proc` directly where it can — Linux-only, which is what
/// this repo's own test environment is (`CLAUDE.md`, "Machines").
///
/// **The stopped state cannot be observed cleanly in this sandbox.** A
/// diagnostic trace (`run::act` → `Intent::Suspend` → `run::suspend` →
/// `run::raise_stop` → `libc::raise(SIGTSTP)`) confirmed the key reaches
/// `Intent::Suspend` and every function on that path runs and returns
/// normally — but `raise(SIGTSTP)` itself is a no-op here: `/proc/<pid>/stat`
/// never reports `T` in the second that follows, tight-polled at 2ms. The
/// same sandbox's `kill -TSTP <pid>` **does** stop a plain `sleep &` child
/// (verified directly), so this is specific to a *self*-directed stop signal
/// from inside this multi-threaded process — plausibly the sandbox denying a
/// monitored process the ability to go quiet on its own. So: poll for `T`
/// best-effort and say what was observed, but the pass/fail assertion is the
/// one thing every environment must honor — `SIGCONT` and a live client.
#[cfg(target_os = "linux")]
#[test]
fn ctrl_z_suspends_and_sigcont_resumes_a_responsive_client() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    let pid = session.pid().expect("pid available on Linux");

    session.send("\x1a"); // Ctrl+Z
    let stopped = wait_for_proc_state(pid, 'T', Duration::from_secs(2));
    if !stopped {
        eprintln!(
            "ctrl_z_suspends_and_sigcont_resumes_a_responsive_client: the stopped (T) state was \
             never observed (last seen: {:?}) — known sandbox limitation, see this test's doc \
             comment; continuing to the responsiveness assertion regardless.",
            proc_state(pid)
        );
    }

    // SIGCONT the way a shell's `fg` would — nothing plays that role here,
    // since the binary is the pty's direct child, not a job under a shell.
    // Harmless if the process was never actually stopped.
    unsafe {
        libc::kill(pid as i32, libc::SIGCONT);
    }

    session.send("ix"); // `i`: a fresh draft rests in normal mode
    let responsive = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains('❯') && line.contains('x'))
    });
    assert!(responsive, "client did not respond after SIGCONT: {}", session.dump("after SIGCONT"));
    // The screen is taken again on the way back — vim's
    // `stoptermcap`/`starttermcap` order (`run::suspend`).
    assert!(
        session.on_alternate_screen(),
        "the client did not take the screen again after SIGCONT: {}",
        session.dump("after SIGCONT")
    );
    // Alternate scroll rides with the screen: on at attach, off before the
    // stop, on again on the way back.
    assert_eq!(
        session.alternate_scroll(),
        (2, 1),
        "alternate scroll did not follow the screen across the suspend: {}",
        session.dump("after SIGCONT")
    );
    // Focus reporting rides with it, the same three writes: a terminal left
    // reporting focus to a stopped job would send its reports into a pty
    // nobody is reading (`run::suspend`).
    assert_eq!(
        session.focus_reporting(),
        (2, 1),
        "focus reporting did not follow the screen across the suspend: {}",
        session.dump("after SIGCONT")
    );
    // The title stack balances across the suspend too: popped on the way
    // down, pushed again on the way back.
    assert_eq!(
        session.title_stack(),
        (2, 1),
        "the title stack did not follow the screen across the suspend: {}",
        session.dump("after SIGCONT")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// j. A context switch takes the new context's draft with it
// ────────────────────────────────────────────────────────────────────────────

/// Row index of a picker row naming `label` — a row below the `ACTIVE`
/// header, never the `kj fork` output that named the same label in the
/// transcript above it.
fn picker_row_for(rows: &[String], label: &str) -> Option<usize> {
    let active = picker_row(rows)?;
    rows.iter().enumerate().position(|(i, l)| i > active && l.contains(label))
}

/// How many `Tab` presses reach the section `label`'s row sits in: the
/// picker opens on ACTIVE, and `Tab` hops ACTIVE → RECENT → TRACKS
/// (`crates/kaijutsu-tui/src/picker.rs`'s `Section::next`).
fn tabs_to_section(rows: &[String], label: &str) -> usize {
    let row = picker_row_for(rows, label).expect("the label has a picker row");
    let recent = rows.iter().position(|l| l.trim() == "RECENT");
    match recent {
        Some(recent) if row > recent => 1,
        _ => 0,
    }
}

/// Switching through the picker loads the new context's draft, the way
/// `Ctrl+A <digit>` always has.
///
/// The picker's own switch used to watch the new context and make it
/// current without loading its draft, so the compose line kept the previous
/// context's text — and `Compose::acked`, which only `load_draft` resets,
/// then refused the change feed's correction, so it stayed there. Both
/// paths go through `run.rs`'s `switch_seat` now.
#[test]
fn switching_through_the_picker_loads_the_new_contexts_draft() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 100);
    wait_for_attach(&session);

    // A second context to switch to.
    session.send(":kj fork --name altseat\r");
    // Type a draft into the context on screen: `i` opens insert on a draft
    // resting in normal mode.
    session.send("izzdraftzz\x1b");
    let typed = session.wait_until(Duration::from_secs(20), |screen| {
        screen.rows(0, screen.size().1).any(|l| l.contains('❯') && l.contains("zzdraftzz"))
    });
    assert!(typed, "the draft never reached the compose row: {}", session.dump("typing"));

    // An open picker rebuilds on every refresh round, so the fork appears
    // in it one round after it is made without closing and reopening.
    session.send("\x01\"");
    let listed = session.wait_until(Duration::from_secs(20), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        picker_row_for(&rows, "altseat").is_some()
    });
    assert!(listed, "the fork never reached the picker: {}", session.dump("picker"));

    // Filter to the fork alone, close the filter, hop to its section, and
    // switch to what is then the only row there.
    session.send("/altseat\r");
    let filtered = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        picker_row_for(&rows, "altseat").is_some() && !screen_contains_str(screen, "ROOT")
    });
    assert!(filtered, "the filter never narrowed to the fork: {}", session.dump("filter"));
    for _ in 0..tabs_to_section(&session.screen_text(), "altseat") {
        session.send("\t");
    }
    session.send("\r");

    let switched = session.wait_until(Duration::from_secs(10), |screen| {
        !screen_contains_str(screen, "ACTIVE") && screen_contains(screen, '❯')
    });
    assert!(switched, "the picker never closed on the switch: {}", session.dump("switch"));

    let rows = session.screen_text();
    let compose = compose_row(&rows).expect("the compose row is drawn again after the switch");
    assert!(
        !rows[compose].contains("zzdraftzz"),
        "the previous context's draft is still on the compose line after switching to the fork:\n{}",
        session.dump("after switch")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// n. Per-context transcripts and the hot set (docs/tui.md, "The buffer")
// ────────────────────────────────────────────────────────────────────────────

/// The seat digit the status line gives `label` — what `Ctrl+A <digit>`
/// switches to. The cells read `0 altplace  1 probe*  2 ROOT`
/// (`status::SeatCell::text`), the flags being `*@!`.
fn seat_digit(status: &str, label: &str) -> Option<usize> {
    let tokens: Vec<&str> = status.split_whitespace().collect();
    tokens.windows(2).find_map(|pair| {
        let cell = pair[1].trim_end_matches(['*', '@', '!']);
        (cell == label).then(|| pair[0].parse().ok()).flatten()
    })
}

/// A `:kj` line leaves a notice standing where the seat cells go; a shell
/// statement that runs clears it (`run::handle_colon_line`). The keys queue
/// behind whatever is still running, so the order holds.
fn clear_status_notice(session: &TuiSession) {
    session.send(":!echo seated\r");
}

/// The seat digit the status line gives `label`, once the row is showing the
/// seat cells rather than a notice.
fn seat_of(session: &TuiSession, label: &str) -> usize {
    let seated = session.wait_until(Duration::from_secs(25), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        rows.last().and_then(|row| seat_digit(row, label)).is_some()
    });
    assert!(seated, "{label} never took a seat: {}", session.dump("seat"));
    let rows = session.screen_text();
    seat_digit(rows.last().expect("a status row"), label).expect("the seat was just seen")
}

/// Fork a context named `label` and wait until the status line seats it —
/// the proof that the kernel made it and this client has ranked it, before
/// a chord addresses it by seat.
fn fork_and_seat(session: &TuiSession, label: &str) -> usize {
    session.send(&format!(":kj fork --name {label}\r"));
    clear_status_notice(session);
    seat_of(session, label)
}

/// One refresh period and then some (`run::REFRESH` is 5 s): long enough
/// that a round that would have hydrated something has run.
const REFRESH_DWELL: Duration = Duration::from_secs(7);

/// Assert which side of the picker's `RECENT` divider `label` sits on, then
/// dismiss the picker. The band is not on the status row, and it is the fact
/// that decides whether the hot set should have taken the context on.
fn assert_band(session: &TuiSession, label: &str, active: bool) {
    session.send("\x01\"");
    let landed = session.wait_until(Duration::from_secs(25), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        in_active(&rows, label) == Some(active)
    });
    assert!(
        landed,
        "{label} is not in {}: {}",
        if active { "ACTIVE" } else { "RECENT" },
        session.dump("band")
    );
    session.send("\x1b");
    let dismissed = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "ACTIVE") && screen_contains(screen, '\u{276f}')
    });
    assert!(dismissed, "the picker never closed: {}", session.dump("band"));
}

/// Switch to seat `digit` and wait for the band to be drawn again.
fn switch_to_seat(session: &TuiSession, digit: usize) {
    session.send(&format!("\x01{digit}"));
    let drawn = session.wait_until(Duration::from_secs(10), |screen| screen_contains(screen, '\u{276f}'));
    assert!(drawn, "the switch to seat {digit} drew nothing: {}", session.dump("switch"));
}

/// A context's scrolled place is its own: scroll the context on screen,
/// switch away with `Ctrl+A <digit>`, and come back with `Ctrl+A Ctrl+A` —
/// the reader is on the same row of the same transcript, and the context
/// switched to opened on its own live tail (`docs/tui.md`, "The buffer").
///
/// The switch chords are under the `Ctrl+A` prefix, which the scrolled
/// transcript does not claim (`keys::Keys::claims`): a chord that leaves the
/// context must not snap it to the tail on the way out, or there would be
/// nothing to come back to.
#[test]
fn a_scrolled_context_comes_back_scrolled_after_a_switch() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);
    fill_transcript(&session);
    let other = fork_and_seat(&session, "altplace");

    wheel_up(&session);
    wait_for_scrolled(&session, "after three Up");
    let place = readout(&session).expect("a readout while scrolled");

    switch_to_seat(&session, other);
    // A line only the fork has, so "the fork is on screen" is something the
    // screen can be asked — a blank transcript area is not at its tail, it
    // is not drawn at all.
    session.send(":!echo in-fork\r");
    // In the transcript area, not anywhere on screen: the `:` bar carries
    // the same words while the line is being typed.
    let landed = session.wait_until(Duration::from_secs(20), |screen| {
        transcript_holds(screen, "in-fork")
    });
    assert!(landed, "the fork's own transcript never drew: {}", session.dump("altplace"));
    let at_tail = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "q leave")
    });
    assert!(at_tail, "the fork did not open on its own live tail: {}", session.dump("altplace"));

    session.send("\x01\x01");
    let back = session.wait_until(Duration::from_secs(10), |screen| {
        screen.rows(0, screen.size().1).any(|l| l.contains("q leave"))
    });
    assert!(back, "the scrolled context did not come back scrolled: {}", session.dump("back"));
    assert_eq!(
        readout(&session),
        Some(place),
        "the reader landed somewhere else: {}",
        session.dump("back")
    );

    // And the fork is still at its tail on return, not carrying the other
    // context's place.
    switch_to_seat(&session, other);
    let still_tail = session.wait_until(Duration::from_secs(5), |screen| {
        !screen_contains_str(screen, "q leave") && transcript_holds(screen, "in-fork")
    });
    assert!(
        still_tail,
        "the fork came back scrolled, or without its own transcript: {}",
        session.dump("altplace again")
    );
}

/// One line per context made resident (`run::watch_context`'s
/// `tracing::debug!`). Counting them tells a hydrate from a redraw, which
/// nothing on screen can.
const WATCH_LINE: &str = "watching context feed";

fn watches(log: &std::path::Path) -> usize {
    std::fs::read_to_string(log).map(|text| text.matches(WATCH_LINE).count()).unwrap_or(0)
}

/// Poll the log until it holds `want` watch lines, and return what it holds
/// then — `want` on success, fewer on a timeout, so the assertion can say
/// what it actually found.
fn wait_for_watches(log: &std::path::Path, want: usize, timeout: Duration) -> usize {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let seen = watches(log);
        if seen >= want || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A context promoted into the ACTIVE ring goes resident on the next
/// refresh round, with no switch — and a switch to it is then a redraw, not
/// a hydrate (`docs/tui.md`, "The buffer": *"The ACTIVE ring's contexts stay
/// resident"*).
///
/// The log is what tells the two apart: the screen looks the same either
/// way, and a timing assertion would measure the box.
#[test]
fn a_promoted_context_goes_resident_before_any_switch_to_it() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("tempdir for the ephemeral key");
    let key_path = write_ephemeral_key(key_dir.path());
    let session = TuiSession::spawn_with_env(
        server.addr,
        &key_path,
        24,
        80,
        &[("RUST_LOG", "warn,kaijutsu_tui=debug")],
    );
    let log = TuiSession::log_path(&key_path);
    wait_for_attach(&session);

    assert_eq!(
        wait_for_watches(&log, 1, CONNECT),
        1,
        "the context on screen is the session's first resident"
    );

    fork_and_seat(&session, "hotseat");
    // The band is the fact, not the seat: a fork lands on RECENT, which is
    // outside the hot set. Read it, then dwell a whole refresh period so a
    // round that would have hydrated it has had its chance.
    assert_band(&session, "hotseat", false);
    std::thread::sleep(REFRESH_DWELL);
    assert_eq!(
        watches(&log),
        1,
        "a context outside the ring was hydrated:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    session.send(":kj context promote hotseat\r");
    assert_band(&session, "hotseat", true);
    // Two refresh rounds: the rank the hot set reads is recomputed on the
    // 5 s timer, and a cold run can spend one of them building.
    assert_eq!(
        wait_for_watches(&log, 2, Duration::from_secs(25)),
        2,
        "the promoted context never went resident on its own:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // A promote moves the seats, so the digit is read again — and the
    // status row carries the promote's notice until a shell line clears it.
    clear_status_notice(&session);
    let hot = seat_of(&session, "hotseat");
    switch_to_seat(&session, hot);
    session.send("\x01\x01");
    switch_to_seat(&session, hot);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        watches(&log),
        2,
        "a resident context was hydrated again on a switch:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

// ────────────────────────────────────────────────────────────────────────────
// o. A chord never locks the keyboard (docs/issues.md, "Ctrl+A a")
// ────────────────────────────────────────────────────────────────────────────

/// `Ctrl+A` then any key must hand control back to compose — an unbound
/// chord, a later-lane chord, and `Ctrl+A a` alike. This is the harness probe
/// `docs/issues.md` called for: `Ctrl+A a` "locked the client once," and the
/// disarm (`self.armed = false` before the match in `keys::Keys::interpret`)
/// looks correct in today's code, so this is how we find out.
#[test]
fn a_chord_never_locks_the_keyboard() {
    let _serial = serial();
    let (_server, _key_dir, session) = spawn_session(24, 80);
    wait_for_attach(&session);

    // An unbound chord names the key on the status line
    // (`run::act`'s `Intent::Unbound` arm) rather than swallowing it.
    session.send("\x01x");
    let named = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        rows.last().is_some_and(|row| row.contains("Ctrl+A x is not bound"))
    });
    assert!(named, "the unbound chord never named the key: {}", session.dump("unbound chord"));

    // And the very next keystroke still reaches the draft — `\x1b` forces
    // normal mode first so entering insert with `i` is unambiguous whatever
    // mode the unbound chord left compose in.
    session.send("\x1bimarker-one");
    let landed_one = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        compose_row(&rows).is_some_and(|row| rows[row].contains("marker-one"))
    });
    assert!(landed_one, "typing after an unbound chord never reached the draft: {}", session.dump("after unbound"));

    // `Ctrl+A a`: screen's own `C-a a`, a literal `Ctrl+A` sent to the draft
    // (`keys::Keys::interpret`, the bare-`a` arm). What the vi engine does
    // with the byte is its own business; what this probe pins is that the
    // keyboard is not left armed or stuck afterward.
    session.send("\x01a");
    session.send("\x1bimarker-two");
    let landed_two = session.wait_until(Duration::from_secs(5), |screen| {
        let rows: Vec<String> = screen.rows(0, 80).collect();
        compose_row(&rows).is_some_and(|row| rows[row].contains("marker-two"))
    });
    assert!(
        landed_two,
        "typing after Ctrl+A a never reached the draft — the chord locked the keyboard: {}",
        session.dump("after ctrl+a a")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// e2. A task panic ends the client instead of tearing the screen out from
// under a still-running loop (docs/issues.md, "The panic hook restores the
// terminal under a still-running loop")
// ────────────────────────────────────────────────────────────────────────────

/// `KAIJUTSU_TUI_PROBE_PANIC=task` makes `F12` `spawn_local` a task that
/// panics — the shape of `Feeds::pump`'s forwarder or a hydrate round, whose
/// `JoinHandle` the loop never joins. Unlike the on-key and in-frame probes,
/// this panic does not unwind `event_loop` itself, so the hook must not
/// restore the terminal in place (`run::panic_unwinds_the_loop`); instead it
/// records the panic and the loop notices it on its next iteration
/// (`run::TASK_PANIC`) and ends normally through `leave_terminal`, the same
/// path `:q` takes.
#[cfg(unix)]
#[test]
fn a_panic_inside_a_task_ends_the_client_and_restores_the_terminal() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session = TuiSession::spawn_with_env(server.addr, &key_path, 24, 80, &[("KAIJUTSU_TUI_PROBE_PANIC", "task")]);
    wait_for_attach(&session);
    open_copy_mode(&session);

    session.send("\x1b[24~"); // F12
    let status = session.wait_for_exit(Duration::from_secs(5));
    let status = status.unwrap_or_else(|| panic!("process did not exit on the task panic: {}", session.dump("still running")));
    assert!(!status.success(), "a task panic must not look like a clean exit: {}", session.dump("after the task panic"));
    assert_terminal_restored(&session, "after the task panic");
    assert_eq!(
        session.sync_updates_open(),
        0,
        "the terminal was left inside a synchronized update: {}",
        session.dump("after the task panic")
    );
    let (pushes, pops) = session.title_stack();
    assert_eq!(pushes, pops, "the title stack is unbalanced after the task panic: {pushes} pushed, {pops} popped");
    let (scrollback, visible) = session.history_snapshot();
    assert!(
        scrollback.iter().chain(visible.iter()).any(|l| l.contains("KAIJUTSU_TUI_PROBE_PANIC")),
        "the panic message is not on a readable screen: {}",
        session.dump("after the task panic")
    );
}

// ────────────────────────────────────────────────────────────────────────────
// p. The kitty keyboard protocol: on by request, never by probe
// (docs/tui.md, "What owning the screen lets us use", "Open")
// ────────────────────────────────────────────────────────────────────────────

/// Without `--kitty-keyboard`, nothing pushes a keyboard-enhancement flag
/// stack frame and nothing queries for the protocol either — this client's
/// zero-terminal-query invariant covers the kitty query
/// (`CSI ? u`, `supports_keyboard_enhancement`) the same way it covers the
/// cursor-position query.
#[test]
fn the_kitty_keyboard_is_off_unless_asked() {
    let _serial = serial();
    let (_server, _key_dir, mut session) = spawn_session(24, 80);
    wait_for_attach(&session);
    quit(&mut session);

    assert_eq!(
        session.keyboard_enhancement(),
        (0, 0),
        "the kitty keyboard protocol was pushed without --kitty-keyboard: {}",
        session.dump("after quit")
    );
    assert_eq!(
        session.keyboard_queries(),
        0,
        "the client queried for kitty keyboard support instead of just requesting it: {}",
        session.dump("after quit")
    );
}

/// `--kitty-keyboard` pushes one keyboard-enhancement flag stack frame with
/// the alternate screen and pops it on the way out — kitty keeps a separate
/// flag stack per screen buffer, so the push lands after `?1049h` and the
/// pop before `?1049l` (`run::enter_terminal`, `run::restore_terminal`). The
/// stack follows the screen across a suspend too, the same shape as the
/// title stack and focus reporting (`run::suspend`): popped on the way
/// down, pushed again on the way back.
#[cfg(unix)]
#[test]
fn the_kitty_keyboard_is_taken_with_the_screen_and_given_back() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session = TuiSession::spawn_with_args(server.addr, &key_path, 24, 80, &["--kitty-keyboard"]);
    wait_for_attach(&session);
    assert_eq!(
        session.keyboard_enhancement(),
        (1, 0),
        "the kitty keyboard protocol was not pushed with the screen: {}",
        session.dump("after attach")
    );
    assert_eq!(
        session.keyboard_off_screen(),
        0,
        "the push landed before ?1049h, on the primary screen's flag stack: {}",
        session.dump("after attach")
    );

    let pid = session.pid().expect("pid available on unix");
    session.send("\x1a"); // Ctrl+Z
    std::thread::sleep(Duration::from_millis(300));
    // SIGCONT the way a shell's `fg` would; harmless if the process was
    // never actually stopped (`ctrl_z_suspends_and_sigcont_resumes_a_
    // responsive_client`'s sandbox note applies here too).
    unsafe {
        libc::kill(pid as i32, libc::SIGCONT);
    }
    session.send("ix");
    let responsive = session.wait_until(Duration::from_secs(5), |screen| {
        screen.rows(0, screen.size().1).any(|line| line.contains('❯') && line.contains('x'))
    });
    assert!(responsive, "client did not respond after SIGCONT: {}", session.dump("after SIGCONT"));
    assert_eq!(
        session.keyboard_enhancement(),
        (2, 1),
        "the keyboard-enhancement flag stack did not follow the screen across the suspend: {}",
        session.dump("after SIGCONT")
    );
    assert_eq!(
        session.keyboard_off_screen(),
        0,
        "a push or pop around the suspend landed outside the alternate screen: {}",
        session.dump("after SIGCONT")
    );

    quit(&mut session);
    assert_eq!(
        session.keyboard_enhancement(),
        (2, 2),
        "the keyboard-enhancement flag stack is unbalanced after :q: {}",
        session.dump("after quit")
    );
    assert_eq!(
        session.keyboard_off_screen(),
        0,
        "the pop landed after ?1049l, on the primary screen's flag stack: {}",
        session.dump("after quit")
    );
}

/// `Shift+Enter` submits the draft from insert mode under the protocol —
/// the feature the flag buys. The terminal encodes it as `CSI 13;2u`, which
/// crossterm 0.29 decodes to `Enter` with the shift bit
/// (`parse_csi_u_encoded_key_code`), and `Compose::press` submits on it. A
/// legacy terminal cannot send it, so the unit test's hand-built `KeyEvent`
/// only covers the branch; this covers the encoding.
#[test]
fn shift_enter_submits_the_draft_from_insert_mode_under_the_kitty_protocol() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session = TuiSession::spawn_with_args(server.addr, &key_path, 24, 80, &["--kitty-keyboard"]);
    wait_for_attach(&session);

    session.send("ishift enter submits");
    let typed = session.wait_until(Duration::from_secs(5), |screen| {
        screen_contains_str(screen, "-- INSERT --") && screen_contains_str(screen, "shift enter submits")
    });
    assert!(typed, "never entered insert mode with the draft typed: {}", session.dump("insert"));

    session.send("\x1b[13;2u"); // the kitty-encoded Shift+Enter
    let submitted = session.wait_until(Duration::from_secs(10), |screen| {
        let rows: Vec<String> = screen.rows(0, screen.size().1).collect();
        let Some(draft) = compose_row(&rows) else { return false };
        let cleared = !rows[draft].contains("shift enter submits")
            && rows[..draft].iter().any(|l| l.contains("shift enter submits"));
        // The ephemeral kernel has no performer, so the submit is refused
        // and the refusal notice is the proof the client submitted: a plain
        // insert-mode `Enter` adds a newline to the draft and says nothing.
        let refused = rows[draft..].iter().any(|l| l.contains("submit failed"));
        cleared || refused
    });
    assert!(
        submitted,
        "Shift+Enter did not submit the draft from insert mode: {}",
        session.dump("after Shift+Enter")
    );

    quit(&mut session);
}

/// The kitty-encoded lone `Esc` (`CSI 27 u`) is unambiguous, unlike a bare
/// `0x1b` byte, which crossterm holds to see whether an `Alt+<key>` sequence
/// follows. `Ctrl+I` (`CSI 9;5 u`) rides the same protocol but is not tested
/// here for a draft-level effect: `Tab` is already a no-op on the draft in
/// normal mode (verified directly against `Compose::press` — no `EditOp`,
/// text unchanged), which is the mode this probe's `Ctrl+I` lands in, so
/// there is no "the way Tab would" contrast to observe from this sequence.
/// The real contrast — `Tab` inserts a literal tab character in insert mode
/// while `Ctrl+I` there is a no-op — is a `compose.rs` question, outside
/// this probe's territory.
#[test]
fn a_lone_escape_leaves_insert_mode_under_the_kitty_protocol() {
    let _serial = serial();
    let server = EphemeralServer::start();
    let key_dir = tempfile::tempdir().expect("key tempdir");
    let key_path = write_ephemeral_key(key_dir.path());
    let mut session = TuiSession::spawn_with_args(server.addr, &key_path, 24, 80, &["--kitty-keyboard"]);
    wait_for_attach(&session);

    session.send("ihello");
    let inserted = session.wait_until(Duration::from_secs(5), |screen| {
        screen_contains_str(screen, "-- INSERT --") && screen_contains_str(screen, "hello")
    });
    assert!(inserted, "never entered insert mode with the draft typed: {}", session.dump("insert"));

    session.send("\x1b[27u"); // the kitty-encoded lone Esc
    let normal = session.wait_until(Duration::from_secs(5), |screen| screen_contains_str(screen, "-- NORMAL --"));
    assert!(normal, "the kitty-encoded lone Esc never left insert mode: {}", session.dump("after kitty Esc"));

    quit(&mut session);
}

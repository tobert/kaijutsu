//! Key events → intents, including the `Ctrl+A` prefix.
//!
//! The prefix table is `docs/input.md`, "The prefix table", ported verbatim;
//! this lane answers the chords a skeleton can honestly answer and names the
//! rest in the status line rather than swallowing them.
//!
//! Pure: no clock, no terminal. The double-tap windows live in
//! [`crate::app`], which is where the `Instant` is handed in.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What a keystroke asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Nothing to do, and nothing to say about it.
    Ignored,
    /// Switch to a ring-0 seat.
    SwitchSeat(usize),
    /// `Ctrl+A Ctrl+A` — toggle to the previous context.
    LastContext,
    /// `Ctrl+C` — the caller decides whether this is the second press.
    Interrupt,
    /// A key for compose's `VimMachine`, which owns both the draft and the
    /// `:` bar. The event rides through undecoded because modalkit reads it
    /// directly (`kaijutsu-editor` drives modalkit on
    /// `crossterm::event::KeyEvent`) and because what `Enter` or `Esc` means
    /// depends on the vi mode, which this module deliberately does not know.
    InputKey(KeyEvent),
    /// `Ctrl+Z` — suspend the process the way a shell job does (`docs/tui.md`,
    /// "The `:` line": the `Ctrl+Z` shell surface retired in favor of `:!`,
    /// so this is now a single-press suspend, not a toggle).
    Suspend,
    /// A chord a later lane owns. The text is what the status line says, so
    /// a key is never swallowed silently.
    NotYet(&'static str),
    /// The prefix was armed or cancelled; the legend line changed.
    LegendChanged,
    /// `Ctrl+A v` — open the diff viewer on the newest diff block in the
    /// current context. The app's `v`-on-a-focused-block gesture, ported:
    /// nothing on the wire opens a diff view, so this is a local decision
    /// about a block the context already holds (`docs/tui.md`, "Editor and
    /// diff").
    OpenDiff,
    /// `Ctrl+A l` — open the ledger view (`docs/tui.md`, "The ledger").
    OpenLedger,
    /// Bare `Tab`. Only meaningful when the compose draft starts with `/`
    /// (`crate::completion`); the caller decides that, not this module —
    /// `Keys::interpret` has no view of the compose line.
    Tab,
    /// `Ctrl+A "` / `Ctrl+A w` — open or close the picker
    /// (`docs/tui.md`, "The picker"). While the picker is open, keys are
    /// routed to it directly rather than through [`Keys::interpret`] — see
    /// `run.rs`.
    TogglePicker,
}

/// The prefix state machine.
#[derive(Debug, Default, Clone, Copy)]
pub struct Keys {
    armed: bool,
}

impl Keys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `Ctrl+A` is pending, which is when the legend replaces the
    /// status line.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Interpret one key event.
    pub fn interpret(&mut self, key: KeyEvent) -> Intent {
        // Release events arrive only where the terminal negotiated the
        // enhanced keyboard protocol; acting on both edges would double
        // every keystroke.
        if key.kind == KeyEventKind::Release {
            return Intent::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if self.armed {
            self.armed = false;
            return match key.code {
                KeyCode::Char('a') if ctrl => Intent::LastContext,
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    Intent::SwitchSeat(c as usize - '0' as usize)
                }
                KeyCode::Char('"') | KeyCode::Char('w') => Intent::TogglePicker,
                KeyCode::Char('l') => Intent::OpenLedger,
                KeyCode::Char('v') => Intent::OpenDiff,
                KeyCode::Char('\'') | KeyCode::Char('A') | KeyCode::Char('q')
                | KeyCode::Char('n') | KeyCode::Char('p') | KeyCode::Char('d')
                | KeyCode::Char('h') => Intent::NotYet("chord: later lane"),
                KeyCode::Esc => Intent::LegendChanged,
                _ => Intent::NotYet("unbound chord"),
            };
        }

        match key.code {
            KeyCode::Char('a') if ctrl => {
                self.armed = true;
                Intent::LegendChanged
            }
            KeyCode::Char('c') if ctrl => Intent::Interrupt,
            KeyCode::Char('z') if ctrl => Intent::Suspend,
            // Everything else belongs to the input region. Control chords go
            // too: `<C-w>` and `<C-r>` are vi keys, and the two this client
            // reserves are already claimed above.
            // Bare `Tab` is completion when the draft is a `/` command and
            // compose text otherwise; `run.rs` decides, this module cannot.
            KeyCode::Tab if !ctrl => Intent::Tab,
            _ => Intent::InputKey(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// An unclaimed key reaches the input region undecoded: the compose
    /// `VimMachine` is what decides whether `h` is text or a motion.
    #[test]
    fn an_unclaimed_key_reaches_the_input_region() {
        let mut keys = Keys::new();
        for code in [KeyCode::Char('h'), KeyCode::Backspace, KeyCode::Enter, KeyCode::Esc] {
            assert_eq!(keys.interpret(press(code)), Intent::InputKey(press(code)));
        }
        let chord = ctrl('w');
        assert_eq!(keys.interpret(chord), Intent::InputKey(chord), "vi keeps its ctrl chords");
    }

    #[test]
    fn ctrl_a_arms_the_prefix_and_digits_pick_a_seat() {
        let mut keys = Keys::new();
        assert_eq!(keys.interpret(ctrl('a')), Intent::LegendChanged);
        assert!(keys.armed());
        assert_eq!(keys.interpret(press(KeyCode::Char('3'))), Intent::SwitchSeat(3));
        assert!(!keys.armed(), "the prefix disarms after one chord");
    }

    #[test]
    fn ctrl_a_ctrl_a_toggles_to_the_previous_context() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(ctrl('a')), Intent::LastContext);
    }

    #[test]
    fn an_armed_digit_is_never_compose_text() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_ne!(
            keys.interpret(press(KeyCode::Char('0'))),
            Intent::InputKey(press(KeyCode::Char('0')))
        );
    }

    #[test]
    fn esc_cancels_the_prefix_without_firing_a_chord() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(press(KeyCode::Esc)), Intent::LegendChanged);
        assert!(!keys.armed());
    }

    #[test]
    fn ctrl_a_quote_opens_the_picker() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(press(KeyCode::Char('"'))), Intent::TogglePicker);
    }


    /// `Ctrl+Z` is never compose text — it suspends, one press, no toggle.
    #[test]
    fn ctrl_z_is_a_suspend_intent() {
        let mut keys = Keys::new();
        assert_eq!(keys.interpret(ctrl('z')), Intent::Suspend);
    }

    #[test]
    fn ctrl_a_v_opens_the_diff_viewer() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(press(KeyCode::Char('v'))), Intent::OpenDiff);
        assert!(!keys.armed());
    }

    #[test]
    fn ctrl_a_l_opens_the_ledger() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(press(KeyCode::Char('l'))), Intent::OpenLedger);
    }

    #[test]
    fn bare_tab_is_never_swallowed() {
        let mut keys = Keys::new();
        assert_eq!(keys.interpret(press(KeyCode::Tab)), Intent::Tab);
    }

    #[test]
    fn ctrl_c_is_an_interrupt_the_caller_times() {
        let mut keys = Keys::new();
        assert_eq!(keys.interpret(ctrl('c')), Intent::Interrupt);
    }

    #[test]
    fn a_release_event_does_nothing() {
        let mut keys = Keys::new();
        let mut ev = press(KeyCode::Char('x'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(keys.interpret(ev), Intent::Ignored);
    }
}

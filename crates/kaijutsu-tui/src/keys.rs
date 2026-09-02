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
    /// Type into the compose line.
    ComposeInsert(char),
    ComposeBackspace,
    /// `Enter` — submit the compose line through the kernel's input surface.
    Submit,
    /// A chord a later lane owns. The text is what the status line says, so
    /// a key is never swallowed silently.
    NotYet(&'static str),
    /// The prefix was armed or cancelled; the legend line changed.
    LegendChanged,
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
                KeyCode::Char('"') | KeyCode::Char('w') => Intent::NotYet("picker: later lane"),
                KeyCode::Char('l') => Intent::NotYet("ledger: later lane"),
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
            KeyCode::Char('z') if ctrl => Intent::NotYet("shell surface: later lane"),
            KeyCode::Enter => Intent::Submit,
            KeyCode::Backspace => Intent::ComposeBackspace,
            // A control chord that reached here is not compose text.
            KeyCode::Char(c) if !ctrl => Intent::ComposeInsert(c),
            _ => Intent::Ignored,
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

    #[test]
    fn typing_edits_the_compose_line() {
        let mut keys = Keys::new();
        assert_eq!(keys.interpret(press(KeyCode::Char('h'))), Intent::ComposeInsert('h'));
        assert_eq!(keys.interpret(press(KeyCode::Backspace)), Intent::ComposeBackspace);
        assert_eq!(keys.interpret(press(KeyCode::Enter)), Intent::Submit);
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
            Intent::ComposeInsert('0')
        );
    }

    #[test]
    fn esc_cancels_the_prefix_without_firing_a_chord() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(keys.interpret(press(KeyCode::Esc)), Intent::LegendChanged);
        assert!(!keys.armed());
    }

    /// A key a later lane owns is named, never swallowed.
    #[test]
    fn the_picker_and_the_shell_surface_say_they_are_later_lanes() {
        let mut keys = Keys::new();
        keys.interpret(ctrl('a'));
        assert_eq!(
            keys.interpret(press(KeyCode::Char('"'))),
            Intent::NotYet("picker: later lane")
        );
        assert_eq!(
            keys.interpret(ctrl('z')),
            Intent::NotYet("shell surface: later lane")
        );
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

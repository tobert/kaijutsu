//! The desktop notification for an ask that lands while the terminal is
//! unfocused.

use kaijutsu_types::ContextId;

/// What one ledger fold saw about a newly open ask — the whole input to
/// [`notify_target`], so the rule is one pure decision rather than a chain
/// of conditions at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifySeen {
    /// Whether the terminal reports itself focused (`App::focused`).
    pub focused: bool,
    /// Whether this was the session's first synced fold. It brings every
    /// ask already open, including asks raised before this client attached;
    /// it is the baseline, not news.
    pub baseline: bool,
    /// The context of the ask the fold found newly open: one in the context
    /// on screen when there is one ([`super::LedgerFold::raised_in`]).
    pub raised_in: Option<ContextId>,
    /// The context on screen when the fold landed.
    pub current: Option<ContextId>,
}

/// The context whose new ask is worth a desktop notification, or `None`.
///
/// One ask, one notification, and only for the seat on screen: an ask in
/// another context is that seat's `!`, not a toast.
pub fn notify_target(seen: NotifySeen) -> Option<ContextId> {
    if seen.focused || seen.baseline {
        return None;
    }
    let raised_in = seen.raised_in?;
    (seen.current == Some(raised_in)).then_some(raised_in)
}

/// The desktop notification for an ask that landed while nobody was
/// looking, or `None` while the terminal has focus — an ask you are sitting
/// in front of is the card on screen, not a toast
/// (`docs/tui.md`, "What owning the screen lets us use").
///
/// Two sequences, because no one escape covers the target terminals: OSC
/// 777 (`notify`) is wezterm, kitty, foot and rxvt-unicode; OSC 9 is
/// iTerm2. A terminal that knows neither ignores both. Control bytes are
/// dropped from `label`: one inside a sequence would end it early and print
/// the rest onto the screen.
pub fn ask_notification(focused: bool, label: &str) -> Option<String> {
    if focused {
        return None;
    }
    let label: String = label.chars().filter(|c| !c.is_control()).collect();
    let body = format!("{label}: ask pending");
    Some(format!("\x1b]777;notify;kaijutsu;{body}\x1b\\\x1b]9;{body}\x1b\\"))
}

#[cfg(test)]
mod notify_tests {
    use super::*;
    use kaijutsu_types::ContextId;

    /// The round that first polls the ledger reports every ask already
    /// pending as new — asks raised before this client attached, some of
    /// them hours old. They are the baseline, not news.
    #[test]
    fn the_first_poll_round_of_a_session_never_notifies() {
        let ctx = ContextId::new();
        let seen = NotifySeen { focused: false, baseline: true, raised_in: Some(ctx), current: Some(ctx) };
        assert_eq!(notify_target(seen), None);
    }

    /// A switch can land between a round starting and its answer arriving.
    /// The seat the ask belongs to is no longer the seat on screen, and the
    /// notification would name the context the player just left.
    #[test]
    fn an_ask_for_a_context_no_longer_on_screen_never_notifies() {
        let seen = NotifySeen {
            focused: false,
            baseline: false,
            raised_in: Some(ContextId::new()),
            current: Some(ContextId::new()),
        };
        assert_eq!(notify_target(seen), None);
    }

    #[test]
    fn an_ask_raised_out_of_focus_on_the_seat_on_screen_notifies() {
        let ctx = ContextId::new();
        let seen = NotifySeen { focused: false, baseline: false, raised_in: Some(ctx), current: Some(ctx) };
        assert_eq!(notify_target(seen), Some(ctx));

        let focused = NotifySeen { focused: true, ..seen };
        assert_eq!(notify_target(focused), None);

        let nothing = NotifySeen { raised_in: None, ..seen };
        assert_eq!(notify_target(nothing), None);
    }

    #[test]
    fn a_focused_terminal_is_never_notified() {
        assert_eq!(ask_notification(true, "probe"), None);
    }

    #[test]
    fn an_unfocused_terminal_gets_one_notification_of_each_kind() {
        let bytes = ask_notification(false, "probe").expect("an unfocused terminal is notified");
        assert_eq!(bytes.matches("\x1b]777;notify;kaijutsu;").count(), 1, "got {bytes:?}");
        assert_eq!(bytes.matches("\x1b]9;").count(), 1, "got {bytes:?}");
        assert!(bytes.contains("probe: ask pending"), "got {bytes:?}");
    }

    /// A label carries whatever a player typed. An escape byte inside one
    /// would end the sequence early and print the rest onto the screen, so
    /// control bytes never reach the terminal.
    #[test]
    fn a_control_byte_in_a_label_never_reaches_the_terminal() {
        let bytes = ask_notification(false, "pr\x1b]0;pwned\x07obe").expect("notified");
        // Two sequences, each an `ESC ]` introducer and an `ESC \` terminator.
        assert_eq!(bytes.matches('\x1b').count(), 4, "no escape but the four it writes: {bytes:?}");
        assert!(!bytes.contains("\x07"), "got {bytes:?}");
        assert!(bytes.contains("pr]0;pwnedobe: ask pending"), "got {bytes:?}");
    }
}

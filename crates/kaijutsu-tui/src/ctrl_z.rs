//! `Ctrl+Z`: the toggle between a person's root console (or the console)
//! and a context the person performs under it, and the double tap that
//! suspends the process (`docs/tui.md`, "Keys").
//!
//! Pure: no RPC, no terminal, no clock of its own. [`toggle`] turns the
//! context on screen and the listing into a [`Toggle`]; the caller makes the
//! `create` call or runs `switch_seat`. [`Tap::press`] takes the instant in.
//! The double-tap window is [`crate::app::DOUBLE_TAP`], the window `Ctrl+C`
//! and `Ctrl+A Ctrl+A` use. The wire facts decide (`is_root_console`,
//! `is_console`, `performed_by_person`), never a label.

use std::time::Instant;

use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;

use crate::app::DOUBLE_TAP;

/// What one `Ctrl+Z` press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggle {
    /// Switch the screen to this context: the live context the person
    /// performs under a root console or the console, or the context a
    /// person's context was opened from.
    Switch(ContextId),
    /// A root console or the console with no live context the person
    /// performs under it: create one, `kj context create <person>`, from
    /// `parent`, labelled `label`, and then switch to it.
    Create { parent: ContextId, label: String },
    /// The context on screen is neither a root console, the console, nor a
    /// context a person performs. Say so; do not move.
    Refuse(&'static str),
}

/// The label a person's `Ctrl+Z` context is created with:
/// `<parent-label>-shell`.
pub fn shell_label(root_label: &str) -> String {
    format!("{root_label}-shell")
}

/// Decide what `Ctrl+Z` does from the context on screen and the kernel's
/// listing.
///
/// From a root console or the console: the live context a person performs
/// that was opened from it, else a create. A child with the predictable
/// label is preferred, so two such children (one made by hand) resolve the
/// same way every time. From any other context a person performs: the
/// context it was opened from. From anything else: refuse.
pub fn toggle(current: &ContextInfo, contexts: &[ContextInfo]) -> Toggle {
    if current.is_root_console || current.is_console {
        let children: Vec<&ContextInfo> = contexts
            .iter()
            .filter(|c| {
                c.performed_by_person
                    && !c.is_root_console
                    && c.forked_from == Some(current.id)
                    && is_live(c)
            })
            .collect();
        let want = shell_label(&current.label);
        return match children.iter().find(|c| c.label == want).or(children.first()) {
            Some(child) => Toggle::Switch(child.id),
            None => Toggle::Create { parent: current.id, label: want },
        };
    }
    if current.performed_by_person {
        return match current.forked_from {
            Some(parent) => Toggle::Switch(parent),
            None => Toggle::Refuse("Ctrl+Z: this context has no context it was opened from to return to"),
        };
    }
    Toggle::Refuse("Ctrl+Z toggles a root console or the console and a context you perform under it; this context is neither")
}

/// Archived and concluded contexts are not reused: a live child is the one
/// `Ctrl+Z` returns to.
fn is_live(context: &ContextInfo) -> bool {
    !context.archived && context.concluded_at.is_none()
}

/// The `Ctrl+Z` double tap: the second press within [`crate::app::DOUBLE_TAP`]
/// of the first. Mirrors `interrupt::Ladder`'s window, without its escalation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tap {
    last: Option<Instant>,
}

impl Tap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a press at `now`. Returns `true` when it completes a double
    /// tap. A completed double tap clears the pending first press, so a
    /// third press starts a fresh tap rather than suspending again.
    pub fn press(&mut self, now: Instant) -> bool {
        let second = self.last.is_some_and(|prev| now.duration_since(prev) <= DOUBLE_TAP);
        self.last = if second { None } else { Some(now) };
        second
    }

    /// Forget any pending first press, e.g. after the process has been
    /// suspended and resumed.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DOUBLE_TAP;
    use kaijutsu_types::Status;
    use std::time::Duration;

    /// A context a model character performs.
    fn ctx(label: &str, forked_from: Option<ContextId>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
            input_tokens: None,
            cost: None,
            performer: None,
            performed_by_person: false,
            is_root_console: false,
            is_console: false,
            is_mcp_session: false,
        }
    }

    /// A person's root console.
    fn root(label: &str) -> ContextInfo {
        let mut info = ctx(label, None);
        info.performed_by_person = true;
        info.is_root_console = true;
        info
    }

    /// A context the person performs, opened from `parent`.
    fn shell_of(parent: &ContextInfo, label: &str) -> ContextInfo {
        let mut info = ctx(label, Some(parent.id));
        info.performed_by_person = true;
        info
    }

    /// The model console is a root console for this purpose: the person's
    /// context under it is the one Ctrl+Z goes to, so the console and the
    /// root console toggle the same way. Its label decides nothing.
    #[test]
    fn a_console_toggles_to_its_shell_child_and_creates_one_when_missing() {
        let mut console = ctx("banto", Some(ContextId::new()));
        console.is_console = true;
        let child = shell_of(&console, "banto-shell");
        assert_eq!(toggle(&console, &[console.clone(), child.clone()]), Toggle::Switch(child.id));
        assert_eq!(
            toggle(&console, &[console.clone()]),
            Toggle::Create { parent: console.id, label: "banto-shell".into() },
        );
    }

    #[test]
    fn a_root_with_no_shell_child_creates_one_with_the_predictable_label() {
        let amy = root("amy");
        let contexts = vec![amy.clone()];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    #[test]
    fn a_root_switches_to_its_live_shell_child() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(toggle(&amy, &contexts), Toggle::Switch(shell.id));
    }

    /// The reuse mutation: a root that ignores existing children would create
    /// a second shell every press.
    #[test]
    fn a_root_reuses_the_child_it_already_has_and_does_not_create_another() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert!(!matches!(toggle(&amy, &contexts), Toggle::Create { .. }));
    }

    /// A model's context under the root, even one labelled like the shell,
    /// is not the person's: Ctrl+Z creates the person's own.
    ///
    /// Falsified by matching children by label or by parent alone.
    #[test]
    fn a_model_s_child_under_the_root_is_not_the_person_s() {
        let amy = root("amy");
        let lane = ctx("amy-shell", Some(amy.id));
        let contexts = vec![amy.clone(), lane];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    #[test]
    fn an_archived_or_concluded_shell_child_is_not_reused() {
        let amy = root("amy");
        let mut archived = shell_of(&amy, "amy-shell");
        archived.archived = true;
        let mut concluded = shell_of(&amy, "amy-shell-2");
        concluded.concluded_at = Some(2_000);
        let contexts = vec![amy.clone(), archived, concluded];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// A shell under another root is not this root's shell.
    #[test]
    fn a_shell_under_another_root_is_not_this_roots() {
        let amy = root("amy");
        let bob = root("bob");
        let theirs = shell_of(&bob, "bob-shell");
        let contexts = vec![amy.clone(), bob, theirs];
        assert_eq!(
            toggle(&amy, &contexts),
            Toggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// Two shell children: the one with the predictable label wins, whatever
    /// the listing order.
    #[test]
    fn the_predictable_label_wins_among_shell_children() {
        let amy = root("amy");
        let other = shell_of(&amy, "scratch");
        let named = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), other, named.clone()];
        assert_eq!(toggle(&amy, &contexts), Toggle::Switch(named.id));
    }

    #[test]
    fn a_shell_switches_back_to_its_parent() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(toggle(&shell, &contexts), Toggle::Switch(amy.id));
    }

    #[test]
    fn a_shell_with_no_parent_refuses_rather_than_guessing() {
        let mut orphan = ctx("orphan", None);
        orphan.performed_by_person = true;
        assert!(matches!(toggle(&orphan, &[orphan.clone()]), Toggle::Refuse(_)));
    }

    /// A context a model performs does nothing: `Ctrl+Z` never creates under
    /// a coder, whatever its label.
    #[test]
    fn any_other_context_refuses_and_creates_nothing() {
        let lane = ctx("console", Some(ContextId::new()));
        let contexts = vec![lane.clone()];
        assert!(matches!(toggle(&lane, &contexts), Toggle::Refuse(_)));
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_first_press_is_never_a_double_tap() {
        let mut tap = Tap::new();
        assert!(!tap.press(t0()));
    }

    #[test]
    fn a_second_press_within_the_window_completes_the_double_tap() {
        let mut tap = Tap::new();
        let start = t0();
        assert!(!tap.press(start));
        assert!(tap.press(start + Duration::from_millis(200)));
    }

    /// Same boundary as `Ladder`: a press exactly at the window still counts.
    #[test]
    fn a_second_press_at_the_window_edge_counts() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        assert!(tap.press(start + DOUBLE_TAP));
    }

    /// The window mutation: a stale first press must not pair with a late
    /// second one. The late press starts a new tap, and the one after it
    /// completes that.
    #[test]
    fn a_press_outside_the_window_starts_over() {
        let mut tap = Tap::new();
        let start = t0();
        assert!(!tap.press(start));
        assert!(!tap.press(start + Duration::from_millis(600)));
        assert!(tap.press(start + Duration::from_millis(800)));
    }

    /// A completed double tap leaves nothing pending: the next press in the
    /// same window is a first press, not a third that suspends again.
    #[test]
    fn a_completed_double_tap_leaves_no_pending_first_press() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        assert!(tap.press(start + Duration::from_millis(100)));
        assert!(!tap.press(start + Duration::from_millis(200)));
    }

    #[test]
    fn reset_forgets_a_pending_first_press() {
        let mut tap = Tap::new();
        let start = t0();
        tap.press(start);
        tap.reset();
        assert!(!tap.press(start + Duration::from_millis(100)));
    }
}

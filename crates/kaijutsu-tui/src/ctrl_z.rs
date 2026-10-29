//! `Ctrl+Z`: the toggle between a root context and its shell child, and the
//! double tap that suspends the process (`docs/tui.md`, "Keys").
//!
//! Pure: no RPC, no terminal, no clock of its own. [`toggle`] turns the
//! context on screen and the listing into a [`Toggle`]; the caller makes the
//! `create` call or runs `switch_seat`. [`Tap::press`] takes the instant in.
//! The double-tap window is [`crate::app::DOUBLE_TAP`], the window `Ctrl+C`
//! and `Ctrl+A Ctrl+A` use.

use std::time::Instant;

use kaijutsu_client::ContextInfo;
use kaijutsu_types::{CONSOLE_CONTEXT_TYPE, ContextId, ROOT_CONTEXT_TYPE};

use crate::app::DOUBLE_TAP;

/// The context type of a root's shell child: a child whose chat submit runs
/// as shell.
pub const SHELL_CONTEXT_TYPE: &str = "shell";

/// What one `Ctrl+Z` press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggle {
    /// Switch the screen to this context: a root's live shell child, or a
    /// shell's parent.
    Switch(ContextId),
    /// A root with no live shell child: create one under `parent`, labelled
    /// `label`, and then switch to it.
    Create { parent: ContextId, label: String },
    /// The context on screen is neither a root nor a shell. Say so; do not
    /// move.
    Refuse(&'static str),
}

/// The label a root's shell child is created with: `<root-label>-shell`.
pub fn shell_label(root_label: &str) -> String {
    format!("{root_label}-shell")
}

/// Decide what `Ctrl+Z` does from the context on screen and the kernel's
/// listing.
///
/// From a root: its live shell child, else a create. A child under the same
/// root with the predictable label is preferred, so two shell children (one
/// made by hand) resolve the same way every time. From a shell: its parent.
/// From any other type: refuse.
pub fn toggle(current: &ContextInfo, contexts: &[ContextInfo]) -> Toggle {
    match current.context_type.as_str() {
        ROOT_CONTEXT_TYPE | CONSOLE_CONTEXT_TYPE => {
            let children: Vec<&ContextInfo> = contexts
                .iter()
                .filter(|c| {
                    c.context_type == SHELL_CONTEXT_TYPE
                        && c.forked_from == Some(current.id)
                        && is_live(c)
                })
                .collect();
            let want = shell_label(&current.label);
            match children.iter().find(|c| c.label == want).or(children.first()) {
                Some(child) => Toggle::Switch(child.id),
                None => Toggle::Create { parent: current.id, label: want },
            }
        }
        SHELL_CONTEXT_TYPE => match current.forked_from {
            Some(parent) => Toggle::Switch(parent),
            None => Toggle::Refuse("Ctrl+Z: this shell has no root to return to"),
        },
        _ => Toggle::Refuse("Ctrl+Z toggles a root and its shell; this context is neither"),
    }
}

/// Archived and concluded contexts are not reused: a live shell child is the
/// one `Ctrl+Z` returns to.
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

    fn ctx(label: &str, context_type: &str, forked_from: Option<ContextId>) -> ContextInfo {
        ContextInfo {
            id: ContextId::new(),
            label: label.to_string(),
            forked_from,
            provider: String::new(),
            model: String::new(),
            created_at: 1_000,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: context_type.to_string(),
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
            cast_label: None,
            origin_host: None,
            cwd: None,
            last_call_at: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cache_ttl_secs: None,
            input_tokens: None,
        }
    }

    fn root(label: &str) -> ContextInfo {
        ctx(label, ROOT_CONTEXT_TYPE, None)
    }

    fn shell_of(parent: &ContextInfo, label: &str) -> ContextInfo {
        ctx(label, SHELL_CONTEXT_TYPE, Some(parent.id))
    }

    /// The model console is a root for this purpose: its shell child is the
    /// one Ctrl+Z goes to, so the console and the root toggle the same way.
    #[test]
    fn a_console_toggles_to_its_shell_child_and_creates_one_when_missing() {
        let console = ctx("ROOT", CONSOLE_CONTEXT_TYPE, None);
        let child = shell_of(&console, "ROOT-shell");
        assert_eq!(toggle(&console, &[console.clone(), child.clone()]), Toggle::Switch(child.id));
        assert_eq!(
            toggle(&console, &[console.clone()]),
            Toggle::Create { parent: console.id, label: "ROOT-shell".into() },
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
        let orphan = ctx("orphan", SHELL_CONTEXT_TYPE, None);
        assert!(matches!(toggle(&orphan, &[orphan.clone()]), Toggle::Refuse(_)));
    }

    /// Any other type does nothing: `Ctrl+Z` never creates under a coder.
    #[test]
    fn any_other_context_refuses_and_creates_nothing() {
        let lane = ctx("lane", "coder", None);
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

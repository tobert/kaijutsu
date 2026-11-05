//! The `Ctrl+Z` decision, shared by the terminal client and the GUI client:
//! the toggle between a person's root console (or the console) and a context
//! the person performs under it (`docs/character.md`, "A person, the
//! console, MCP sessions, the judge").
//!
//! Pure: no RPC, no terminal, no clock. [`shell_toggle`] turns the context on
//! screen and the kernel's listing into a [`ShellToggle`]; the caller makes
//! the `create` call or moves the screen. The wire facts and the seat's
//! character decide (`is_root_console`, `is_console`, `performer`), never a
//! label.

use kaijutsu_types::ContextId;

use crate::rpc::ContextInfo;

/// What one `Ctrl+Z` press asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellToggle {
    /// Switch the screen to this context: the live context the person
    /// performs under a root console or the console, or the context a
    /// person's context was opened from.
    Switch(ContextId),
    /// A root console or the console with no live context the person
    /// performs under it: create one, `kj context create <person>`, from
    /// `parent`, with `--name label`, and then switch to it. `label` is one
    /// no live context holds, because the kernel refuses an explicit
    /// `--name` that a live context holds.
    Create { parent: ContextId, label: String },
    /// The press does nothing here: the context on screen is not a root
    /// console, the console, or a context the caller performs as a person,
    /// or the caller is not a person. Say why; do not move.
    Refuse(String),
}

/// The label a person's `Ctrl+Z` context is first offered:
/// `<parent-label>-shell`.
pub fn shell_label(root_label: &str) -> String {
    format!("{root_label}-shell")
}

/// `<parent-label>-shell`, else `-shell-2`, `-shell-3`, ... the first that no
/// live context in `contexts` holds. An archived context frees its label;
/// a concluded one does not, as in the kernel's unique index.
fn free_shell_label(parent_label: &str, contexts: &[ContextInfo]) -> String {
    let base = shell_label(parent_label);
    let held = |label: &str| contexts.iter().any(|c| !c.archived && c.label == label);
    if !held(&base) {
        return base;
    }
    (2u32..).map(|n| format!("{base}-{n}")).find(|label| !held(label)).expect("an unbounded range has a free label")
}

/// Decide what `Ctrl+Z` does from the context on screen and the kernel's
/// listing.
///
/// `caller` is the seat's character, the name `whoami` returns. From a root
/// console or the console: the live context `caller` performs that was
/// opened from it, else a create. A child with the predictable label is
/// preferred, so two such children (one made by hand) resolve the same way
/// every time. A caller that is not a person, and a person on another
/// person's root console, refuse. From any other context the caller
/// performs as a person: the context it was opened from. From anything
/// else: refuse.
pub fn shell_toggle(current: &ContextInfo, contexts: &[ContextInfo], caller: &str) -> ShellToggle {
    if current.is_root_console || current.is_console {
        if !is_person(caller, contexts) {
            return ShellToggle::Refuse(format!(
                "Ctrl+Z opens a context you perform as a person, and {caller} is not a person \
                 (no live root console is performed by {caller})"
            ));
        }
        if current.is_root_console && current.performer.as_deref() != Some(caller) {
            let owner = current.performer.as_deref().unwrap_or("nobody");
            return ShellToggle::Refuse(format!(
                "Ctrl+Z: this is {owner}'s root console, not {caller}'s; press it in your own root console"
            ));
        }
        let children: Vec<&ContextInfo> = contexts
            .iter()
            .filter(|c| {
                c.performer.as_deref() == Some(caller)
                    && !c.is_root_console
                    && c.forked_from == Some(current.id)
                    && is_live(c)
            })
            .collect();
        let want = shell_label(&current.label);
        return match children.iter().find(|c| c.label == want).or(children.first()) {
            Some(child) => ShellToggle::Switch(child.id),
            None => ShellToggle::Create { parent: current.id, label: free_shell_label(&current.label, contexts) },
        };
    }
    if current.performed_by_person && current.performer.as_deref() == Some(caller) {
        return match current.forked_from {
            Some(parent) => ShellToggle::Switch(parent),
            None => ShellToggle::Refuse("Ctrl+Z: this context has no context it was opened from to return to".into()),
        };
    }
    ShellToggle::Refuse("Ctrl+Z toggles a root console or the console and a context you perform under it; this context is neither".into())
}

/// Whether `caller` is a person: the listing holds a live root console it
/// performs. `whoami` carries no root flag, and a live root always has its
/// root console.
fn is_person(caller: &str, contexts: &[ContextInfo]) -> bool {
    contexts.iter().any(|c| c.is_root_console && c.performer.as_deref() == Some(caller) && is_live(c))
}

/// Archived and concluded contexts are not reused: a live child is the one
/// `Ctrl+Z` returns to.
fn is_live(context: &ContextInfo) -> bool {
    !context.archived && context.concluded_at.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::Status;

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

    /// A person's root console, performed by the person of the same name.
    fn root(label: &str) -> ContextInfo {
        let mut info = ctx(label, None);
        info.performer = Some(label.to_string());
        info.performed_by_person = true;
        info.is_root_console = true;
        info
    }

    /// A context `person` performs, opened from `parent`.
    fn shell_of(parent: &ContextInfo, label: &str, person: &str) -> ContextInfo {
        let mut info = ctx(label, Some(parent.id));
        info.performer = Some(person.to_string());
        info.performed_by_person = true;
        info
    }

    /// The model console is a root console for this purpose: the person's
    /// context under it is the one Ctrl+Z goes to, so the console and the
    /// root console toggle the same way. Its label decides nothing.
    #[test]
    fn a_console_toggles_to_its_shell_child_and_creates_one_when_missing() {
        let amy = root("amy");
        let mut console = ctx("banto", Some(amy.id));
        console.performer = Some("banto".into());
        console.is_console = true;
        let child = shell_of(&console, "banto-shell", "amy");
        assert_eq!(shell_toggle(&console, &[amy.clone(), console.clone(), child.clone()], "amy"), ShellToggle::Switch(child.id));
        assert_eq!(
            shell_toggle(&console, &[amy, console.clone()], "amy"),
            ShellToggle::Create { parent: console.id, label: "banto-shell".into() },
        );
    }

    #[test]
    fn a_root_with_no_shell_child_creates_one_with_the_predictable_label() {
        let amy = root("amy");
        let contexts = vec![amy.clone()];
        assert_eq!(
            shell_toggle(&amy, &contexts, "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    #[test]
    fn a_root_switches_to_its_live_shell_child() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell", "amy");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(shell_toggle(&amy, &contexts, "amy"), ShellToggle::Switch(shell.id));
    }

    /// The reuse mutation: a root that ignores existing children would create
    /// a second shell every press.
    #[test]
    fn a_root_reuses_the_child_it_already_has_and_does_not_create_another() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell", "amy");
        let contexts = vec![amy.clone(), shell.clone()];
        assert!(!matches!(shell_toggle(&amy, &contexts, "amy"), ShellToggle::Create { .. }));
    }

    /// A model's context under the root, even one labelled like the shell,
    /// is not the person's: Ctrl+Z creates the person's own, under a label
    /// the kernel has free.
    ///
    /// Falsified by matching children by label or by parent alone.
    #[test]
    fn a_model_s_child_under_the_root_is_not_the_person_s() {
        let amy = root("amy");
        let mut lane = ctx("amy-shell", Some(amy.id));
        lane.performer = Some("coder".into());
        let contexts = vec![amy.clone(), lane];
        assert_eq!(
            shell_toggle(&amy, &contexts, "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell-2".to_string() }
        );
    }

    /// Another person's context under amy's root console, even one labelled
    /// like her shell, is not amy's: Ctrl+Z creates her own.
    ///
    /// Falsified by counting a child any person performs.
    #[test]
    fn another_person_s_child_under_the_root_is_not_the_caller_s() {
        let amy = root("amy");
        let bob = root("bob");
        let theirs = shell_of(&amy, "amy-shell", "bob");
        let contexts = vec![amy.clone(), bob, theirs];
        assert_eq!(
            shell_toggle(&amy, &contexts, "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell-2".to_string() }
        );
    }

    /// A model character attached to a root console or the console is not a
    /// person: Ctrl+Z refuses, naming it, and creates nothing, so no press
    /// leaves a stray context it would not recognize next time.
    ///
    /// Falsified by creating for any caller.
    #[test]
    fn a_model_caller_on_a_root_console_or_the_console_refuses() {
        let amy = root("amy");
        let mut console = ctx("console", Some(amy.id));
        console.performer = Some("banto".into());
        console.is_console = true;
        let contexts = vec![amy.clone(), console.clone()];
        for on in [&amy, &console] {
            let ShellToggle::Refuse(why) = shell_toggle(on, &contexts, "banto") else {
                panic!("{}: a model caller must refuse", on.label)
            };
            assert!(why.contains("banto") && why.contains("not a person"), "{why}");
        }
    }

    /// A person on another person's root console refuses, naming whose it
    /// is, and creates nothing there.
    #[test]
    fn a_person_on_another_person_s_root_console_refuses() {
        let amy = root("amy");
        let bob = root("bob");
        let contexts = vec![amy.clone(), bob];
        let ShellToggle::Refuse(why) = shell_toggle(&amy, &contexts, "bob") else { panic!("bob must refuse on amy's root console") };
        assert!(why.contains("amy's root console"), "{why}");
    }

    #[test]
    fn an_archived_or_concluded_shell_child_is_not_reused() {
        let amy = root("amy");
        let mut archived = shell_of(&amy, "amy-shell", "amy");
        archived.archived = true;
        let mut concluded = shell_of(&amy, "amy-shell-2", "amy");
        concluded.concluded_at = Some(2_000);
        let contexts = vec![amy.clone(), archived, concluded];
        assert_eq!(
            shell_toggle(&amy, &contexts, "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// A shell under another root is not this root's shell.
    #[test]
    fn a_shell_under_another_root_is_not_this_roots() {
        let amy = root("amy");
        let bob = root("bob");
        let theirs = shell_of(&bob, "bob-shell", "amy");
        let contexts = vec![amy.clone(), bob, theirs];
        assert_eq!(
            shell_toggle(&amy, &contexts, "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell".to_string() }
        );
    }

    /// Two shell children: the one with the predictable label wins, whatever
    /// the listing order.
    #[test]
    fn the_predictable_label_wins_among_shell_children() {
        let amy = root("amy");
        let other = shell_of(&amy, "scratch", "amy");
        let named = shell_of(&amy, "amy-shell", "amy");
        let contexts = vec![amy.clone(), other, named.clone()];
        assert_eq!(shell_toggle(&amy, &contexts, "amy"), ShellToggle::Switch(named.id));
    }

    #[test]
    fn a_shell_switches_back_to_its_parent() {
        let amy = root("amy");
        let shell = shell_of(&amy, "amy-shell", "amy");
        let contexts = vec![amy.clone(), shell.clone()];
        assert_eq!(shell_toggle(&shell, &contexts, "amy"), ShellToggle::Switch(amy.id));
    }

    #[test]
    fn a_shell_with_no_parent_refuses_rather_than_guessing() {
        let mut orphan = ctx("orphan", None);
        orphan.performer = Some("amy".into());
        orphan.performed_by_person = true;
        assert!(matches!(shell_toggle(&orphan, &[orphan.clone()], "amy"), ShellToggle::Refuse(_)));
    }

    /// A context a model performs does nothing: `Ctrl+Z` never creates under
    /// a coder, whatever its label.
    #[test]
    fn any_other_context_refuses_and_creates_nothing() {
        let lane = ctx("console", Some(ContextId::new()));
        let contexts = vec![lane.clone()];
        assert!(matches!(shell_toggle(&lane, &contexts, "amy"), ShellToggle::Refuse(_)));
    }

    #[test]
    fn shell_label_is_root_label_with_shell_suffix() {
        assert_eq!(shell_label("amy"), "amy-shell");
    }

    /// The kernel refuses `--name` when a live context holds the label, and
    /// gives an explicit name no first-free fallback: the decision offers a
    /// free one. A model's context labelled `amy-shell` is not the person's,
    /// and holds the label.
    ///
    /// Falsified by offering `<parent>-shell` whatever the listing holds.
    #[test]
    fn a_label_a_live_context_holds_is_not_offered() {
        let amy = root("amy");
        let mut lane = ctx("amy-shell", Some(amy.id));
        lane.performer = Some("coder".into());
        let mut second = ctx("amy-shell-2", None);
        second.performer = Some("coder".into());
        assert_eq!(
            shell_toggle(&amy, &[amy.clone(), lane.clone()], "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell-2".to_string() }
        );
        assert_eq!(
            shell_toggle(&amy, &[amy.clone(), lane, second], "amy"),
            ShellToggle::Create { parent: amy.id, label: "amy-shell-3".to_string() }
        );
    }

    /// A label held by a context anywhere in the listing counts, not only
    /// one under this parent; an archived holder frees it; a concluded one
    /// does not (the kernel's index covers every unarchived row).
    #[test]
    fn only_an_archived_holder_frees_the_label() {
        let amy = root("amy");
        let mut elsewhere = ctx("amy-shell", None);
        elsewhere.performer = Some("coder".into());
        let mut concluded = elsewhere.clone();
        concluded.concluded_at = Some(2_000);
        let mut archived = elsewhere.clone();
        archived.archived = true;
        let create = |label: &str| ShellToggle::Create { parent: amy.id, label: label.to_string() };
        assert_eq!(shell_toggle(&amy, &[amy.clone(), elsewhere], "amy"), create("amy-shell-2"));
        assert_eq!(shell_toggle(&amy, &[amy.clone(), concluded], "amy"), create("amy-shell-2"));
        assert_eq!(shell_toggle(&amy, &[amy.clone(), archived], "amy"), create("amy-shell"));
    }
}

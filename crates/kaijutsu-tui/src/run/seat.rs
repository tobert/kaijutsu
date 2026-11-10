//! The seat: switching contexts, the shell toggle, closing, the `:` bar,
//! interrupts, and the draft's own edits.

use std::time::{Duration, Instant};

use kaijutsu_types::ContextId;

use crate::app::App;
use crate::bridge::KernelBridge;
use crate::cmdline::{self, ColonVerb};
use kaijutsu_client::{DraftEdit, follow_target};
use kaijutsu_client::draft_outbox::Landed;
use crate::completion;
use kaijutsu_client::{ShellToggle, shell_toggle};
use crate::interrupt::{self, Step as InterruptStep};

use super::feeds::{Feeds, release_cold, watch_context};

/// How long a latched `Ctrl+A q` waits for the same chord to confirm it,
/// the app's notice lifetime (`docs/input.md`, "Placement keys").
pub(crate) const CLOSE_CONFIRM_WINDOW: Duration = Duration::from_secs(6);

/// Whether this `Ctrl+A q` on `ctx` confirms the latched one in `armed`.
fn close_confirms(armed: Option<(ContextId, Instant)>, ctx: ContextId, now: Instant) -> bool {
    armed.is_some_and(|(id, at)| id == ctx && now.duration_since(at) < CLOSE_CONFIRM_WINDOW)
}

/// `Ctrl+A q`: run `kj context demote` on `ctx`, then go back to the
/// previous context. A latched step moves nothing and stays on `ctx`; the
/// same chord again within [`CLOSE_CONFIRM_WINDOW`] runs it with
/// `--confirm`. With no previous context the view stays where it is.
pub(super) async fn close_and_demote(bridge: &KernelBridge, app: &mut App, ctx: ContextId, feeds: &mut Feeds) {
    let now = Instant::now();
    let mut argv = vec!["context".to_string(), "demote".to_string(), ctx.to_hex()];
    if close_confirms(app.close_armed.take(), ctx, now) {
        argv.push("--confirm".to_string());
    }
    match bridge.execute_kj(ctx, argv).await {
        Ok(result) if result.latch.is_some() => {
            let message = result.latch.map(|l| l.message).unwrap_or_default();
            app.close_armed = Some((ctx, now));
            app.note(format!("Ctrl+A q again confirms: {message}"));
        }
        Ok(result) if result.exit_code != 0 => {
            let why = result.stderr.lines().next().unwrap_or("").trim().to_string();
            app.note(format!("demote failed (exit {}): {why}", result.exit_code));
        }
        Ok(result) => {
            let done = result.stdout.lines().next().unwrap_or("demoted").to_string();
            app.roster_changed = true;
            if let Some(previous) = app.previous.filter(|p| *p != ctx) {
                switch_seat(bridge, app, previous, feeds).await;
                if app.current != Some(previous) {
                    // `switch_seat` said why it could not go back.
                    return;
                }
            }
            app.note(done);
        }
        Err(e) => app.note(format!("demote failed: {e}")),
    }
}

/// Record the context on screen as this client's last view, so a start
/// without `--context` reopens it. Best-effort: a failed write costs the next
/// start its place, not this one.
pub(super) async fn remember_view(bridge: &KernelBridge, id: ContextId) {
    if let Err(e) = bridge.remember_context(id).await {
        tracing::warn!(context = %id.short(), error = %format!("{e:#}"), "could not record the last context");
    }
}

/// Switch the screen to `id`: watch it, make it current, and load its draft.
/// The draft is per context, so the compose buffer follows the switch rather
/// than carrying the old context's text along — `load_draft` keeps the `:`
/// line's own history, which is this session's, not this draft's.
///
/// `Ctrl+Z`'s toggle: a root console or the console to the live context the
/// person performs under it (created on first use), a person's context back
/// to the context it was opened from, and a notice from anywhere else
/// (`kaijutsu_client::shell_toggle`).
///
/// The listing is asked for here rather than read from the app, so a shell
/// child made a moment ago is found and not created twice. The switch goes
/// through [`switch_seat`] like every other.
pub(super) async fn toggle_shell(bridge: &KernelBridge, app: &mut App, feeds: &mut Feeds) {
    let Some(current) = app.current_info().cloned() else {
        app.note("no context attached");
        return;
    };
    let contexts = match bridge.list_contexts().await {
        Ok(contexts) => contexts,
        Err(e) => {
            app.note(format!("cannot list contexts: {e:#}"));
            return;
        }
    };
    let me = match bridge.identity().await {
        Ok(me) => me.username,
        Err(e) => {
            app.note(format!("cannot read who this seat is: {e:#}"));
            return;
        }
    };
    let target = match shell_toggle(&current, &contexts, &me) {
        ShellToggle::Switch(id) => id,
        ShellToggle::Refuse(why) => {
            app.note(why);
            return;
        }
        ShellToggle::Create { parent, label } => {
            match bridge.create_shell_child(parent, &label, &me).await {
                Ok((info, cwd_refused)) => {
                    if let Some(refused) = cwd_refused {
                        tracing::warn!(%refused, "the shell took the parent's cwd; the kernel refused this one");
                    }
                    // The app's listing predates the create; the screen,
                    // the submit and the next Ctrl+Z read the new context's
                    // facts from it before the next refresh lands.
                    let id = info.id;
                    let mut contexts = contexts;
                    contexts.push(info);
                    app.set_contexts(contexts);
                    id
                }
                Err(e) => {
                    app.note(format!("cannot create {label}: {e:#}"));
                    return;
                }
            }
        }
    };
    switch_seat(bridge, app, target, feeds).await;
}

/// **Every path that changes the context on screen goes through here**: the
/// prefix chords, `Ctrl+A Ctrl+A`, and the picker. A path that watched and
/// switched on its own got the switch without the draft, and
/// the draft ledger's ack floor then kept the change feed from correcting it.
///
/// A switch never fails silently and never tears the loop down: a context
/// that cannot be watched leaves the screen where it was with a notice, and
/// a draft that cannot be read says so rather than blanking the line.
pub(super) async fn switch_seat(
    bridge: &KernelBridge,
    app: &mut App,
    id: ContextId,
    feeds: &mut Feeds,
) {
    if let Err(e) = watch_context(bridge, app, id, feeds).await {
        app.note(format!("cannot watch {}: {e}", app.label_for(id)));
        return;
    }
    // Switching back to a context whose last keystrokes are still on the
    // wire would read the draft without them, so the read queues behind them.
    let draft = app.drafts.read(id).await;
    app.switch_to(id);
    remember_view(bridge, id).await;
    app.compose.load_draft(draft.as_deref().unwrap_or(""));
    // The switch moved both ends of the hot set — the context on screen and
    // the one `Ctrl+A Ctrl+A` goes back to — so whatever fell out of it is
    // released now. Only the release half: the watch half is a kernel round
    // trip and belongs to the refresh round, and the one context this switch
    // made hot is already watched above.
    release_cold(app, feeds);
    match draft {
        Err(e) => app.note(format!("draft of {} unread: {e}", app.label_for(id))),
        // The background round has this one and the view lands when it does.
        // The transcript draws a placeholder meanwhile
        // (`render::transcript_window`); the status row says why.
        Ok(_) if feeds.hydrating.contains(&id) => app.note(hydrating_notice(app, id)),
        // The transcript is the new context's own from the next frame, so
        // the switch has nothing to say about what is on screen.
        Ok(_) => app.clear_notice(),
    }
}

/// What the status row says while the context on screen is still being
/// hydrated. One spelling, so the notice can be taken back down again by
/// exactly the line that put it up.
pub(super) fn hydrating_notice(app: &App, id: ContextId) -> String {
    format!("hydrating {}…", app.label_for(id))
}

/// Mirror what the vi engine did to the draft onto the context's draft
/// block, one `edit_input` per op, queued on the draft outbox. The key path
/// does not wait for the kernel; the acks land through [`land_draft_edit`].
pub(super) fn mirror_ops(app: &mut App, ctx: ContextId, ops: &[kaijutsu_editor::EditOp]) {
    for op in ops {
        let generation = app.compose.sent();
        if let Err(e) = app.drafts.edit(
            ctx,
            generation,
            DraftEdit { offset: op.offset as u64, insert: op.insert.clone(), delete: op.delete as u64 },
        ) {
            app.compose.landed(generation, None);
            app.note(draft_edit_failure(&e));
        }
    }
}

/// One of our draft edits came back from the kernel ([`kaijutsu_client::draft_outbox`]).
/// Returns whether the screen changed.
pub(super) fn land_draft_edit(app: &mut App, landed: Landed) -> bool {
    match landed.result {
        Ok(version) => app.compose.landed(landed.generation, Some(version)),
        // The draft is the kernel's copy; a failed edit means the two have
        // diverged, and saying so beats typing into a line that is no
        // longer going anywhere.
        Err(e) => {
            app.compose.landed(landed.generation, None);
            app.note(draft_edit_failure(&e));
            true
        }
    }
}

fn draft_edit_failure(error: &dyn std::fmt::Display) -> String {
    format!("draft edit failed: {error:#}")
}

/// Dispatch one submitted `:` line (`docs/tui.md`, "The `:` line"). The tui
/// parses it itself (`cmdline::parse`) — the core's own `:w`/`:q` ex-command
/// dialect answers the alternate-screen editor, not this bar.
pub(super) async fn handle_colon_line(bridge: &KernelBridge, app: &mut App, ctx: ContextId, line: String, feeds: &mut Feeds) {
    match cmdline::parse(&line) {
        ColonVerb::Kj(argv) => match bridge.execute_kj(ctx, argv).await {
            Ok(result) if result.latch.is_some() => {
                app.note(result.latch.map(|l| l.message).unwrap_or_default());
            }
            Ok(result) => {
                // Any kj verb may have changed the roster (fork, promote,
                // archive); the picker and the rank should not wait a tick.
                app.roster_changed = true;
                let said = result.stdout.lines().next().unwrap_or("done").to_string();
                match follow_target(result.data.as_ref(), ctx) {
                    Ok(Some(id)) => {
                        switch_seat(bridge, app, id, feeds).await;
                        // A failed switch left its own notice; keep it.
                        if app.current == Some(id) {
                            app.note(said);
                        }
                    }
                    Ok(None) => app.note(said),
                    Err(e) => app.note(format!("{said}; cannot follow: {e}")),
                }
            }
            Err(e) => app.note(format!(":kj failed: {e:#}")),
        },
        ColonVerb::Shell(statement) => match bridge.shell_execute(ctx, &statement).await {
            Ok(_) => app.clear_notice(),
            Err(e) => app.note(format!("shell failed: {e}")),
        },
        ColonVerb::Quit { force } => {
            if force || !app.any_turn_running() {
                app.quit = true;
            } else {
                app.note("a turn this client started is still running — :q! quits anyway");
            }
        }
        ColonVerb::Unknown => app.note(format!("not a tui command: {line}")),
    }
}

/// `Ctrl+C`: the escalation ladder (`docs/tui.md`, "Ctrl+C reclaimed"). The
/// call is fire-and-forget the way the app's own `handle_interrupt` treats
/// it — the notice already says what was asked for, and a failed RPC here
/// would just repeat what `interrupt_context`'s own `Err` already logs.
pub(super) async fn interrupt_ctrl_c(bridge: &KernelBridge, app: &mut App, ladder: &mut interrupt::Ladder) {
    let Some(ctx) = app.current else {
        app.note("no context attached");
        return;
    };
    let running = app.turn_running(ctx);
    match ladder.press(std::time::Instant::now(), running) {
        InterruptStep::Nothing => app.note("nothing to interrupt — :q quits"),
        InterruptStep::Soft => {
            let _ = bridge.interrupt_context(ctx, false).await;
            app.note("interrupting after this tool call — Ctrl+C again to abort");
        }
        InterruptStep::Hard => {
            let _ = bridge.interrupt_context(ctx, true).await;
            app.note("aborted");
        }
        InterruptStep::HardAndClear => {
            let _ = bridge.interrupt_context(ctx, true).await;
            clear_draft(app, ctx);
            app.note("aborted, draft cleared");
        }
    }
}

/// The 3rd `Ctrl+C` press: clear the draft both locally and on the kernel's
/// copy — one edit deletes the whole text, queued behind the keystrokes
/// that wrote it, then `reset` puts compose back in its fresh-draft shape
/// (`docs/tui.md`, "Ctrl+C reclaimed").
fn clear_draft(app: &mut App, ctx: ContextId) {
    let len = app.compose.text().chars().count();
    if len > 0 {
        mirror_ops(app, ctx, &[kaijutsu_editor::EditOp { offset: 0, insert: String::new(), delete: len }]);
    }
    app.compose.reset();
}

/// `Tab` while `:kj ` is being typed in the bar: recompute candidates for
/// the current text, cycling the selection when it's the same prefix as
/// last time (repeated `Tab` walks the list, the way a shell's does), and
/// rewrite the bar to the selected candidate. Purely local — the bar is not
/// a kernel-mirrored block, unlike the compose draft, so no RPC is needed.
pub(super) fn apply_kj_completion(app: &mut App) {
    let Some(typed) = app.compose.kj_typed() else {
        return;
    };
    let fresh = completion::complete(&typed, &app.kj_catalog);
    app.completion = match (app.completion.take(), fresh) {
        (Some(mut prev), Some(next)) if prev.prefix == next.prefix => {
            prev.cycle();
            Some(prev)
        }
        (_, next) => next,
    };
    let Some(candidate) = app.completion.as_ref().and_then(|c| c.current()) else {
        return;
    };
    let takes_input = !app
        .kj_catalog
        .iter()
        .find(|k| k.name == candidate.name)
        .is_some_and(|k| k.input_hint.is_empty());
    let body = completion::accept(&candidate.name, !takes_input);
    app.compose.set_command_body(&body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy;
    use crate::run::feeds::tests::watched;

    /// A latched `Ctrl+A q` is confirmed only by the same chord on the same
    /// context inside the window (`docs/input.md`, "Placement keys").
    #[test]
    fn ctrl_a_q_confirms_only_its_own_latch_inside_the_window() {
        let (here, there) = (ContextId::new(), ContextId::new());
        let at = Instant::now();
        let armed = Some((here, at));
        assert!(close_confirms(armed, here, at + Duration::from_secs(5)));
        assert!(!close_confirms(armed, here, at + CLOSE_CONFIRM_WINDOW));
        assert!(!close_confirms(armed, there, at + Duration::from_secs(1)));
        assert!(!close_confirms(None, here, at));
    }

    /// An edit the writer lost when it stopped lands as a failure the player
    /// sees, and it settles the compose count of edits in flight.
    #[test]
    fn an_edit_lost_with_the_writer_is_noted_on_the_status_row() {
        let mut app = App::new("amy");
        let generation = app.compose.sent();
        assert_eq!(app.compose.edits_in_flight(), 1);
        let landed = Landed {
            context_id: ContextId::new(),
            generation,
            result: Err("the draft writer stopped before this edit was acknowledged".into()),
        };
        assert!(land_draft_edit(&mut app, landed));
        assert_eq!(app.compose.edits_in_flight(), 0, "the lost edit was counted as landed");
        let note = app.notice().unwrap_or_default().to_string();
        assert!(note.contains("draft edit failed") && note.contains("writer stopped"), "{note}");
    }

    #[test]
    fn a_draft_edit_failure_names_the_underlying_refusal() {
        let error = anyhow::anyhow!("context has no binding: edit_input is denied").context("edit input doc");
        let note = draft_edit_failure(&error);
        assert!(note.contains("edit input doc"));
        assert!(note.contains("context has no binding: edit_input is denied"), "{note}");
    }

    /// The place the reader scrolled to belongs to the context, not to the
    /// screen: switching away and back finds it where it was, and a context
    /// at its tail is still at its tail (`docs/tui.md`, "The buffer").
    ///
    /// The page height fingerprints the view, so this reads the same
    /// `Scrolled` back rather than merely some scrolled state.
    #[test]
    fn switching_seats_keeps_each_contexts_own_place() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let mut app = App::new("amy");
        watched(&mut app, a);
        watched(&mut app, b);
        app.switch_to(a);
        assert!(app.set_scrolled(copy::Scrolled::entering(7)), "a is watched");

        app.switch_to(b);
        assert!(app.following(), "b opens on its own live tail");

        app.switch_to(a);
        assert_eq!(
            app.scrolled().map(copy::Scrolled::page),
            Some(7),
            "a came back to the place the reader left it"
        );

        app.switch_to(b);
        assert!(app.following(), "b never left its tail");
    }

    /// A released context's place goes with it: the next visit hydrates and
    /// opens on the live tail, which is what a cold context should do.
    #[test]
    fn releasing_a_context_takes_its_scrolled_place_with_it() {
        let (a, b) = (ContextId::new(), ContextId::new());
        let mut app = App::new("amy");
        watched(&mut app, a);
        watched(&mut app, b);
        app.switch_to(a);
        assert!(app.set_scrolled(copy::Scrolled::entering(7)), "a is watched");

        app.switch_to(b);
        assert!(app.cold_contexts().is_empty(), "a is the previous context, and hot");
        app.switch_to(ContextId::new());
        let cold = app.cold_contexts();
        assert_eq!(cold, vec![a], "a left both the screen and the ring");
        for id in cold {
            app.release(id);
        }

        watched(&mut app, a);
        app.switch_to(a);
        assert!(app.following(), "a came back cold, on its tail");
    }
}

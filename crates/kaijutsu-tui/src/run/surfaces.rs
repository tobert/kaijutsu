//! The full-screen surfaces: the editor peer and its pushes, and the diff
//! viewer.

use anyhow::{Context, Result};
use kaijutsu_client::ServerEvent;
use kaijutsu_types::ContextId;
use tokio::sync::mpsc;

use crate::app::App;
use crate::bridge::KernelBridge;
use kaijutsu_client::editor_outbox;
use crate::render;

use kaijutsu_client::{PeerConfig, PeerInvocation};

use crate::editor::{self, EditorOpen, ScreenMode};

// ────────────────────────────────────────────────────────────────────────────
// The full-screen surfaces: editor and diff (docs/tui.md, "Editor and diff")
// ────────────────────────────────────────────────────────────────────────────

/// The nick this client attaches to the kernel's peer registry under.
///
/// Not `kaijutsu-app`: that nick is the fallback the kernel signals when the
/// submitter principal owns no window, and taking it would put a terminal
/// between the app and a headless `vi`.
const PEER_NICK: &str = "kaijutsu-tui";

/// One answer from the editor outbox. A state is drawn on whichever screen
/// holds its session, on screen or parked; a lost session gives the
/// conversation back. Returns whether the screen changed.
pub(super) fn land_editor_answer(app: &mut App, landed: editor_outbox::Landed) -> bool {
    match landed.result {
        Ok(state) => editor::apply_state(app, state),
        Err(message) if editor::is_session_lost(&message) => {
            // A kernel restart: the sessions are in memory and the persisted
            // kernel id is unchanged, so the buffer on screen is dead and
            // typing echoes nothing. Give the conversation back instead of
            // freezing.
            editor::leave_on_session_lost(app, "editor session lost (kernel restarted?); reopen with vi")
        }
        Err(message) => {
            tracing::warn!(session = landed.session, sent = %landed.sent, error = %message, "editor call failed");
            // The alternate screen hides the log, so the status row says it.
            app.note(format!("editor call failed: {message}"));
            true
        }
    }
}

/// Attach as a peer so `vi <path>` and `kj vi` reach this terminal.
///
/// This is the **only** notification that an editor session opened. The
/// `subscribeEditor` push channel carries state changes and closes; the kernel
/// publishes neither at open (`Kernel::editor_open_as`), so a client watching
/// that stream alone never learns a session exists. `signal_open_editor` fans
/// the `open_editor` invoke out to the submitter principal's attached peers,
/// which is why this attaches rather than polling.
///
/// The fan-out is by principal, so a `vi` run by a *model's* principal in a
/// sibling context does not reach here — it reaches the `kaijutsu-app`
/// fallback instead. Exact-window targeting is `docs/vi.md`'s open item.
pub(super) async fn attach_editor_peer(bridge: &KernelBridge, tx: mpsc::Sender<EditorOpen>) -> Result<()> {
    let (invocation_tx, invocation_rx) = std::sync::mpsc::channel::<PeerInvocation>();
    bridge
        .actor()
        .attach_peer(
            PeerConfig {
                // Per-process, so two terminals for one user coexist in the
                // registry instead of evicting each other.
                nick: PEER_NICK.to_string(),
                instance: uuid::Uuid::new_v4().to_string(),
            },
            invocation_tx,
        )
        .await
        .context("attach as a peer for open_editor signals")?;
    // A plain thread, not a task: `PeerInvocation` arrives on a std channel and
    // `blocking_send` would panic on a runtime thread.
    std::thread::spawn(move || serve_invocations(invocation_rx, tx));
    Ok(())
}

/// Answer peer invocations until the kernel drops the channel.
///
/// The reply is sent as soon as the signal is decoded and forwarded, which is
/// what it claims: the signal was received. The kernel bounds its wait on this
/// reply, so it must never wait for a frame.
fn serve_invocations(rx: std::sync::mpsc::Receiver<PeerInvocation>, tx: mpsc::Sender<EditorOpen>) {
    while let Ok(invocation) = rx.recv() {
        let answer = match invocation.action.as_str() {
            "open_editor" => match editor::parse_open_signal(&invocation.params) {
                Ok(open) => {
                    let echo = serde_json::json!({
                        "session": open.state.session,
                        "path": open.state.path,
                    });
                    if tx.blocking_send(open).is_err() {
                        return;
                    }
                    serde_json::to_vec(&echo).map_err(|e| format!("serialize: {e}"))
                }
                Err(e) => Err(e),
            },
            other => Err(format!("kaijutsu-tui serves open_editor, not {other}")),
        };
        let _ = invocation.reply.send(answer);
    }
}

/// Apply an editor push to the screen. Returns whether the frame changed.
///
/// `EditorClosed` is what `:q`/`ZZ`/`ZQ` produce — the kernel alone knows the
/// mode, so it alone decides a quit — and it is what gives the conversation
/// back. A `Reconnected` probes the session with an empty key batch, because a
/// kernel restart leaves the id unknown while the connection looks ordinary.
pub(super) async fn observe_editor_event(bridge: &KernelBridge, app: &mut App, event: &ServerEvent) -> bool {
    if editor::apply_push(app, event) {
        return true;
    }
    match event {
        ServerEvent::Reconnected => {
            let Some(session) = app.screen.editor().map(|s| s.session) else {
                return false;
            };
            // An empty batch applies no keys and mutates nothing; the only
            // thing it can report is whether the session still exists.
            match bridge.actor().editor_keys(session, "").await {
                Ok(_) => false,
                Err(e) if editor::is_session_lost(&e.to_string()) => editor::leave_on_session_lost(
                    app,
                    "editor session lost across the reconnect; reopen with vi",
                ),
                Err(e) => {
                    tracing::warn!(session, error = %e, "editor liveness probe failed");
                    false
                }
            }
        }
        _ => false,
    }
}

/// `Ctrl+A v` — open the diff viewer, or say why it did not.
pub(super) fn act_open_diff(app: &mut App, width: u16) {
    match open_diff(app, width) {
        Some(screen) => {
            app.screen = ScreenMode::Diff(screen);
            app.clear_notice();
        }
        None if app.following() => app.note("no diff block in this context"),
        None => app.note("no diff block at or above this line"),
    }
}

/// The diff at the reader's block (`render::reader_target`), frozen into a
/// screen: the nearest diff block at or above the reader's line while
/// scrolled, the newest on the live tail. The scrolled view is left as it
/// is, so closing the viewer returns the reader to the line they left.
///
/// Nothing on the wire opens a diff view: `kj diff` authors a
/// `ContentType::Diff` block and the client decides. The rule is the app's
/// [`crate::diff::openable_diff`], so the two clients never disagree about
/// what is a diff.
fn open_diff(app: &mut App, width: u16) -> Option<crate::diff::DiffScreen> {
    let target = render::reader_target(app, width, |_, b| {
        crate::diff::openable_diff(b.content_type, &b.content).is_some()
    })?;
    let block = app.current_view()?.mirror.block(&target)?;
    let how = crate::diff::openable_diff(block.content_type, &block.content)?;
    let title = format!("block #{}", block.id.seq);
    Some(match crate::diff::parse_open(&how, &block.content) {
        Ok(model) => crate::diff::DiffScreen::new(title, &model, &app.palette),
        // A declared diff that will not parse is a visible error, never an
        // empty viewer (`kaijutsu-diff`'s viewer contract).
        Err(e) => {
            crate::diff::DiffScreen::unparsed(title, &e.to_string(), &block.content, &app.palette)
        }
    })
}

/// `--diff <a> [b]` — run `kj diff` in the starting context and freeze its
/// output into the diff screen.
///
/// The kernel's verb is the one source: `kj diff` resolves both sides through
/// `resolve_editor_target`, the same function the vi editor binds with, so the
/// TUI never has its own idea of what document a path names.
pub(super) async fn open_kj_diff(
    bridge: &KernelBridge,
    app: &mut App,
    context_id: ContextId,
    args: &[String],
) -> Result<()> {
    let mut argv = vec!["diff".to_string()];
    argv.extend(args.iter().cloned());
    let title = format!("kj {}", argv.join(" "));
    let result = bridge.execute_kj(context_id, argv).await?;
    if result.exit_code != 0 {
        anyhow::bail!(
            "{title} exited {}: {}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    let how = crate::diff::OpenAs::Declared;
    app.screen = ScreenMode::Diff(match crate::diff::parse_open(&how, &result.stdout) {
        Ok(model) => crate::diff::DiffScreen::new(title, &model, &app.palette),
        Err(e) => {
            crate::diff::DiffScreen::unparsed(title, &e.to_string(), &result.stdout, &app.palette)
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ContextView;
    use crate::copy;
    use crate::run::feeds::tests::watched;
    use kaijutsu_client::ContextMirror;
    use kaijutsu_types::{BlockId, PrincipalId};

    /// An editor call that failed for any reason other than a lost session
    /// shows on the status row, since the alternate screen hides the log.
    #[test]
    fn an_editor_call_lost_with_the_writer_is_noted_on_the_status_row() {
        let mut app = App::new("amy");
        let landed = editor_outbox::Landed {
            session: 4,
            sent: "ab".into(),
            result: Err("the editor writer stopped before this call was answered".into()),
        };
        assert!(land_editor_answer(&mut app, landed));
        let note = app.notice().unwrap_or_default().to_string();
        assert!(note.contains("editor call failed") && note.contains("writer stopped"), "{note}");
    }

    /// Two diff blocks with prose between and after; the context on screen.
    fn two_diffs() -> (App, Vec<kaijutsu_types::BlockSnapshot>) {
        use kaijutsu_types::{BlockKind, BlockSnapshotBuilder, ContentType, Role, Status};
        let id = ContextId::new();
        let mk = |seq: u64, kind: BlockKind, text: &str| {
            BlockSnapshotBuilder::new(BlockId::new(id, PrincipalId::new(), seq), kind)
                .role(Role::Model)
                .status(Status::Done)
                .content(text)
                .build()
        };
        let diff = |seq: u64, file: &str| {
            let text = format!("--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-old\n+new\n");
            let mut b = mk(seq, BlockKind::Text, &text);
            b.content_type = ContentType::Diff;
            b
        };
        let long = (0..30).map(|n| format!("prose {n}")).collect::<Vec<_>>().join("\n");
        let blocks = vec![
            mk(1, BlockKind::Text, "first"),
            diff(2, "one.rs"),
            mk(3, BlockKind::Text, &long),
            diff(4, "two.rs"),
            mk(5, BlockKind::Text, &long),
        ];
        let mut app = App::new("amy");
        app.screen_rows = 12;
        let mut mirror = ContextMirror::new(id);
        mirror.apply_snapshot(blocks.clone(), 1).expect("snapshot applies");
        app.views.insert(id, ContextView::new(mirror));
        app.switch_to(id);
        (app, blocks)
    }

    fn diff_title(app: &App) -> Option<String> {
        match &app.screen {
            ScreenMode::Diff(screen) => Some(screen.title.clone()),
            _ => None,
        }
    }

    /// Scrolled, `Ctrl+A v` opens the diff at or above the reader's line,
    /// not the newest; on the tail it opens the newest; above every diff it
    /// says so and changes no screen.
    #[test]
    fn the_diff_viewer_opens_on_the_diff_at_or_above_the_reader() {
        let (mut app, blocks) = two_diffs();
        act_open_diff(&mut app, 80);
        assert_eq!(diff_title(&app), Some(format!("block #{}", blocks[3].id.seq)), "the tail: the newest diff");
        app.screen = ScreenMode::Conversation;

        let rows = render::transcript_all_lines(&mut app, 80)
            .iter()
            .map(copy::line_text)
            .collect::<Vec<_>>();
        let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle)).expect(needle);
        let mut scrolled = copy::Scrolled::entering(6);
        scrolled.jump_to(row_of("prose 3"));
        assert!(app.set_scrolled(scrolled));
        act_open_diff(&mut app, 80);
        assert_eq!(diff_title(&app), Some(format!("block #{}", blocks[1].id.seq)), "the diff above the reader, not the newest");
        let place = app.scrolled().map(|s| s.reader_block());
        assert_eq!(place, Some(Some(blocks[2].id)), "opening the viewer left the reader where they were");
        app.screen = ScreenMode::Conversation;
        assert_eq!(render::reader_block(&mut app, 80), Some(blocks[2].id), "and closing it did too");

        app.scrolled_mut().expect("scrolled").jump_to(row_of("first"));
        act_open_diff(&mut app, 80);
        assert!(matches!(app.screen, ScreenMode::Conversation), "nothing opened");
        assert_eq!(app.notice(), Some("no diff block at or above this line"));
    }

    /// A context with no diff block says so on the tail.
    #[test]
    fn the_diff_viewer_on_a_tail_with_no_diff_says_so() {
        let mut app = App::new("amy");
        let id = ContextId::new();
        watched(&mut app, id);
        app.switch_to(id);
        act_open_diff(&mut app, 80);
        assert!(matches!(app.screen, ScreenMode::Conversation));
        assert_eq!(app.notice(), Some("no diff block in this context"));
    }
}

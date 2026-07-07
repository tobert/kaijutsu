//! e2e: the in-app editor **wire surface** — capnp `editorOpen`/`editorKeys`/
//! `editorState`/`editorSave`/`editorQuit` + the `subscribeEditor` push channel.
//!
//! Drives a real SSH + Cap'n Proto round-trip end to end, no GUI — vi.md slice-2
//! step 1a. The kernel-local editor semantics are already covered headless
//! (`kaijutsu-kernel`); this proves the same surface survives the wire AND that
//! a state change reaches a subscriber over the push channel (the reason the
//! editor channel is push, not poll). The concurrent remote-merge push is step
//! 1b (needs `EditorCore::apply_remote_ops`) and is tested there.

mod common;

use std::time::Duration;

use tokio::sync::broadcast::Receiver;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{EditorState, PeerConfig, ServerEvent, editor_events_channel};

/// A script the server seeds into the rc CRDT on a fresh kernel — guaranteed to
/// exist, so `editorOpen` binds to a real config-owned block.
const RC_PATH: &str = "/etc/rc/coder/create/S00-stance.kai";

/// Drain the editor push channel until a `EditorStateChanged` arrives (or fail
/// loud on timeout — a missing push is the bug this test exists to catch).
async fn recv_state(rx: &mut Receiver<ServerEvent>) -> EditorState {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(ServerEvent::EditorStateChanged { state })) => return state,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("editor push channel error: {e}"),
            Err(_) => panic!("timed out waiting for an EditorStateChanged push"),
        }
    }
}

/// Drain the push channel until a `EditorStateChanged` for a *specific* session
/// arrives (other sessions' pushes are stepped over).
async fn recv_state_for(rx: &mut Receiver<ServerEvent>, session: u64) -> EditorState {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(ServerEvent::EditorStateChanged { state })) if state.session == session => {
                return state;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("editor push channel error: {e}"),
            Err(_) => panic!("timed out waiting for session {session}'s EditorStateChanged"),
        }
    }
}

/// Drain until an `EditorClosed` arrives.
async fn recv_closed(rx: &mut Receiver<ServerEvent>) -> u64 {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Ok(ServerEvent::EditorClosed { session_id })) => return session_id,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("editor push channel error: {e}"),
            Err(_) => panic!("timed out waiting for an EditorClosed push"),
        }
    }
}

#[test]
fn editor_open_keys_state_push_and_rollback_over_the_wire() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        // Subscribe to the push channel BEFORE editing, so we can't miss the event.
        let (callback, mut rx) = editor_events_channel(64);
        kernel.subscribe_editor(callback).await.unwrap();

        // Open a kernel-seeded rc script over the wire. A freshly opened block
        // is clean and binds to its config-owned CRDT block.
        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        assert!(!opened.dirty, "a freshly opened block must be clean");
        let session = opened.session;
        let original = opened.text.clone();
        assert!(!original.is_empty(), "the seeded rc script has content");

        // Feed keys: insert 'Z' at the start. The keys() RPC returns the new
        // state synchronously — the app's own edits never need the push.
        let after = kernel.editor_keys(session, "iZ<Esc>").await.unwrap();
        assert_eq!(after.session, session);
        assert_eq!(after.text, format!("Z{original}"));
        assert!(after.dirty, "buffer diverged from the open checkpoint");

        // The SAME state must independently arrive on the push channel — this is
        // the wire half that a second renderer (another window) would rely on.
        let pushed = recv_state(&mut rx).await;
        assert_eq!(pushed.session, session);
        assert_eq!(
            pushed.text, after.text,
            "pushed state must match the keys() return"
        );
        assert!(pushed.dirty);

        // editorState reads back the same buffer.
        let polled = kernel.editor_state(session).await.unwrap();
        assert_eq!(polled.text, after.text, "editorState matches after keys");

        // Quit: ZQ rolls the block back to the open checkpoint and pushes Closed.
        kernel.editor_quit(session).await.unwrap();
        assert_eq!(recv_closed(&mut rx).await, session, "quit pushes Closed");

        // Re-open to prove the rollback restored the original text faithfully
        // (mirror onto / off the CRDT block is lossless over the wire).
        let reopened = kernel.editor_open(RC_PATH).await.unwrap();
        assert_eq!(
            reopened.text, original,
            "ZQ rollback restored the block byte-for-byte"
        );
        assert!(!reopened.dirty, "the rolled-back block re-opens clean");
        kernel.editor_quit(reopened.session).await.unwrap();
    });
}

#[test]
fn editor_save_clears_dirty_and_pushes_over_the_wire() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let (callback, mut rx) = editor_events_channel(64);
        kernel.subscribe_editor(callback).await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;

        let edited = kernel.editor_keys(session, "iQ<Esc>").await.unwrap();
        assert!(edited.dirty);
        let _ = recv_state(&mut rx).await; // the keys push

        // ZZ checkpoints the buffer: dirty flips false, and a push reflects it.
        let saved = kernel.editor_save(session).await.unwrap();
        assert!(!saved.dirty, "save must clear dirty");
        let pushed = recv_state(&mut rx).await;
        assert!(!pushed.dirty, "the save push carries the now-clean state");
        assert_eq!(pushed.text, saved.text);

        // The server is ephemeral per test, so no restoration is needed; just
        // close the session cleanly.
        kernel.editor_quit(session).await.unwrap();
    });
}

#[test]
fn a_peer_edit_reconciles_and_pushes_merged_state_to_a_sibling_session() {
    // The remote-merge half of the push channel (vi.md step 1b): two editor
    // sessions bound to the SAME block. When A writes, the server's editor
    // reconciler must merge A's edit into B's stale buffer and push B's new
    // state — even though B made no edit. This is the reason the channel is
    // push, not poll.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let (callback, mut rx) = editor_events_channel(64);
        kernel.subscribe_editor(callback).await.unwrap();

        // Two sessions on the same rc path → both bind to the same owning block.
        let a = kernel.editor_open(RC_PATH).await.unwrap();
        let b = kernel.editor_open(RC_PATH).await.unwrap();
        assert_ne!(a.session, b.session, "distinct session handles");
        let original = b.text.clone();
        assert!(!b.dirty, "B opens clean");

        // Session A inserts 'Z' at the start; the edit mirrors onto the shared
        // CRDT block and emits a block.text_ops the reconciler observes.
        let a_after = kernel.editor_keys(a.session, "iZ<Esc>").await.unwrap();
        assert_eq!(a_after.text, format!("Z{original}"));

        // The reconciler pushes the merged state to B (the sibling that did NOT
        // edit). B's buffer was stale; now it reflects A's edit.
        let b_pushed = recv_state_for(&mut rx, b.session).await;
        assert_eq!(
            b_pushed.text, a_after.text,
            "B must see A's edit merged into its buffer"
        );
        assert!(b_pushed.dirty, "B's buffer now differs from the checkpoint it opened on");

        kernel.editor_quit(a.session).await.unwrap();
        kernel.editor_quit(b.session).await.unwrap();
    });
}

#[test]
fn colon_commands_save_and_close_over_the_wire() {
    // Slice 3: the `:`-line dialect rides the existing `editorKeys` surface. A
    // `:wq` keystroke stream must save the edit and close the session (pushing
    // Closed), and the in-progress `:`-line must surface on `command_line` so a
    // renderer can draw the strip without tracking mode.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let (callback, mut rx) = editor_events_channel(64);
        kernel.subscribe_editor(callback).await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;
        let original = opened.text.clone();

        // Edit, then type the `:`-line partially: the command line surfaces on
        // the returned state (the strip a renderer draws).
        kernel.editor_keys(session, "iZ<Esc>").await.unwrap();
        let typing = kernel.editor_keys(session, ":w").await.unwrap();
        assert_eq!(
            typing.command_line.as_deref(),
            Some(":w"),
            "the in-progress :-line rides the state over the wire"
        );

        // Complete `:wq<CR>`: saves the edit and closes the session.
        let _ = recv_state(&mut rx).await; // drain prior pushes opportunistically
        kernel.editor_keys(session, "q<CR>").await.unwrap();
        assert_eq!(recv_closed(&mut rx).await, session, ":wq pushes Closed");

        // Re-open: `:wq` kept the inserted text (save+quit), unlike ZQ.
        let reopened = kernel.editor_open(RC_PATH).await.unwrap();
        assert_eq!(
            reopened.text,
            format!("Z{original}"),
            ":wq persisted the edit on the block"
        );
        assert!(reopened.command_line.is_none(), "a fresh open has no :-line");
        // Ephemeral server per test; just close the session cleanly (clean → :q).
        kernel.editor_keys(reopened.session, ":q<CR>").await.unwrap();
    });
}

#[test]
fn colon_substitute_edits_the_block_over_the_wire() {
    // Slice 3 step 2: `:s` is an edit that rides the existing `editorKeys` wire
    // (keystrokes → EditOps → CRDT). Open a clean block, prepend a known marker
    // so we have something deterministic to substitute, run `:s`, and read the
    // result back over the wire.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;
        let original = opened.text.clone();

        // Insert "AAA " at the start so the buffer begins with a known token.
        let primed = kernel.editor_keys(session, "iAAA <Esc>").await.unwrap();
        assert_eq!(primed.text, format!("AAA {original}"));

        // Substitute the marker (first match on the current line).
        let after = kernel.editor_keys(session, ":s/AAA/ZZZ/<CR>").await.unwrap();
        assert!(after.text.starts_with("ZZZ "), "':s' replaced the marker: {:?}", &after.text[..8.min(after.text.len())]);
        assert_eq!(after.command_line, None, "the bar closed after submit");

        // editorState confirms the block holds the substitution over the wire.
        let polled = kernel.editor_state(session).await.unwrap();
        assert_eq!(polled.text, after.text, "block matches after :s");

        // Discard so the seeded rc isn't left mutated (ephemeral server anyway).
        kernel.editor_keys(session, ":q!<CR>").await.unwrap();
    });
}

#[test]
fn unknown_colon_command_reports_on_the_status_line_over_the_wire() {
    // Slice 3 polish: an unknown `:command` must NOT error the `editorKeys` call
    // (which the renderer would surface as a hard failure / popped editor) — it
    // returns a normal state carrying the vim-E492 message, session still open.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;

        let after = kernel
            .editor_keys(session, ":frobnicate<CR>")
            .await
            .expect("a bad command does not error the RPC");
        let msg = after.message.expect("a status message rode the wire");
        assert!(msg.contains("Not an editor command"), "got: {msg}");
        assert_eq!(after.text, opened.text, "the buffer is untouched");

        // The session is still alive: editorState answers and the message has
        // cleared (it's transient, set only on the offending submit's push).
        let polled = kernel.editor_state(session).await.unwrap();
        assert_eq!(polled.message, None, "the status message is transient");

        kernel.editor_keys(session, ":q!<CR>").await.unwrap();
    });
}

#[test]
fn dirty_quit_refusal_reports_on_the_status_line_over_the_wire() {
    // The error-channel unification (2026-07-07): `:q` on a dirty buffer refuses
    // on the STATUS LINE (vim E37), not as an RPC error — a hard error never
    // reached the GUI, and with no state push the `:`-strip kept the stale `:q`.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;

        kernel.editor_keys(session, "iZ<Esc>").await.unwrap(); // dirty
        let after = kernel
            .editor_keys(session, ":q<CR>")
            .await
            .expect("a dirty :q does not error the RPC");
        let msg = after.message.expect("the refusal rode the state");
        assert!(msg.contains("No write since last change"), "got: {msg}");
        assert_eq!(after.command_line, None, "the stale :q line is gone");

        // Session alive; `:q!` discards (rolls back — sole session) and closes.
        kernel
            .editor_keys(session, ":q!<CR>")
            .await
            .expect(":q! closes the refused session");
    });
}

#[test]
fn colon_r_reads_a_file_into_the_buffer_over_the_wire() {
    // Slice 3: `:r <file>` fetches content (VFS) inside the now-async editor_keys
    // and splices it at the cursor. Read the editor's own seeded file into itself
    // at the top — the buffer grows and still contains the original content.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let opened = kernel.editor_open(RC_PATH).await.unwrap();
        let session = opened.session;
        let original = opened.text.clone();
        assert!(!original.is_empty());

        // Cursor opens at the top; `:r` splices the file's content before it.
        let after = kernel
            .editor_keys(session, &format!(":r {RC_PATH}<CR>"))
            .await
            .unwrap();
        assert!(
            after.text.len() > original.len(),
            "`:r` grew the buffer with the file's content"
        );
        assert!(after.text.contains(original.trim()), "the original content survives");
        assert!(after.dirty, "`:r` dirties the buffer");
        assert_eq!(after.command_line, None, "the bar closed after submit");

        // Discard so the seeded rc isn't left mutated (ephemeral server anyway).
        kernel.editor_keys(session, ":q!<CR>").await.unwrap();
    });
}

#[test]
fn vi_over_the_shell_signals_the_app_peer_to_open_a_renderer() {
    // The `open_editor` peer signal (vi.md step 2): a human's `vi <path>` in the
    // app shell must nudge the submitter's app windows to pop a renderer. We
    // attach as the well-known app peer, run `vi` over the same connection's
    // shell, and assert the peer receives an `open_editor` invocation carrying
    // the session + path — submitter-aware fan-out reaching our window.
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        // The shell needs an active context to materialize `vi`.
        let ctx = kernel.create_context("editor-signal").await.unwrap();
        kernel.join_context(ctx, "app-instance").await.unwrap();

        // Attach as the app peer; a worker thread captures invocations onto a
        // channel the async test can await, and replies so the kernel's
        // best-effort signal completes cleanly.
        let (inv_tx, inv_rx) = std::sync::mpsc::channel::<kaijutsu_client::PeerInvocation>();
        let (cap_tx, mut cap_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
        std::thread::spawn(move || {
            while let Ok(inv) = inv_rx.recv() {
                let _ = cap_tx.send((inv.action.clone(), inv.params.clone()));
                let _ = inv.reply.send(Ok(b"ok".to_vec()));
            }
        });
        kernel
            .attach_peer(
                &PeerConfig {
                    nick: "kaijutsu-app".to_string(),
                    ..Default::default()
                },
                inv_tx,
            )
            .await
            .expect("attach as app peer");

        // Run `vi` over the shell — the builtin opens a session and fires the
        // open_editor signal to the submitter's app windows (us).
        kernel
            .execute(&format!("vi {RC_PATH}"))
            .await
            .expect("execute vi");

        // The app peer receives open_editor with {session, path}.
        let (action, params) = tokio::time::timeout(Duration::from_secs(10), cap_rx.recv())
            .await
            .expect("timed out waiting for the open_editor signal")
            .expect("capture channel closed");
        assert_eq!(action, "open_editor", "the signal action");
        let v: serde_json::Value = serde_json::from_slice(&params).expect("params are JSON");
        assert_eq!(v["path"], RC_PATH, "signal carries the path");
        assert!(
            v["session"].as_u64().is_some(),
            "signal carries a numeric session id: {v}"
        );
    });
}

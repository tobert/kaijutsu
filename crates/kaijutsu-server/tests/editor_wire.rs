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
use kaijutsu_client::{EditorState, ServerEvent, editor_events_channel};

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

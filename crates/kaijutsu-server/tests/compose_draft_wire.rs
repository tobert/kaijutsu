//! e2e: the compose draft is a block, over a real SSH + Cap'n Proto round trip.
//!
//! The claim this file exists to pin is **zero-copy submit**. The old
//! `submit_input` read the draft, cleared it, and only *then* tried to author a
//! block — so a failure in between destroyed what someone had typed. When the
//! block you type into *is* the message you send, that interval cannot exist.
//! The assertion is therefore an identity one: the id that comes back from
//! submit is the id that held the draft. No injected fault is needed to prove a
//! window is gone if there is nothing to copy between.
//!
//! Nothing here decodes an operation. The draft is observed the same way any
//! other block is — through `getBlocks` and through the change feed — because
//! that is the point of making it a block.

mod common;

use std::time::Duration;

use common::{connect_client, run_local, start_server, start_server_with_mock_llm};
use kaijutsu_client::{
    ContextMirror, FeedEvent, KernelHandle, RpcClient, context_feed_channel,
};
use kaijutsu_crdt::ContextId;
use kaijutsu_types::{BlockKind, BlockQuery, BlockSnapshot, Status};

/// Every block in the context, in document order.
async fn blocks(kernel: &KernelHandle, context_id: ContextId) -> Vec<BlockSnapshot> {
    kernel
        .get_blocks(context_id, &BlockQuery::All)
        .await
        .expect("block query")
}

/// The caller's draft block, if the kernel is holding one.
///
/// Looked up by status rather than by a remembered id on purpose: a test that
/// tracked the id itself could not catch a submit that authored a *second*
/// block and left the draft behind.
async fn draft(kernel: &KernelHandle, context_id: ContextId) -> Option<BlockSnapshot> {
    blocks(kernel, context_id)
        .await
        .into_iter()
        .find(|b| b.status == Status::Draft)
}

async fn open_context(kernel: &KernelHandle, label: &str) -> ContextId {
    let context_id = kernel.create_context(label).await.unwrap();
    kernel.join_context(context_id, "draft-test").await.unwrap();
    context_id
}

async fn bind(client: &RpcClient) -> KernelHandle {
    client.bind_kernel().await.unwrap().0
}

/// Typing creates a draft block; it is `Draft`, `ephemeral`, and last.
#[test]
fn typing_creates_an_ephemeral_draft_block_at_the_end() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-create").await;

        kernel.edit_input(context_id, 0, "hello", 0).await.unwrap();

        let all = blocks(&kernel, context_id).await;
        let d = all.last().expect("a draft block exists");
        assert_eq!(d.status, Status::Draft, "the draft carries Status::Draft");
        assert!(d.ephemeral, "the draft is ephemeral");
        assert_eq!(d.kind, BlockKind::Text);
        assert_eq!(d.content, "hello");

        // And the read verb agrees with the block.
        let state = kernel.get_input_state(context_id).await.unwrap();
        assert_eq!(state.content, "hello");
    });
}

/// **The zero-copy claim.** Submit returns the id the draft already had.
#[test]
fn chat_submit_promotes_the_draft_rather_than_copying_it() {
    run_local(async {
        let addr = start_server_with_mock_llm().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit").await;

        kernel
            .edit_input(context_id, 0, "what did we ship?", 0)
            .await
            .unwrap();
        let before = draft(&kernel, context_id)
            .await
            .expect("draft exists before submit");

        let result = kernel.submit_input(context_id, false).await.unwrap();

        assert_eq!(
            result.block_id, before.id,
            "submit must promote the draft in place, not author a copy"
        );

        let all = blocks(&kernel, context_id).await;
        let submitted = all
            .iter()
            .find(|b| b.id == before.id)
            .expect("the draft's block survives submit");
        assert_eq!(submitted.status, Status::Done);
        assert!(
            !submitted.ephemeral,
            "a submitted message is no longer ephemeral"
        );
        assert_eq!(submitted.content, "what did we ship?");
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "no draft remains after submit"
        );
    });
}

/// A whitespace-only draft is refused **without being cleared** — a stray Enter
/// neither sends nothing nor destroys what is there.
#[test]
fn an_empty_draft_is_refused_and_survives() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-empty").await;

        kernel.edit_input(context_id, 0, "   ", 0).await.unwrap();
        kernel
            .submit_input(context_id, false)
            .await
            .expect_err("an empty draft is refused");

        let d = draft(&kernel, context_id)
            .await
            .expect("the refused draft is still there");
        assert_eq!(d.content, "   ", "a refused submit destroys nothing");
    });
}

/// Shell mode cannot promote — a shell command is a `ToolCall` block, not the
/// user's `Text`. So the draft is *consumed*, and the order is the assertion:
/// the command block must exist before the draft is cleared.
#[test]
fn shell_submit_authors_the_command_before_clearing_the_draft() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-shell").await;

        kernel
            .edit_input(context_id, 0, "echo hi", 0)
            .await
            .unwrap();
        let result = kernel.submit_input(context_id, true).await.unwrap();

        let all = blocks(&kernel, context_id).await;
        let command = all
            .iter()
            .find(|b| b.id == result.block_id)
            .expect("the command block exists");
        assert_eq!(
            command.kind,
            BlockKind::ToolCall,
            "shell submit authors a ToolCall, not the draft"
        );
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "the draft is consumed once the command block exists"
        );
    });
}

/// `clearInput` discards the caller's draft.
#[test]
fn clearing_removes_the_draft_block() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-clear").await;

        kernel.edit_input(context_id, 0, "nevermind", 0).await.unwrap();
        assert!(draft(&kernel, context_id).await.is_some());

        kernel.clear_input(context_id).await.unwrap();
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "clear removes the draft block itself"
        );
        // Clearing a draft that is not there is not an error.
        kernel.clear_input(context_id).await.unwrap();
    });
}

/// Edits are **character**-indexed, not byte-indexed.
///
/// This is the coordinate doctrine the old input document never proved: callers
/// counted characters while its `InputDocEntry` type (deleted 2026-08-16)
/// bounds-checked Rust byte lengths. Editing after an emoji is where those two
/// disagree.
#[test]
fn draft_edits_are_character_indexed_through_multibyte_text() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-utf8").await;

        // 5 characters, 13 bytes.
        kernel.edit_input(context_id, 0, "日本語 🎵", 0).await.unwrap();
        // Append at character 5 — a byte-indexed kernel would panic or misplace.
        kernel.edit_input(context_id, 5, " ok", 0).await.unwrap();
        // Delete the two characters "語 " starting at character 2.
        kernel.edit_input(context_id, 2, "", 2).await.unwrap();

        let state = kernel.get_input_state(context_id).await.unwrap();
        assert_eq!(state.content, "日本🎵 ok");
    });
}

/// A co-player sees you typing: the draft rides the change feed like any other
/// block. A single shared input document could never do this.
#[test]
fn the_draft_rides_the_change_feed() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-feed").await;

        // Subscribe FIRST, then snapshot — the mandated order.
        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();
        let (snapshot, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(context_id);
        mirror.apply_snapshot(snapshot, version).unwrap();

        kernel.edit_input(context_id, 0, "typing", 0).await.unwrap();
        kernel.edit_input(context_id, 6, " more", 0).await.unwrap();

        let target = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap()
            .1;
        while mirror.version() < target {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(FeedEvent::Changed(delivery))) => {
                    mirror.receive(delivery).expect("delivery applies cleanly")
                }
                Ok(Some(other)) => panic!("unexpected feed event: {other:?}"),
                Ok(None) => panic!("feed closed before version {target}"),
                Err(_) => panic!("timed out at {} waiting for {target}", mirror.version()),
            }
        }

        let seen = mirror
            .blocks()
            .iter()
            .find(|b| b.status == Status::Draft)
            .expect("the draft reached the mirror");
        assert_eq!(
            seen.content, "typing more",
            "a co-player follows the draft through the ordinary feed"
        );
    });
}

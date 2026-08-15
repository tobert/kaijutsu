//! e2e: the context change feed over a real SSH + Cap'n Proto round trip
//! (docs/change-feed.md).
//!
//! This is the test that says the migration works. Everything else in the lane
//! is a means to one end: a client can follow a context **without linking the
//! text engine**. So the assertions here are made through `ContextMirror`,
//! which decodes no operations and holds no CRDT — if its text matches the
//! kernel's, the wire carried enough.
//!
//! Mutations are driven through `kj` because that is what a client can
//! actually reach today: with `pushOps` deleted there is no client-facing
//! block-text RPC at all, and `kj block append` / `kj block edit` run the same
//! kernel mutators the LLM stream and the MCP tools do. That also makes these
//! tests cover the classification honestly — `kj block append` reaches
//! `append_text_as`, and `kj block edit replace` reaches `edit_text_as` with a
//! mid-text splice.

mod common;

use std::time::Duration;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{
    ContextChange, ContextMirror, FeedEvent, KernelHandle, context_feed_channel,
};
use kaijutsu_crdt::{ContextId, PrincipalId, Role};
use kaijutsu_types::{BlockId, BlockQuery};
use tokio::sync::mpsc::Receiver;

/// Drain deliveries into the mirror until it reaches `version`.
///
/// A timeout is a hard failure: a missing delivery is precisely the bug this
/// file exists to catch, and "it probably arrived" is the guess the feed was
/// built to replace.
async fn drain_until(mirror: &mut ContextMirror, rx: &mut Receiver<FeedEvent>, version: u64) {
    while mirror.version() < version {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(FeedEvent::Changed(delivery))) => {
                mirror.receive(delivery).expect("delivery applies cleanly")
            }
            Ok(Some(FeedEvent::Terminated { reason, .. })) => {
                panic!("feed terminated early: {reason:?}")
            }
            // These tests drive one connection and never drop it, so a
            // reconnect notice here would mean the transport went away
            // underneath the assertion — a failure, not something to absorb.
            Ok(Some(FeedEvent::Resubscribed)) => {
                panic!("feed re-subscribed: the connection dropped mid-test")
            }
            Ok(None) => panic!("feed channel closed before reaching version {version}"),
            Err(_) => panic!(
                "timed out at version {} waiting for {version}",
                mirror.version()
            ),
        }
    }
}

async fn author_empty_block(
    kernel: &KernelHandle,
    context_id: ContextId,
    principal: PrincipalId,
) -> BlockId {
    kernel
        .author_block(&kaijutsu_client::AuthorBlock::text(
            context_id,
            principal,
            Role::Model,
            "",
        ))
        .await
        .expect("author block")
}

/// Run a `kj` command, failing loudly on a non-zero exit — a silently failed
/// mutation would make the feed look correct for the wrong reason.
async fn kj(kernel: &KernelHandle, context_id: ContextId, argv: &[&str]) {
    let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
    let result = kernel
        .execute_kj(context_id, &argv)
        .await
        .expect("kj call reaches the kernel");
    assert_eq!(
        result.exit_code, 0,
        "kj {argv:?} failed: {} {}",
        result.stdout, result.stderr
    );
}

async fn kernel_version(kernel: &KernelHandle, context_id: ContextId) -> u64 {
    kernel
        .get_blocks_versioned(context_id, &BlockQuery::All)
        .await
        .expect("versioned query")
        .1
}

/// Streamed appends reassemble byte-for-byte through the feed, and a
/// non-append edit arrives as a whole-text replace. No CRDT on the client side.
#[test]
fn appends_and_an_edit_reproduce_the_kernel_text() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("feed-stream").await.unwrap();
        kernel.join_context(context_id, "feed-test").await.unwrap();
        let principal = PrincipalId::for_agent_session("feed-author");
        let block_id = author_empty_block(&kernel, context_id, principal).await;
        let key = block_id.to_key();

        // Subscribe FIRST, then snapshot — the mandated order.
        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();
        let (blocks, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(context_id);
        mirror.apply_snapshot(blocks, version).unwrap();

        // Multibyte on purpose: a suffix is characters, and a byte-length
        // confusion anywhere in the path shows up here rather than in
        // production.
        for chunk in ["Hello", ", ", "世界", "!", " 🎵"] {
            kj(&kernel, context_id, &["block", "append", &key, "--text", chunk]).await;
        }
        // A line replace — the non-append path.
        kj(
            &kernel,
            context_id,
            &[
                "block", "edit", &key, "replace", "--start", "0", "--end", "1", "--content",
                "replaced entirely",
            ],
        )
        .await;

        let (kernel_blocks, kv) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        drain_until(&mut mirror, &mut rx, kv).await;

        let expected = &kernel_blocks
            .iter()
            .find(|b| b.id == block_id)
            .expect("the block is still there")
            .content;
        assert_eq!(
            &mirror.block(&block_id).unwrap().content,
            expected,
            "a client following only the feed must hold exactly the kernel's text"
        );
        assert_eq!(mirror.version(), kv);
    });
}

/// A client that subscribes, then fetches, must not double-apply what the
/// snapshot already contains — the failure mode is a duplicated suffix, which
/// reads as plausible text rather than as an error.
#[test]
fn subscribing_before_the_snapshot_does_not_double_apply() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("feed-recovery").await.unwrap();
        kernel.join_context(context_id, "feed-test").await.unwrap();
        let principal = PrincipalId::for_agent_session("feed-author");
        let block_id = author_empty_block(&kernel, context_id, principal).await;
        let key = block_id.to_key();

        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();

        // Mutate BETWEEN subscribing and fetching: these land on the feed AND
        // in the snapshot.
        for chunk in ["alpha", "+beta", "+gamma"] {
            kj(&kernel, context_id, &["block", "append", &key, "--text", chunk]).await;
        }

        let (blocks, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();

        // Buffer everything that arrived meanwhile, then reconcile against the
        // snapshot's version.
        let mut mirror = ContextMirror::new(context_id);
        while let Ok(Some(FeedEvent::Changed(delivery))) =
            tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
        {
            mirror.receive(delivery).expect("buffered while unhydrated");
        }
        mirror.apply_snapshot(blocks, version).unwrap();

        assert_eq!(
            mirror.block(&block_id).unwrap().content,
            "alpha+beta+gamma",
            "the snapshot already held these appends; replaying them would duplicate text"
        );
    });
}

/// Status rides the same feed as text, so a client sees a block complete
/// without polling — in the same ordered stream as the text it completed.
#[test]
fn status_changes_arrive_on_the_same_feed_as_text() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("feed-status").await.unwrap();
        kernel.join_context(context_id, "feed-test").await.unwrap();
        let principal = PrincipalId::for_agent_session("feed-author");
        let block_id = author_empty_block(&kernel, context_id, principal).await;
        let key = block_id.to_key();

        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();
        let (blocks, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(context_id);
        mirror.apply_snapshot(blocks, version).unwrap();

        kj(
            &kernel,
            context_id,
            &["block", "append", &key, "--text", "output"],
        )
        .await;
        kj(&kernel, context_id, &["block", "status", &key, "done"]).await;

        let kv = kernel_version(&kernel, context_id).await;
        drain_until(&mut mirror, &mut rx, kv).await;

        let seen = mirror.block(&block_id).unwrap();
        assert_eq!(seen.content, "output");
        assert_eq!(seen.status, kaijutsu_types::Status::Done);
    });
}

/// The feed is one context's. A second context's changes must never appear on
/// it — they carry a different version counter, and mixing them would make
/// either one meaningless.
#[test]
fn a_feed_carries_only_its_own_context() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let watched = kernel.create_context("feed-watched").await.unwrap();
        let other = kernel.create_context("feed-other").await.unwrap();
        kernel.join_context(watched, "feed-test").await.unwrap();
        let principal = PrincipalId::for_agent_session("feed-author");
        let watched_block = author_empty_block(&kernel, watched, principal).await;
        let other_block = author_empty_block(&kernel, other, principal).await;

        let (observer, mut rx) = context_feed_channel(256);
        kernel.subscribe_context(watched, observer).await.unwrap();
        let (blocks, version) = kernel
            .get_blocks_versioned(watched, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(watched);
        mirror.apply_snapshot(blocks, version).unwrap();

        // Noise on the other context first, then the change we wait for.
        kj(
            &kernel,
            other,
            &[
                "block",
                "append",
                &other_block.to_key(),
                "--text",
                "not yours",
            ],
        )
        .await;
        kj(
            &kernel,
            watched,
            &[
                "block",
                "append",
                &watched_block.to_key(),
                "--text",
                "yours",
            ],
        )
        .await;

        let kv = kernel_version(&kernel, watched).await;
        // `receive` REFUSES a foreign delivery, so a leak fails this loop
        // rather than being quietly absorbed.
        drain_until(&mut mirror, &mut rx, kv).await;

        assert!(
            mirror.block(&other_block).is_none(),
            "the other context's block must never reach this feed"
        );
        assert!(
            mirror
                .block(&watched_block)
                .expect("watched block")
                .content
                .contains("yours")
        );
    });
}

/// The classification survives the wire: an append arrives as a suffix, not as
/// a whole-text replace. Asserted on the delivery itself rather than through
/// the mirror, because the mirror would produce identical text either way —
/// and shipping the whole block per token is the bandwidth regression the feed
/// exists to avoid.
#[test]
fn an_append_crosses_the_wire_as_a_suffix() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();

        let context_id = kernel.create_context("feed-suffix").await.unwrap();
        kernel.join_context(context_id, "feed-test").await.unwrap();
        let principal = PrincipalId::for_agent_session("feed-author");
        let block_id = author_empty_block(&kernel, context_id, principal).await;
        let key = block_id.to_key();
        kj(
            &kernel,
            context_id,
            &["block", "append", &key, "--text", "already here"],
        )
        .await;

        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();
        let (blocks, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(context_id);
        mirror.apply_snapshot(blocks, version).unwrap();

        kj(
            &kernel,
            context_id,
            &["block", "append", &key, "--text", " and more"],
        )
        .await;

        let mut appended = None;
        let kv = kernel_version(&kernel, context_id).await;
        while mirror.version() < kv {
            let delivery = match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(FeedEvent::Changed(d))) => d,
                other => panic!("expected a delivery, got {other:?}"),
            };
            for event in &delivery.events {
                if let ContextChange::TextAppended { block_id: id, suffix } = event
                    && *id == block_id
                {
                    appended = Some(suffix.clone());
                }
            }
            mirror.receive(delivery).unwrap();
        }

        assert_eq!(
            appended.as_deref(),
            Some(" and more"),
            "an append must cross the wire as its suffix alone, never as the whole block"
        );
    });
}

//! e2e: the **backpressure wire surface** — per-subscription `subSeq`,
//! coalesced `onBlockTextOpsBatch`, and `declareEventCapabilities`.
//!
//! Drives a real SSH + Cap'n Proto round-trip end to end, no GUI. The point of
//! doing it over the wire (rather than against the bus in-process) is that the
//! 2026-08-05 incidents all lived in the gap BETWEEN the bus and the client:
//! events the kernel published, the forwarder dropped, and the client never
//! learned about. So the assertions here are all client-side observations.
//!
//! The load-bearing one is `batched_delivery_is_byte_identical_to_per_op`: two
//! clients, same kernel, same burst, one with coalescing declared and one
//! without. If the op blobs they receive differ in a single byte or a single
//! position, coalescing is corrupting content and the test says so — that is
//! the property that lets the firehose fix be a *transport* change rather than
//! a change in what a client sees.

mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use capnp::capability::Promise;
use common::{connect_client, run_local, start_server};
use kaijutsu_client::KernelHandle;
use kaijutsu_client::kaijutsu_capnp::block_events;

/// A kernel-seeded rc script — guaranteed to exist on a fresh kernel, and its
/// CRDT block is a real block on the block flow bus. Typing into it is our
/// per-token op generator, standing in for a streaming model.
const RC_PATH: &str = "/etc/rc/coder/create/S00-stance.kai";

/// How many single-character edits the burst makes.
const BURST_KEYS: usize = 60;

/// Per-callback stall in the recording client.
///
/// Not padding: it is the *mechanism under test*. A client that answers
/// instantly never gives the server anything to coalesce, so a batch would only
/// ever form by luck. Stalling a few milliseconds per callback reproduces the
/// real condition (a GUI client whose executor is busy) deterministically.
const CALLBACK_STALL: Duration = Duration::from_millis(25);

#[derive(Default)]
struct Recorded {
    /// Text-op blobs in delivery order, flattened out of any batching. This is
    /// the sequence that must not depend on whether batching happened.
    ops: Vec<Vec<u8>>,
    /// Every `subSeq` seen on the ordered lane, in arrival order.
    sub_seqs: Vec<u64>,
    /// Callbacks that carried text ops (batched or not) — the "events per
    /// second at the client" number.
    text_calls: u64,
    /// Callbacks that were batches.
    batch_calls: u64,
}

/// A `BlockEvents` client that records what it is told and answers slowly.
struct Recorder {
    rec: Rc<RefCell<Recorded>>,
}

impl Recorder {
    fn new() -> (Self, Rc<RefCell<Recorded>>) {
        let rec = Rc::new(RefCell::new(Recorded::default()));
        (Self { rec: rec.clone() }, rec)
    }
}

/// Answer after a stall, so ops pile up behind us.
fn stall() -> Promise<(), capnp::Error> {
    Promise::from_future(async {
        tokio::time::sleep(CALLBACK_STALL).await;
        Ok(())
    })
}

#[allow(refining_impl_trait)]
impl block_events::Server for Recorder {
    fn on_block_text_ops(
        self: Rc<Self>,
        params: block_events::OnBlockTextOpsParams,
        _results: block_events::OnBlockTextOpsResults,
    ) -> Promise<(), capnp::Error> {
        let p = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let ops = match p.get_ops() {
            Ok(d) => d.to_vec(),
            Err(e) => return Promise::err(e),
        };
        {
            let mut r = self.rec.borrow_mut();
            r.ops.push(ops);
            r.sub_seqs.push(p.get_sub_seq());
            r.text_calls += 1;
        }
        stall()
    }

    fn on_block_text_ops_batch(
        self: Rc<Self>,
        params: block_events::OnBlockTextOpsBatchParams,
        _results: block_events::OnBlockTextOpsBatchResults,
    ) -> Promise<(), capnp::Error> {
        let p = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };
        let list = match p.get_ops() {
            Ok(l) => l,
            Err(e) => return Promise::err(e),
        };
        {
            let mut r = self.rec.borrow_mut();
            for i in 0..list.len() {
                match list.get(i) {
                    Ok(d) => r.ops.push(d.to_vec()),
                    Err(e) => return Promise::err(e),
                }
            }
            r.sub_seqs.push(p.get_sub_seq());
            r.text_calls += 1;
            r.batch_calls += 1;
        }
        stall()
    }

    // The rest of the channel: accept and count the sequence, so `subSeq`
    // continuity is asserted over the WHOLE stream rather than only the text
    // ops. Returning `unimplemented` here would make the server treat us as a
    // failing peer and muddy what the test is measuring.
    fn on_block_inserted(
        self: Rc<Self>,
        params: block_events::OnBlockInsertedParams,
        _r: block_events::OnBlockInsertedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_status_changed(
        self: Rc<Self>,
        params: block_events::OnBlockStatusChangedParams,
        _r: block_events::OnBlockStatusChangedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_metadata_changed(
        self: Rc<Self>,
        params: block_events::OnBlockMetadataChangedParams,
        _r: block_events::OnBlockMetadataChangedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_output_changed(
        self: Rc<Self>,
        params: block_events::OnBlockOutputChangedParams,
        _r: block_events::OnBlockOutputChangedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_deleted(
        self: Rc<Self>,
        params: block_events::OnBlockDeletedParams,
        _r: block_events::OnBlockDeletedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_collapsed(
        self: Rc<Self>,
        params: block_events::OnBlockCollapsedParams,
        _r: block_events::OnBlockCollapsedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_excluded_changed(
        self: Rc<Self>,
        params: block_events::OnBlockExcludedChangedParams,
        _r: block_events::OnBlockExcludedChangedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_block_moved(
        self: Rc<Self>,
        params: block_events::OnBlockMovedParams,
        _r: block_events::OnBlockMovedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_sync_reset(
        self: Rc<Self>,
        params: block_events::OnSyncResetParams,
        _r: block_events::OnSyncResetResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_context_switched(
        self: Rc<Self>,
        params: block_events::OnContextSwitchedParams,
        _r: block_events::OnContextSwitchedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_input_text_ops(
        self: Rc<Self>,
        params: block_events::OnInputTextOpsParams,
        _r: block_events::OnInputTextOpsResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }

    fn on_input_cleared(
        self: Rc<Self>,
        params: block_events::OnInputClearedParams,
        _r: block_events::OnInputClearedResults,
    ) -> Promise<(), capnp::Error> {
        self.note(params.get().map(|p| p.get_sub_seq()))
    }
}

impl Recorder {
    fn note(&self, seq: Result<u64, capnp::Error>) -> Promise<(), capnp::Error> {
        match seq {
            Ok(s) => {
                self.rec.borrow_mut().sub_seqs.push(s);
                Promise::ok(())
            }
            Err(e) => Promise::err(e),
        }
    }
}

/// Subscribe kernel-wide with a recording callback.
async fn subscribe(kernel: &KernelHandle, instance: &str) -> Rc<RefCell<Recorded>> {
    let (recorder, rec) = Recorder::new();
    let client: block_events::Client = capnp_rpc::new_client(recorder);
    kernel
        .subscribe_blocks_filtered(client, &kaijutsu_types::BlockEventFilter::default(), instance)
        .await
        .expect("subscribe_blocks_filtered");
    rec
}

/// Type `BURST_KEYS` single characters into the rc script's CRDT block, one op
/// per keystroke — the wire's closest analogue to a per-token model stream.
async fn burst(kernel: &KernelHandle) {
    let opened = kernel.editor_open(RC_PATH).await.expect("editor_open");
    let session = opened.session;
    kernel
        .editor_keys(session, "i")
        .await
        .expect("enter insert mode");
    // Fire every keystroke without waiting for each reply. Cap'n Proto
    // preserves call order on a connection, so the kernel still applies them in
    // sequence — but the ops now arrive on the bus as fast as the kernel can
    // produce them, instead of one per client round trip. That is the shape of
    // a real streaming turn, and it is the only way a wire test can put the
    // coalescer under genuine pressure.
    let chars: Vec<String> = (0..BURST_KEYS)
        .map(|i| char::from(b'a' + (i % 26) as u8).to_string())
        .collect();
    let keys: Vec<_> = chars
        .iter()
        .map(|ch| kernel.editor_keys(session, ch))
        .collect();
    for r in futures::future::join_all(keys).await {
        r.expect("editor_keys");
    }
    kernel.editor_keys(session, "<Esc>").await.expect("esc");
    // ZQ rolls the block back, leaving the kernel as we found it.
    kernel.editor_quit(session).await.expect("editor_quit");
}

/// **The integrity property.** Two clients, one kernel, one burst: the client
/// that opted into coalescing must receive byte-identical op blobs, in the same
/// order, as the client that did not — while making substantially fewer calls.
///
/// This is what makes `onBlockTextOpsBatch` a transport change rather than a
/// content change. It also proves the negotiation works in both directions: the
/// non-declaring client is the stand-in for the app binary on moltar, and it
/// must keep receiving exactly what it always did.
#[test]
fn batched_delivery_is_byte_identical_to_per_op() {
    run_local(async {
        let addr = start_server().await;

        // Client A: declares coalescing. Client B: an older-style client that
        // declares nothing at all.
        let client_a = connect_client(addr).await;
        let (kernel_a, _) = client_a.bind_kernel().await.unwrap();
        kernel_a
            .declare_event_capabilities(true, true)
            .await
            .expect("declare");

        let client_b = connect_client(addr).await;
        let (kernel_b, _) = client_b.bind_kernel().await.unwrap();

        let rec_a = subscribe(&kernel_a, "coalescing-client").await;
        let rec_b = subscribe(&kernel_b, "legacy-client").await;

        // Let subscription-time noise settle, then start both recordings from
        // the same point. `sub_seqs` keeps counting from 1 either way, so the
        // continuity assertion still covers the whole stream.
        tokio::time::sleep(Duration::from_millis(300)).await;
        rec_a.borrow_mut().ops.clear();
        rec_b.borrow_mut().ops.clear();

        burst(&kernel_a).await;

        // Drain: both bridges are stalling ~6ms per callback, so give them time
        // to flush everything the burst produced.
        tokio::time::sleep(Duration::from_secs(6)).await;

        let a = rec_a.borrow();
        let b = rec_b.borrow();

        assert!(
            !b.ops.is_empty(),
            "the burst must produce text ops on the wire at all"
        );
        assert_eq!(
            a.ops, b.ops,
            "coalesced delivery must reproduce the per-op blob sequence \
             byte-for-byte and in order — batching the CALLS must never change \
             the CONTENT"
        );
        assert!(
            a.batch_calls > 0,
            "the declaring client must actually have received batches \
             (otherwise this test proves nothing); got {} text calls, 0 batches",
            a.text_calls,
        );
        assert!(
            a.text_calls * 2 <= b.text_calls,
            "coalescing must cut client-visible text events substantially: \
             declaring client made {} calls for {} ops, legacy client made {}",
            a.text_calls,
            a.ops.len(),
            b.text_calls,
        );

        // subSeq rides the wire and never skips. The kernel bus is
        // lossless-or-terminated, so a gap here would mean a real hole.
        for (rec, who) in [(&*a, "coalescing"), (&*b, "legacy")] {
            assert!(!rec.sub_seqs.is_empty(), "{who} client saw no subSeq");
            for (i, seq) in rec.sub_seqs.iter().enumerate() {
                assert_eq!(
                    *seq,
                    i as u64 + 1,
                    "{who} client: subSeq must count 1,2,3… with no gaps — \
                     saw {seq} at position {i}"
                );
            }
        }
    });
}

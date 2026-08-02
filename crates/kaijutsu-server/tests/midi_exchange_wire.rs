//! e2e: the MIDI **exchange** wire surface — `kj midi identify` travelling
//! kernel → registry → server bridge → `BlockEvents.exchange` → a sink and
//! back, over a real SSH + Cap'n Proto round-trip (`docs/midi-next.md`
//! "SysEx: the exchange pattern", slice 1 step 5).
//!
//! Each half is already covered headless — the registry and the identity
//! parse in `kaijutsu-kernel`, the seat in `kaijutsu-client`, the ALSA client
//! in `kaijutsu-app` (live-gated). What only the wire can prove is the join:
//! that presence attribution really does name a connection the kernel can
//! call back on, that the appended capnp method carries request and reply
//! intact, and that a sink with no hardware refuses loudly instead of looking
//! like a silent device.
//!
//! Deliberately no ALSA anywhere: the "sink" here is a task answering the
//! seat, which is exactly the seam the app's hardware worker occupies.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{connect_client, run_local, start_server};
use kaijutsu_client::{KernelHandle, MidiExchangeSlot, block_events_channel};
use kaijutsu_types::{BlockKind, BlockQuery, ContextId, Status};

/// An Arturia-shaped Identity Reply — what a KeyStep Pro answers.
const IDENTITY_REPLY: [u8; 17] = [
    0xF0, 0x7E, 0x00, 0x06, 0x02, 0x00, 0x20, 0x6B, 0x01, 0x00, 0x02, 0x00, 0x01, 0x00, 0x03,
    0x04, 0xF7,
];

/// Run `code` and poll until its output block is terminal, returning
/// `(stdout + stderr, status)`. Mirrors `e2e_kj_workflow`'s helper (kept
/// local — `common` is shared verbatim by every test binary and this is the
/// only one that needs it here) with one addition: a `kj` verb's REFUSAL
/// lands on stderr, and refusals are half of what this file asserts.
async fn shell_exec_wait(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
) -> (String, Status) {
    let cmd = kernel
        .shell_execute(code, context_id, false)
        .await
        .unwrap_or_else(|e| panic!("shell_execute({code:?}) failed: {e}"));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("shell_exec_wait({code:?}) timed out");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let blocks = kernel
            .get_blocks(context_id, &BlockQuery::All)
            .await
            .unwrap_or_default();
        if let Some(out) = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd))
            && matches!(out.status, Status::Done | Status::Error)
        {
            let mut text = out.content.clone();
            if let Some(err) = out.stderr.as_ref() {
                text.push_str(err);
            }
            return (text, out.status);
        }
    }
}

/// Seat a stand-in sink that answers `answer` to every exchange, and report a
/// device present so the kernel has a connection to address. Returns the seat
/// so a test can empty it.
async fn seat_a_sink(
    kernel: &KernelHandle,
    answer: Result<Vec<u8>, String>,
) -> Arc<MidiExchangeSlot> {
    let slot = MidiExchangeSlot::new();
    // Subscribing is what registers this connection's exchange bridge
    // server-side — the same call the app makes for block events, carrying
    // the same callback in the reverse direction.
    let (callback, _events) = block_events_channel(64, slot.clone());
    kernel
        .subscribe_blocks(callback)
        .await
        .expect("subscribe_blocks (registers the exchange bridge)");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    slot.install(tx);
    tokio::task::spawn_local(async move {
        while let Some(req) = rx.recv().await {
            let _ = req.reply.send(answer.clone());
        }
    });

    // Presence is what tells the kernel WHICH connection to ask; the server
    // stamps the attribution from the connection itself, so this is the real
    // resolution path, not a fixture.
    kernel
        .report_midi_presence(
            "keystep-pro",
            true,
            "alsa",
            &[("KeyStep Pro MIDI 1".to_string(), "24:0".to_string())],
            1,
            "wire-test-host",
        )
        .await
        .expect("reportMidiPresence");
    slot
}

/// The anchor: a live device, a seated sink, a real Identity Reply — the
/// request reaches the sink over the appended capnp method and the parsed
/// answer comes back as a **pulled** fact at `/run/midi/<device>`.
#[test]
fn identify_round_trips_over_the_wire_and_files_a_pulled_fact() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("main").await.unwrap();
        let _ = kernel.join_context(ctx, "midi-exchange-wire").await.unwrap();

        let _slot = seat_a_sink(&kernel, Ok(IDENTITY_REPLY.to_vec())).await;

        let (out, status) = shell_exec_wait(&kernel, "kj midi identify keystep-pro", ctx).await;
        assert_eq!(status, Status::Done, "identify failed: {out}");
        assert!(out.contains("Arturia (00206b)"), "output: {out}");
        assert!(out.contains("/run/midi/keystep-pro"), "output: {out}");

        // …and the read-only view carries it, tagged `pulled` — not `sink`.
        let (view, status) = shell_exec_wait(&kernel, "cat /run/midi/keystep-pro", ctx).await;
        assert_eq!(status, Status::Done, "cat failed: {view}");
        assert!(view.contains("\"pulled\""), "the /run view must tag it pulled: {view}");
        assert!(view.contains("00206b"), "view: {view}");
        assert!(
            view.contains("wire-test-host"),
            "the sink-reported facts keep their own provenance: {view}"
        );
    });
}

/// A sink that refuses (silent device, unroutable port) surfaces its OWN
/// words to the player — bounded and loud, never an empty reply.
#[test]
fn a_sinks_refusal_reaches_the_player_over_the_wire() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("main").await.unwrap();
        let _ = kernel.join_context(ctx, "midi-exchange-wire").await.unwrap();

        let _slot = seat_a_sink(
            &kernel,
            Err("no matching reply from 24:0 within 2s".to_string()),
        )
        .await;

        let (out, status) = shell_exec_wait(&kernel, "kj midi identify keystep-pro", ctx).await;
        assert_eq!(status, Status::Error, "a refusal must be an error: {out}");
        assert!(out.contains("no matching reply from 24:0"), "output: {out}");
    });
}

/// A connection that reported a device but holds no MIDI hardware refuses at
/// once, naming what is missing. This is every non-sink client (the MCP
/// server, a `kj` CLI) — and the reason an empty seat is a loud refusal
/// rather than a silent wait.
#[test]
fn a_client_with_no_sink_installed_refuses_immediately() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("main").await.unwrap();
        let _ = kernel.join_context(ctx, "midi-exchange-wire").await.unwrap();

        let slot = seat_a_sink(&kernel, Ok(IDENTITY_REPLY.to_vec())).await;
        slot.clear(); // the hardware worker went away; the subscription didn't

        let started = std::time::Instant::now();
        let (out, status) = shell_exec_wait(&kernel, "kj midi identify keystep-pro", ctx).await;
        assert_eq!(status, Status::Error, "output: {out}");
        assert!(out.contains("no MIDI exchange sink"), "output: {out}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "an empty seat must refuse at once, not wait out the ladder"
        );
    });
}

/// Presence is the gate for an exchange (the one verb where it is): with
/// nobody reporting the device, the kernel says **unknown** — distinct from
/// absent, and never a guess at some other connection.
#[test]
fn identify_without_any_presence_report_is_an_unknown_state_error() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let (kernel, _) = client.bind_kernel().await.unwrap();
        let ctx = kernel.create_context("main").await.unwrap();
        let _ = kernel.join_context(ctx, "midi-exchange-wire").await.unwrap();

        let (out, status) = shell_exec_wait(&kernel, "kj midi identify keystep-pro", ctx).await;
        assert_eq!(status, Status::Error, "output: {out}");
        assert!(out.contains("UNKNOWN"), "output: {out}");
        assert!(out.contains("not the same as absent"), "output: {out}");
    });
}

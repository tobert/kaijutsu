//! Compose's vi registers and the kernel's store (`docs/vi.md`, "A
//! kernel-owned register store").
//!
//! Each key and each outside yank leaves its register writes on
//! [`ComposeState`]. [`flush_register_writes`] hands them to one writer task
//! per connection, which sends them in order with `registers_put`, so the
//! key path never waits on the wire. A write another connection made arrives
//! as `ServerEvent::RegisterWrite` and is applied by
//! [`apply_pushed_registers`]; it is not recorded as ours, so it is never
//! sent back.

use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_client::ServerEvent;
use kaijutsu_editor::RegisterWrite;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use super::ComposeState;
use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage, ServerEventMessage};
use crate::view::components::GlobalErrorQueue;

/// The writer that sends this connection's register writes, in order.
#[derive(Resource, Default)]
pub struct RegisterSink {
    live: Option<LiveSink>,
    /// Resolves when the newest writer has ended. The next writer waits on
    /// it, so a write made before a reconnect never lands after a later one.
    writer_done: Option<oneshot::Receiver<()>>,
}

struct LiveSink {
    /// The [`RpcActor::generation`] the writer sends through.
    generation: u64,
    tx: UnboundedSender<Vec<RegisterWrite>>,
}

impl RegisterSink {
    /// A sink over a test's own receiver, in place of a writer task.
    #[cfg(test)]
    pub fn for_test() -> (Self, UnboundedReceiver<Vec<RegisterWrite>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { live: Some(LiveSink { generation: 0, tx }), writer_done: None }, rx)
    }
}

/// Keep one register writer per connection: build it when the first actor
/// arrives and again whenever the actor is replaced or a writer ended. A
/// failed `registers_put` is reported as an `RpcError`; the local registers
/// keep the text.
pub fn sync_register_sink(actor: Option<Res<RpcActor>>, mut sink: ResMut<RegisterSink>, results: Res<RpcResultChannel>) {
    let Some(actor) = actor else {
        return;
    };
    if sink.live.as_ref().is_some_and(|live| live.generation == actor.generation && !live.tx.is_closed()) {
        return;
    }
    let (tx, rx): (_, UnboundedReceiver<Vec<RegisterWrite>>) = tokio::sync::mpsc::unbounded_channel();
    let before = sink.writer_done.take();
    let (done, done_rx) = oneshot::channel();
    let handle = actor.handle.clone();
    let errors = results.sender();
    IoTaskPool::get()
        .spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            let mut rx = rx;
            while let Some(batch) = rx.recv().await {
                if let Err(e) = handle.registers_put(batch).await {
                    let _ = errors.send(RpcResultMessage::RpcError { operation: "registers".into(), error: e.to_string() });
                }
            }
            let _ = done.send(());
        })
        .detach();
    sink.writer_done = Some(done_rx);
    sink.live = Some(LiveSink { generation: actor.generation, tx });
}

/// Send the register writes keys and outside yanks left on [`ComposeState`].
/// With no writer yet they wait on the state.
pub fn flush_register_writes(mut compose: ResMut<ComposeState>, mut sink: ResMut<RegisterSink>, mut errors: ResMut<GlobalErrorQueue>, time: Res<Time>) {
    let Some(live) = sink.live.as_ref() else {
        return;
    };
    let writes = compose.take_register_writes();
    if writes.is_empty() {
        return;
    }
    if let Err(e) = live.tx.send(writes) {
        // The writer ended; its batch is lost to the kernel but not to the
        // local registers, and `sync_register_sink` builds a new writer.
        sink.live = None;
        warn!("registers: writer stopped, {} writes not sent", e.0.len());
        errors.push("registers", "register writer stopped; the last yank or delete is not in the kernel's registers", time.elapsed_secs_f64());
    }
}

/// Apply the writes other connections pushed, in the order they arrived.
pub fn apply_pushed_registers(mut events: MessageReader<ServerEventMessage>, mut compose: ResMut<ComposeState>, mut errors: ResMut<GlobalErrorQueue>, time: Res<Time>) {
    for ServerEventMessage(event) in events.read() {
        let ServerEvent::RegisterWrite { write, .. } = event else {
            continue;
        };
        if let Err(e) = compose.apply_register_write(write) {
            warn!("registers: a pushed write was refused: {e}");
            errors.push("registers", format!("a pushed register write was refused: {e}"), time.elapsed_secs_f64());
        }
    }
}

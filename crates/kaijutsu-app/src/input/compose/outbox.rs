//! The connection's draft outbox (`kaijutsu_client::draft_outbox`): chat
//! edits, submits and draft reads reach the kernel in the order they were
//! made, and the key path never waits on one.
//!
//! One outbox per [`RpcActor`]. When the actor is replaced (a reconnect, or
//! a context switch that joins through a new actor), the old outbox is
//! dropped and its writer finishes what was queued, then ends. The new
//! writer starts only after the old one has ended, so an edit made before
//! the replacement never lands after one made after it. The old writer's
//! acks are still read until it ends.

use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use kaijutsu_client::draft_outbox::{self, DraftEdit, DraftOutbox, Landed};
use kaijutsu_editor::EditOp;
use kaijutsu_types::ContextId;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::oneshot;

use super::ComposeState;
use crate::connection::RpcActor;

/// The draft writer's own words for an edit it could not finish
/// (`kaijutsu_client::draft_outbox`).
const WRITER_STOPPED: &str = "draft writer stopped";

/// The draft outbox of the current connection.
#[derive(Resource, Default)]
pub struct DraftOutboxState {
    live: Option<LiveDraftOutbox>,
    /// Ack receivers of replaced outboxes, read until their writers end.
    retiring: Vec<UnboundedReceiver<Landed>>,
    /// Resolves when the newest writer has ended. The next writer waits on
    /// it before it sends anything.
    writer_done: Option<oneshot::Receiver<()>>,
}

struct LiveDraftOutbox {
    /// The [`RpcActor::generation`] the outbox writes to.
    generation: u64,
    outbox: DraftOutbox,
    landed: UnboundedReceiver<Landed>,
}

impl DraftOutboxState {
    /// The outbox, or `None` before the first connection.
    pub fn outbox(&self) -> Option<&DraftOutbox> {
        self.live.as_ref().map(|live| &live.outbox)
    }

    /// Queue `ops` as edits to `context`'s draft, counting each in
    /// `compose`. Returns the first failure; an edit that could not be
    /// queued is counted as landed and failed.
    pub fn send_ops(&mut self, compose: &mut ComposeState, context: ContextId, ops: &[EditOp]) -> Result<(), String> {
        let mut failure = None;
        for op in ops {
            let generation = compose.sent();
            let edit = DraftEdit {
                offset: op.offset as u64,
                insert: op.insert.clone(),
                delete: op.delete as u64,
            };
            let queued = match self.live.as_ref() {
                Some(live) => live.outbox.edit(context, generation, edit).map_err(|e| e.to_string()),
                None => Err("no connection".to_string()),
            };
            if let Err(e) = queued {
                compose.landed(generation, None);
                // A writer that has stopped takes nothing more; the next
                // `sync_draft_outbox` builds a fresh one. Its acks are still
                // read: the edits it lost land there as failures.
                self.retire_live();
                failure.get_or_insert(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Every ack waiting, from the live outbox and the retiring ones.
    pub fn drain(&mut self) -> Vec<Landed> {
        let mut acks = Vec::new();
        let mut writer_stopped = false;
        if let Some(live) = self.live.as_mut() {
            while let Ok(landed) = live.landed.try_recv() {
                writer_stopped |= landed.result.as_ref().is_err_and(|e| e.contains(WRITER_STOPPED));
                acks.push(landed);
            }
        }
        self.retiring.retain_mut(|rx| loop {
            match rx.try_recv() {
                Ok(landed) => acks.push(landed),
                Err(TryRecvError::Empty) => break true,
                Err(TryRecvError::Disconnected) => break false,
            }
        });
        if writer_stopped {
            self.retire_live();
        }
        acks
    }

    /// Install `outbox` and its ack receiver as the live outbox, retiring the
    /// one before it.
    fn replace(&mut self, generation: u64, outbox: DraftOutbox, landed: UnboundedReceiver<Landed>) {
        self.retire_live();
        self.live = Some(LiveDraftOutbox { generation, outbox, landed });
    }

    /// Drop the live outbox and keep reading its acks until its writer
    /// ends: every edit it lost lands there as a failure, and the ledger
    /// counts it as landed only when it is read.
    fn retire_live(&mut self) {
        if let Some(old) = self.live.take() {
            self.retiring.push(old.landed);
        }
    }

    /// An outbox over a test's own sink, whose writer the test runs.
    #[cfg(test)]
    pub fn for_test(outbox: DraftOutbox, landed: UnboundedReceiver<Landed>) -> Self {
        let mut state = Self::default();
        state.replace(0, outbox, landed);
        state
    }

    /// Drop the live outbox, as a replaced connection does; its writer then
    /// finishes what was queued.
    #[cfg(test)]
    pub fn retire(&mut self) {
        self.retire_live();
    }
}

/// Keep one draft outbox per connection: build it when the first actor
/// arrives, again whenever the actor is replaced, and again when a writer
/// stopped. The writer is runtime-free and runs on Bevy's `IoTaskPool`.
pub fn sync_draft_outbox(actor: Option<Res<RpcActor>>, mut state: ResMut<DraftOutboxState>) {
    let Some(actor) = actor else {
        return;
    };
    if state.live.as_ref().is_some_and(|live| live.generation == actor.generation) {
        return;
    }
    let (outbox, writer, landed) = draft_outbox::channel(actor.handle.clone());
    let before = state.writer_done.take();
    let (done, done_rx) = oneshot::channel();
    IoTaskPool::get()
        .spawn(async move {
            if let Some(before) = before {
                // Ended or dropped, either way it sends nothing more.
                let _ = before.await;
            }
            writer.await;
            let _ = done.send(());
        })
        .detach();
    state.writer_done = Some(done_rx);
    state.replace(actor.generation, outbox, landed);
}

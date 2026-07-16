//! Prefix context verbs — the consumers behind `Ctrl+A`'s context chords
//! (docs/input.md "The prefix table"):
//!
//! - `Ctrl+A 0-9` → [`Action::SwitchToActiveSeat`] — ring-0 (ACTIVE rank)
//!   seat n, resolved through the same placement engine the well renders
//!   (`time_well::card::assign_placement`), so the digit you learned at the
//!   well works identically from the conversation view.
//! - `Ctrl+A Ctrl+A` → [`Action::SwitchToPreviousContext`] — MRU toggle.
//! - `Ctrl+A n`/`p` → [`Action::ActiveSeatStep`] — walk ring-0 seats.
//! - `Ctrl+A q` → [`Action::CloseAndDemoteContext`] — demote the current
//!   context one ladder step (kernel-owned ladder) and land on the
//!   MRU-previous one.
//!
//! Replaces the old raw Ctrl+1/2/3 MRU shortcuts (retired 2026-07-16 — they
//! fired underneath the fullscreen vi editor, and their `ids[0]` target was
//! the context you were already on).

use bevy::prelude::*;

use crate::cell::{ContextSwitchRequested, DocumentCache};
use crate::input::{Action, ActionFired};
use crate::view::time_well::card::assign_placement;

/// Ring-0 seat ids in seat order, from the same drift poll the well reads.
fn active_ring(drift: &crate::ui::drift::DriftState) -> Vec<kaijutsu_crdt::ContextId> {
    // The well filters archived contexts before placement (sync.rs does the
    // same) — archived ids never hold a seat.
    let live: Vec<_> = drift
        .contexts
        .iter()
        .filter(|c| !c.archived)
        .cloned()
        .collect();
    assign_placement(&live).rings[0].clone()
}

/// Consume the prefix context verbs.
pub fn handle_prefix_context_verbs(
    mut actions: MessageReader<ActionFired>,
    drift: Res<crate::ui::drift::DriftState>,
    doc_cache: Res<DocumentCache>,
    actor: Option<Res<crate::connection::RpcActor>>,
    mut switch_writer: MessageWriter<ContextSwitchRequested>,
) {
    for ActionFired { action, .. } in actions.read() {
        match action {
            Action::SwitchToActiveSeat(n) => {
                let ring = active_ring(&drift);
                match ring.get(*n) {
                    Some(&id) => {
                        switch_writer.write(ContextSwitchRequested { context_id: id });
                        info!("prefix: seat {n} → {}", id.short());
                    }
                    None => info!("prefix: active ring seat {n} is empty"),
                }
            }

            Action::SwitchToPreviousContext => {
                // mru_ids() is most-recent-first and the current context is
                // touched on every switch, so [0] is where we are and [1] is
                // the toggle target.
                match doc_cache.mru_ids().get(1).copied() {
                    Some(id) => {
                        switch_writer.write(ContextSwitchRequested { context_id: id });
                        info!("prefix: toggle to previous {}", id.short());
                    }
                    None => info!("prefix: no previous context to toggle to"),
                }
            }

            Action::ActiveSeatStep(delta) => {
                let ring = active_ring(&drift);
                if ring.is_empty() {
                    info!("prefix: active ring is empty");
                    continue;
                }
                let cur = doc_cache
                    .active_id()
                    .and_then(|id| ring.iter().position(|&r| r == id));
                // Not seated (or no active context): n lands on seat 0,
                // p on the last seat — entering the ring from either end.
                let target = match cur {
                    Some(pos) => {
                        (pos as i32 + delta).rem_euclid(ring.len() as i32) as usize
                    }
                    None if *delta > 0 => 0,
                    None => ring.len() - 1,
                };
                let id = ring[target];
                switch_writer.write(ContextSwitchRequested { context_id: id });
                info!("prefix: seat step {delta} → seat {target} ({})", id.short());
            }

            Action::CloseAndDemoteContext => {
                let Some(current) = doc_cache.active_id() else {
                    info!("prefix: no active context to close");
                    continue;
                };
                // Demote via the kernel-owned ladder (fire-and-forget, same
                // pattern as the well's `d` verb — failures warn loudly).
                if let Some(actor) = actor.as_ref() {
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.demote_context(current).await {
                                log::warn!(
                                    "prefix: demote {} failed: {e}",
                                    current.short()
                                );
                            }
                        })
                        .detach();
                }
                // Land on the MRU-previous context; nowhere to land → stay
                // (the demotion is placement, the conversation stays open).
                match doc_cache.mru_ids().get(1).copied() {
                    Some(id) => {
                        switch_writer.write(ContextSwitchRequested { context_id: id });
                        info!(
                            "prefix: close-and-demote {} → {}",
                            current.short(),
                            id.short()
                        );
                    }
                    None => info!(
                        "prefix: demoted {} (no previous context to land on)",
                        current.short()
                    ),
                }
            }

            _ => {}
        }
    }
}

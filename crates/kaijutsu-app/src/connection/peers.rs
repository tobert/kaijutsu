//! Attached-peer roster polling — `ActorHandle::list_peers` (the kernel's
//! peer registry: every connected client, human app or MCP session, attaches
//! with a `<kind>/<name>` nick). Follows `drift::poll_drift_state`'s exact
//! pattern (clone the handle, spawn an `IoTaskPool` task, ship the result
//! back through `RpcResultChannel`, drain it here) rather than inventing a
//! new one.
//!
//! Kept at connection level, like drift: `PeerRoster` is data about the
//! attached fleet, not a view. The room's "seats at the table" visuals
//! (`view::room::seats`) are the one consumer today, reconciling wisp
//! entities against `PeerRoster.peers` each frame.

use bevy::prelude::*;

use kaijutsu_client::PeerInfo;

use crate::connection::{RpcActor, RpcResultChannel, RpcResultMessage};

/// How often to poll the peer roster (seconds) — same cadence as
/// `drift::DRIFT_POLL_INTERVAL`. Peer attach/detach has no push event on the
/// wire yet (docs/issues.md's peer-registry doctrine entry), so a poll is
/// the only source; 5s is plenty responsive for "a wisp fades in/out",
/// which itself takes 2-3s.
const PEER_POLL_INTERVAL: f64 = 5.0;

/// Live attached-peer roster — populated by polling, consumed by
/// `view::room::seats::reconcile_seats`.
#[derive(Resource, Default)]
pub struct PeerRoster {
    pub peers: Vec<PeerInfo>,
    /// Last poll timestamp (`Time::elapsed_secs_f64()`), mirrors
    /// `DriftState::last_poll`.
    last_poll: f64,
    /// Whether we've received at least one successful poll.
    pub loaded: bool,
}

pub struct PeerRosterPlugin;

impl Plugin for PeerRosterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PeerRoster>()
            .add_systems(Update, (poll_peer_roster, update_peer_roster).chain());
    }
}

/// Poll the peer roster at [`PEER_POLL_INTERVAL`] — `drift::poll_drift_state`'s
/// own doc has the full rationale for the clone-handle/spawn-task/send-result
/// shape this mirrors.
fn poll_peer_roster(
    actor: Option<Res<RpcActor>>,
    mut roster: ResMut<PeerRoster>,
    time: Res<Time>,
    result_channel: Res<RpcResultChannel>,
) {
    let Some(actor) = actor else { return };

    let elapsed = time.elapsed_secs_f64();
    if elapsed - roster.last_poll < PEER_POLL_INTERVAL {
        return;
    }
    // Set immediately to prevent stacking concurrent requests, same as
    // `poll_drift_state`.
    roster.last_poll = elapsed;

    let handle = actor.handle.clone();
    let tx = result_channel.sender();

    bevy::tasks::IoTaskPool::get()
        .spawn(async move {
            match handle.list_peers().await {
                Ok(peers) => {
                    let _ = tx.send(RpcResultMessage::PeersReceived { peers });
                }
                Err(e) => {
                    log::debug!("peer roster poll: list_peers failed: {e}");
                }
            }
        })
        .detach();
}

/// Drain `PeersReceived` into `PeerRoster`.
fn update_peer_roster(mut roster: ResMut<PeerRoster>, mut events: MessageReader<RpcResultMessage>) {
    for event in events.read() {
        if let RpcResultMessage::PeersReceived { peers } = event {
            roster.peers = peers.clone();
            roster.loaded = true;
        }
    }
}

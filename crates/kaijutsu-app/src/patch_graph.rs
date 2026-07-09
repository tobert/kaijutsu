//! ALSA sequencer topology reader — the patch bay's observed-reality source.
//!
//! Slice 0 of `docs/scenes/patchbay.md`: read the local ALSA seq graph
//! (clients, ports, subscriptions) so the patch-bay scene can render it.
//! Read-only — this module never creates or removes subscriptions.
//!
//! Mirrors the enumeration idioms in `midi_in.rs` (ClientIter/PortIter,
//! get_any_client_info/get_any_port_info). The `Seq` handle is not Send:
//! the reader lives as a `NonSend` resource, same as `MidiSink`.

use std::collections::BTreeSet;
use std::ffi::CString;

use alsa::seq;

/// One ALSA seq port, flattened for the scene.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointInfo {
    pub client_id: i32,
    pub port_id: i32,
    pub client_name: String,
    pub port_name: String,
    /// Port is readable + subscribable (a source others can wire from).
    pub is_source: bool,
    /// Port is writable + subscribable (a sink others can wire to).
    pub is_sink: bool,
}

/// One subscription (wire): src port feeds dst port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireInfo {
    pub src: (i32, i32),
    pub dst: (i32, i32),
}

/// A point-in-time picture of the local seq graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PatchGraphSnapshot {
    pub endpoints: Vec<EndpointInfo>,
    pub wires: Vec<WireInfo>,
}

/// What changed between two snapshots (pure; drives scene reconcile).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphDelta {
    pub added_wires: Vec<WireInfo>,
    pub removed_wires: Vec<WireInfo>,
    /// True when the endpoint set (ids or names) changed at all.
    pub endpoints_changed: bool,
}

impl GraphDelta {
    pub fn is_empty(&self) -> bool {
        self.added_wires.is_empty() && self.removed_wires.is_empty() && !self.endpoints_changed
    }
}

/// Owns its own read-only `Seq` handle (client name "kaijutsu-patchview").
pub struct PatchGraphReader {
    seq: alsa::Seq,
}

impl PatchGraphReader {
    /// Open a sequencer handle for topology reads.
    pub fn open() -> Result<Self, alsa::Error> {
        let seq = alsa::Seq::open(None, None, false)?;
        // A static literal: it never contains an interior NUL, so this can't fail.
        let name =
            CString::new("kaijutsu-patchview").expect("\"kaijutsu-patchview\" has no interior NUL");
        seq.set_client_name(&name)?;
        Ok(Self { seq })
    }

    /// Enumerate every client/port and every subscription, in one pass.
    ///
    /// Subscriptions are queried in both directions
    /// ([`seq::QuerySubsType::READ`] from the sender's port and
    /// [`seq::QuerySubsType::WRITE`] from the receiver's port) and deduped:
    /// the same wire is visible from either end, and querying only one
    /// direction would miss a subscription that — for whatever reason
    /// (a client that exited uncleanly, a permission quirk) — only shows up
    /// from the other side.
    pub fn snapshot(&self) -> Result<PatchGraphSnapshot, alsa::Error> {
        let mut endpoints = Vec::new();
        let mut wires: BTreeSet<WireInfo> = BTreeSet::new();

        for client in seq::ClientIter::new(&self.seq) {
            let client_id = client.get_client();
            let client_name = client.get_name().unwrap_or("?").to_string();

            for port in seq::PortIter::new(&self.seq, client_id) {
                let addr = port.addr();
                let port_name = port.get_name().unwrap_or("?").to_string();
                let caps = port.get_capability();
                let is_source = caps.contains(seq::PortCap::READ | seq::PortCap::SUBS_READ);
                let is_sink = caps.contains(seq::PortCap::WRITE | seq::PortCap::SUBS_WRITE);

                endpoints.push(EndpointInfo {
                    client_id,
                    port_id: addr.port,
                    client_name: client_name.clone(),
                    port_name,
                    is_source,
                    is_sink,
                });

                for kind in [seq::QuerySubsType::READ, seq::QuerySubsType::WRITE] {
                    for sub in seq::PortSubscribeIter::new(&self.seq, addr, kind) {
                        let src = sub.get_sender();
                        let dst = sub.get_dest();
                        wires.insert(WireInfo {
                            src: (src.client, src.port),
                            dst: (dst.client, dst.port),
                        });
                    }
                }
            }
        }

        endpoints.sort_by_key(|e| (e.client_id, e.port_id));
        // `wires` is a BTreeSet<WireInfo> with the derived (src, dst) Ord —
        // iteration is already ascending.
        let wires = wires.into_iter().collect();
        Ok(PatchGraphSnapshot { endpoints, wires })
    }
}

/// Pure diff between two snapshots. Wire identity is (src, dst).
pub fn diff(prev: &PatchGraphSnapshot, next: &PatchGraphSnapshot) -> GraphDelta {
    let prev_wires: BTreeSet<WireInfo> = prev.wires.iter().copied().collect();
    let next_wires: BTreeSet<WireInfo> = next.wires.iter().copied().collect();

    GraphDelta {
        added_wires: next_wires.difference(&prev_wires).copied().collect(),
        removed_wires: prev_wires.difference(&next_wires).copied().collect(),
        endpoints_changed: prev.endpoints != next.endpoints,
    }
}

/// Drop the ALSA System client (client 0: Timer/Announce) and our own
/// reader client from a snapshot — scene clutter, not studio topology.
/// Wires touching dropped endpoints are dropped with them.
pub fn without_plumbing(snapshot: &PatchGraphSnapshot, own_client: i32) -> PatchGraphSnapshot {
    let is_plumbing = |client_id: i32| client_id == 0 || client_id == own_client;

    let endpoints: Vec<EndpointInfo> = snapshot
        .endpoints
        .iter()
        .filter(|e| !is_plumbing(e.client_id))
        .cloned()
        .collect();
    let wires: Vec<WireInfo> = snapshot
        .wires
        .iter()
        .filter(|w| !is_plumbing(w.src.0) && !is_plumbing(w.dst.0))
        .copied()
        .collect();

    PatchGraphSnapshot { endpoints, wires }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(client_id: i32, port_id: i32, client_name: &str, port_name: &str) -> EndpointInfo {
        EndpointInfo {
            client_id,
            port_id,
            client_name: client_name.into(),
            port_name: port_name.into(),
            is_source: true,
            is_sink: false,
        }
    }

    fn wire(src: (i32, i32), dst: (i32, i32)) -> WireInfo {
        WireInfo { src, dst }
    }

    // -- diff ----------------------------------------------------------

    #[test]
    fn diff_of_identical_snapshots_is_empty() {
        let snap = PatchGraphSnapshot {
            endpoints: vec![endpoint(0, 0, "System", "Timer")],
            wires: vec![wire((14, 0), (128, 0))],
        };
        let delta = diff(&snap, &snap);
        assert!(delta.is_empty(), "{delta:?}");
    }

    #[test]
    fn diff_reports_added_wire() {
        let prev = PatchGraphSnapshot::default();
        let next = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((14, 0), (128, 0))],
        };
        let delta = diff(&prev, &next);
        assert_eq!(delta.added_wires, vec![wire((14, 0), (128, 0))]);
        assert!(delta.removed_wires.is_empty());
        assert!(!delta.endpoints_changed);
        assert!(!delta.is_empty());
    }

    #[test]
    fn diff_reports_removed_wire() {
        let prev = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((14, 0), (128, 0))],
        };
        let next = PatchGraphSnapshot::default();
        let delta = diff(&prev, &next);
        assert!(delta.added_wires.is_empty());
        assert_eq!(delta.removed_wires, vec![wire((14, 0), (128, 0))]);
        assert!(!delta.endpoints_changed);
    }

    #[test]
    fn diff_reports_both_added_and_removed_wires_when_wiring_is_replaced() {
        let prev = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((14, 0), (128, 0))],
        };
        let next = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((20, 0), (128, 0))],
        };
        let delta = diff(&prev, &next);
        assert_eq!(delta.added_wires, vec![wire((20, 0), (128, 0))]);
        assert_eq!(delta.removed_wires, vec![wire((14, 0), (128, 0))]);
    }

    #[test]
    fn diff_added_and_removed_wires_come_out_sorted() {
        let prev = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((30, 0), (128, 0)), wire((10, 0), (128, 0))],
        };
        let next = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((40, 0), (128, 0)), wire((5, 0), (128, 0))],
        };
        let delta = diff(&prev, &next);
        assert_eq!(
            delta.added_wires,
            vec![wire((5, 0), (128, 0)), wire((40, 0), (128, 0))]
        );
        assert_eq!(
            delta.removed_wires,
            vec![wire((10, 0), (128, 0)), wire((30, 0), (128, 0))]
        );
    }

    #[test]
    fn diff_flags_endpoints_changed_on_a_name_change_even_with_wiring_untouched() {
        let prev = PatchGraphSnapshot {
            endpoints: vec![endpoint(128, 0, "TiMidity", "port 0")],
            wires: vec![wire((14, 0), (128, 0))],
        };
        let mut next = prev.clone();
        next.endpoints[0].client_name = "TiMidity++".into();
        let delta = diff(&prev, &next);
        assert!(delta.endpoints_changed);
        assert!(delta.added_wires.is_empty());
        assert!(delta.removed_wires.is_empty());
        // Renamed but still non-empty: the delta as a whole isn't empty either.
        assert!(!delta.is_empty());
    }

    #[test]
    fn diff_flags_endpoints_changed_on_id_reordering() {
        // Same endpoints, different order: the Vec (and thus PartialEq) differs
        // even though the *set* of endpoints is identical.
        let prev = PatchGraphSnapshot {
            endpoints: vec![endpoint(14, 0, "A", "p"), endpoint(20, 0, "B", "p")],
            wires: Vec::new(),
        };
        let next = PatchGraphSnapshot {
            endpoints: vec![endpoint(20, 0, "B", "p"), endpoint(14, 0, "A", "p")],
            wires: Vec::new(),
        };
        let delta = diff(&prev, &next);
        assert!(delta.endpoints_changed);
    }

    // -- without_plumbing -----------------------------------------------

    #[test]
    fn without_plumbing_drops_system_client_and_its_wires() {
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![
                endpoint(0, 0, "System", "Timer"),
                endpoint(0, 1, "System", "Announce"),
                endpoint(128, 0, "TiMidity", "port 0"),
            ],
            wires: vec![
                // Announce is subscribed by our own ear (client 999) — plumbing.
                wire((0, 1), (999, 0)),
                // A real studio wire between two non-system clients.
                wire((14, 0), (128, 0)),
            ],
        };
        let cleaned = without_plumbing(&snapshot, 999);
        assert_eq!(
            cleaned.endpoints,
            vec![endpoint(128, 0, "TiMidity", "port 0")]
        );
        assert_eq!(cleaned.wires, vec![wire((14, 0), (128, 0))]);
    }

    #[test]
    fn without_plumbing_drops_own_client_endpoints_and_wires() {
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![
                endpoint(42, 0, "kaijutsu-patchview", "capture"),
                endpoint(128, 0, "TiMidity", "port 0"),
            ],
            wires: vec![wire((42, 0), (128, 0)), wire((14, 0), (128, 0))],
        };
        let cleaned = without_plumbing(&snapshot, 42);
        assert_eq!(
            cleaned.endpoints,
            vec![endpoint(128, 0, "TiMidity", "port 0")]
        );
        assert_eq!(cleaned.wires, vec![wire((14, 0), (128, 0))]);
    }

    #[test]
    fn without_plumbing_keeps_a_wire_whose_far_end_is_dropped_only_if_neither_end_is_plumbing() {
        // A wire where only ONE end is plumbing must still be dropped: it's
        // not a "studio topology" wire once one of its ports vanished.
        let snapshot = PatchGraphSnapshot {
            endpoints: Vec::new(),
            wires: vec![wire((0, 1), (128, 0)), wire((14, 0), (0, 1))],
        };
        let cleaned = without_plumbing(&snapshot, 999);
        assert!(cleaned.wires.is_empty(), "{:?}", cleaned.wires);
    }

    #[test]
    fn without_plumbing_is_a_no_op_when_there_is_no_plumbing_present() {
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![endpoint(14, 0, "A", "p"), endpoint(128, 0, "B", "p")],
            wires: vec![wire((14, 0), (128, 0))],
        };
        let cleaned = without_plumbing(&snapshot, 999);
        assert_eq!(cleaned, snapshot);
    }

    #[test]
    fn without_plumbing_on_an_empty_snapshot_is_empty() {
        let cleaned = without_plumbing(&PatchGraphSnapshot::default(), 42);
        assert_eq!(cleaned, PatchGraphSnapshot::default());
    }

    // -- GraphDelta::is_empty --------------------------------------------

    #[test]
    fn graph_delta_default_is_empty() {
        assert!(GraphDelta::default().is_empty());
    }

    // -- live ALSA integration --------------------------------------------

    /// Opens a real sequencer handle and takes one snapshot. Needs
    /// `/dev/snd/seq`; `#[ignore]` so CI without a sequencer stays green —
    /// run with `--ignored` on a box that has one (e.g. zorak).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a live ALSA sequencer (/dev/snd/seq)"]
    fn alsa_smoke() {
        let reader = PatchGraphReader::open().expect("open ALSA seq for patch-graph reads");
        let snapshot = reader.snapshot().expect("snapshot the local seq graph");

        assert!(
            snapshot.endpoints.iter().any(|e| e.client_id == 0),
            "expected the ALSA System client (0) in {:#?}",
            snapshot.endpoints
        );
        // Endpoints and wires both come out sorted ascending.
        assert!(
            snapshot
                .endpoints
                .windows(2)
                .all(|w| (w[0].client_id, w[0].port_id) <= (w[1].client_id, w[1].port_id))
        );
        assert!(snapshot.wires.windows(2).all(|w| w[0] <= w[1]));
    }
}

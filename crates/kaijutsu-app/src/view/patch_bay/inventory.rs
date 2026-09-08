//! The projected audio inventory (`/run/audio/<node-dir>/inventory.json`,
//! `docs/audio-daemon.md` "One inventory owner") — the JSON contract a
//! daemon publishes and this app parses, plus the pure observed-graph
//! shapes `patch_bay` renders from it.
//!
//! **A separate copy, not a shared type.** `kaijutsu_audio_runtime::
//! patch_graph` keeps its own `EndpointInfo`/`WireInfo`/`PatchGraphSnapshot`/
//! `GraphDelta`/`diff`/`without_plumbing` for the daemon's own local
//! snapshot — the one it serializes into the JSON below. The wire body is
//! the contract between the two crates, not a Rust type; this module owns
//! only the parse half, and never depends on `kaijutsu-audio-runtime`. That
//! dependency is gone from `kaijutsu-app`'s `Cargo.toml` entirely — the app
//! reads the kernel's projection now, never a hardware graph.

use std::collections::BTreeSet;

use serde::Deserialize;

/// `/run/audio`'s well-known root. Not yet in `kaijutsu_types::paths` — the
/// projection lane names the exact node-path encoding as still-to-settle
/// (`docs/audio-daemon.md`, "One inventory owner"); this constant moves
/// there once it lands.
pub const AUDIO_RUN_ROOT: &str = "/run/audio";

// ── Wire JSON (the daemon's published report) ───────────────────────────────

/// One endpoint as a node publishes it. Fields the scene never reads
/// (`revision`, the epoch stamps, `backend`, `state`, `listening`, `usb_id`)
/// are left off this struct on purpose: an ordinary `Deserialize` (no
/// `deny_unknown_fields`) ignores JSON keys with no matching field, so a
/// node is free to publish more than the patch bay uses without breaking
/// this parse.
#[derive(Debug, Clone, Deserialize)]
pub struct RawEndpoint {
    pub client_id: i32,
    pub port_id: i32,
    pub client_name: String,
    pub port_name: String,
    pub is_source: bool,
    pub is_sink: bool,
    /// Monotonic per-endpoint MIDI event count the node has observed (input)
    /// or sent (output) on this port. `render_port_advanced` below is the
    /// pure test of whether this rose since the last poll.
    #[serde(default)]
    pub events: u64,
}

/// One subscription; addresses ride the wire as `"client:port"` strings.
#[derive(Debug, Clone, Deserialize)]
pub struct RawWire {
    pub src: String,
    pub dst: String,
}

/// One node's published inventory report — the JSON body at
/// `/run/audio/<node-dir>/inventory.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct InventoryReport {
    /// The peer nick, e.g. `"audio/moltar"` — carried through to
    /// [`PatchGraphSnapshot`]'s node label so the selection card names which
    /// node's fabric is on screen.
    pub node: String,
    /// The node's connection dropped and this is its last observation
    /// (`docs/audio-daemon.md`: "a retained last observation is labeled
    /// stale").
    #[serde(default)]
    pub stale: bool,
    /// The node's own ALSA client ids — plumbing, filtered by
    /// [`without_plumbing`] the same way the old local reader's own client
    /// id was.
    #[serde(default)]
    pub own_clients: Vec<i32>,
    #[serde(default)]
    pub endpoints: Vec<RawEndpoint>,
    #[serde(default)]
    pub wires: Vec<RawWire>,
}

/// Parse a `"client:port"` address string. `None` on anything that doesn't
/// split into two integers — a malformed wire endpoint is dropped, never
/// guessed (the same "one bad row costs one row, not the rest" stance as
/// `connection::roster::parse_row`).
fn parse_addr(s: &str) -> Option<(i32, i32)> {
    let (client, port) = s.split_once(':')?;
    Some((client.trim().parse().ok()?, port.trim().parse().ok()?))
}

// ── Observed-graph shapes (the app's own copy; see the module doc) ─────────

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointInfo {
    pub client_id: i32,
    pub port_id: i32,
    pub client_name: String,
    pub port_name: String,
    pub is_source: bool,
    pub is_sink: bool,
    pub events: u64,
}

/// One subscription (wire): src port feeds dst port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireInfo {
    pub src: (i32, i32),
    pub dst: (i32, i32),
}

/// A point-in-time picture of one node's observed graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PatchGraphSnapshot {
    pub endpoints: Vec<EndpointInfo>,
    pub wires: Vec<WireInfo>,
    /// Mirrors [`InventoryReport::stale`] — `describe_selection` appends
    /// "(STALE)" when set.
    pub stale: bool,
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

/// Pure diff between two snapshots. Wire identity is (src, dst). Does not
/// consider `stale` — a staleness flip with an unchanged graph is not a
/// topology change, though the caller (`patch_bay::apply_audio_inventory`)
/// still re-dirties the text layer for it.
pub fn diff(prev: &PatchGraphSnapshot, next: &PatchGraphSnapshot) -> GraphDelta {
    let prev_wires: BTreeSet<WireInfo> = prev.wires.iter().copied().collect();
    let next_wires: BTreeSet<WireInfo> = next.wires.iter().copied().collect();

    GraphDelta {
        added_wires: next_wires.difference(&prev_wires).copied().collect(),
        removed_wires: prev_wires.difference(&next_wires).copied().collect(),
        endpoints_changed: prev.endpoints != next.endpoints,
    }
}

/// Drop the node's own ALSA clients (`report.own_clients`) and "Midi
/// Through" from a snapshot — scene clutter, not studio topology. A wire
/// touching a dropped endpoint is dropped with it. The System client (0) is
/// always plumbing too, same rule the old local reader used.
pub fn without_plumbing(snapshot: &PatchGraphSnapshot, own_clients: &[i32]) -> PatchGraphSnapshot {
    let mut plumbing_ids: BTreeSet<i32> = own_clients.iter().copied().collect();
    plumbing_ids.insert(0);
    for ep in &snapshot.endpoints {
        if ep.client_name == "Midi Through" {
            plumbing_ids.insert(ep.client_id);
        }
    }
    let is_plumbing = |client_id: i32| plumbing_ids.contains(&client_id);

    let endpoints: Vec<EndpointInfo> =
        snapshot.endpoints.iter().filter(|e| !is_plumbing(e.client_id)).cloned().collect();
    let wires: Vec<WireInfo> = snapshot
        .wires
        .iter()
        .filter(|w| !is_plumbing(w.src.0) && !is_plumbing(w.dst.0))
        .copied()
        .collect();

    PatchGraphSnapshot { endpoints, wires, stale: snapshot.stale }
}

/// Convert one node's raw JSON report into the scene's (unfiltered) snapshot
/// shape — `without_plumbing` is a separate pass, same as the old reader's
/// `snapshot()` → `without_plumbing()` split. A wire whose `src`/`dst`
/// doesn't parse as `"client:port"` is dropped, not the whole report.
pub fn snapshot_from_report(report: &InventoryReport) -> PatchGraphSnapshot {
    let endpoints = report
        .endpoints
        .iter()
        .map(|e| EndpointInfo {
            client_id: e.client_id,
            port_id: e.port_id,
            client_name: e.client_name.clone(),
            port_name: e.port_name.clone(),
            is_source: e.is_source,
            is_sink: e.is_sink,
            events: e.events,
        })
        .collect();
    let mut wires: Vec<WireInfo> = report
        .wires
        .iter()
        .filter_map(|w| Some(WireInfo { src: parse_addr(&w.src)?, dst: parse_addr(&w.dst)? }))
        .collect();
    wires.sort();
    PatchGraphSnapshot { endpoints, wires, stale: report.stale }
}

/// Did the render port's event counter rise since the last poll? Pure core
/// of `patch_bay::apply_audio_inventory`'s traffic-pulse writer: a strictly
/// greater count is a send; equal (including both absent) or a decrease (the
/// port vanished and came back with a smaller counter) is not.
pub fn render_port_advanced(previous: Option<u64>, current: u64) -> bool {
    previous.is_some_and(|prev| current > prev)
}

// ── Node choice (pure; `docs/scenes/patchbay.md`) ───────────────────────────

/// `/run/audio`'s node-directory naming: the peer nick with `/` replaced by
/// `-` (`docs/audio-daemon.md` names the exact encoding as still-to-settle;
/// this is this client's side of it, mirroring `kaijutsu-audiod`'s own
/// `format!("audio/{hostname}")` node name).
pub fn node_dir_for_hostname(hostname: &str) -> String {
    format!("audio-{hostname}")
}

/// This machine's own preferred node directory, or `None` when the hostname
/// can't be read (same "report empty rather than a placeholder" stance as
/// `kaijutsu_audio_runtime::midi_presence::sink_host`).
pub fn local_preferred_node_dir() -> Option<String> {
    hostname::get().ok().and_then(|h| h.into_string().ok()).map(|h| node_dir_for_hostname(&h))
}

/// Pick which node directory to read this poll: the local host's own node
/// when it's listed, else the alphabetically first — `/run/audio` names no
/// ordering rule of its own, so "alphabetical" is this client's own
/// tie-break, stable across polls as long as the node set doesn't change.
/// Switching nodes with a key is a follow-up, not this slice.
pub fn choose_node_dir(node_dirs: &[String], preferred: Option<&str>) -> Option<String> {
    if let Some(want) = preferred
        && node_dirs.iter().any(|d| d == want)
    {
        return Some(want.to_string());
    }
    node_dirs.iter().min().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── InventoryReport parsing ──────────────────────────────────────────

    /// The exact fixture body from `docs/audio-daemon.md`'s projection
    /// contract, plus one field this parse has no struct member for
    /// (`revision`) to prove an unrecognized key is ignored, not a parse
    /// failure.
    fn fixture_json() -> &'static str {
        r#"{
            "node": "audio/moltar",
            "revision": 7,
            "observed_epoch_ns": 1788869439000000000,
            "received_epoch_ns": 1788869439100000000,
            "stale": false,
            "backend": "alsa",
            "state": "ready",
            "own_clients": [130, 131],
            "endpoints": [
                {"client_id": 24, "port_id": 0, "client_name": "JD-Xi", "port_name": "JD-Xi MIDI 1",
                 "address": "24:0", "is_source": true, "is_sink": true, "listening": true, "events": 1234,
                 "usb_id": "0582:0158"},
                {"client_id": 130, "port_id": 0, "client_name": "kaijutsu-audio", "port_name": "render",
                 "address": "130:0", "is_source": true, "is_sink": false, "listening": false, "events": 7}
            ],
            "wires": [{"src": "24:0", "dst": "130:0"}]
        }"#
    }

    #[test]
    fn a_report_with_an_unknown_field_parses_and_ignores_it() {
        let report: InventoryReport = serde_json::from_str(fixture_json()).expect("fixture parses");
        assert_eq!(report.node, "audio/moltar");
        assert!(!report.stale);
        assert_eq!(report.own_clients, vec![130, 131]);
        assert_eq!(report.endpoints.len(), 2);
        assert_eq!(report.wires.len(), 1);
    }

    #[test]
    fn snapshot_from_report_parses_endpoints_and_wires() {
        let report: InventoryReport = serde_json::from_str(fixture_json()).unwrap();
        let snapshot = snapshot_from_report(&report);
        assert_eq!(snapshot.endpoints.len(), 2);
        assert_eq!(snapshot.endpoints[0].client_name, "JD-Xi");
        assert_eq!(snapshot.endpoints[0].events, 1234);
        assert_eq!(snapshot.wires, vec![WireInfo { src: (24, 0), dst: (130, 0) }]);
        assert!(!snapshot.stale);
    }

    #[test]
    fn snapshot_from_report_carries_the_stale_flag_through() {
        let mut report: InventoryReport = serde_json::from_str(fixture_json()).unwrap();
        report.stale = true;
        let snapshot = snapshot_from_report(&report);
        assert!(snapshot.stale, "the app-side snapshot must expose stale, not swallow it");
    }

    #[test]
    fn a_wire_with_a_malformed_address_is_dropped_not_the_whole_report() {
        let report = InventoryReport {
            node: "audio/moltar".into(),
            stale: false,
            own_clients: Vec::new(),
            endpoints: Vec::new(),
            wires: vec![
                RawWire { src: "24:0".into(), dst: "not-an-address".into() },
                RawWire { src: "24:0".into(), dst: "130:0".into() },
            ],
        };
        let snapshot = snapshot_from_report(&report);
        assert_eq!(snapshot.wires, vec![WireInfo { src: (24, 0), dst: (130, 0) }]);
    }

    // ── without_plumbing (own_clients + Midi Through) ────────────────────

    fn endpoint(client_id: i32, port_id: i32, client_name: &str, port_name: &str) -> EndpointInfo {
        EndpointInfo {
            client_id,
            port_id,
            client_name: client_name.into(),
            port_name: port_name.into(),
            is_source: true,
            is_sink: false,
            events: 0,
        }
    }

    fn wire(src: (i32, i32), dst: (i32, i32)) -> WireInfo {
        WireInfo { src, dst }
    }

    #[test]
    fn without_plumbing_drops_own_clients_and_their_wires() {
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![
                endpoint(0, 0, "System", "Timer"),
                endpoint(130, 0, "kaijutsu-ear", "capture"),
                endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1"),
            ],
            wires: vec![wire((24, 0), (130, 0)), wire((0, 1), (130, 0))],
            stale: false,
        };
        let cleaned = without_plumbing(&snapshot, &[130]);
        assert_eq!(cleaned.endpoints, vec![endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1")]);
        assert!(cleaned.wires.is_empty(), "both wires touch a plumbing endpoint");
    }

    #[test]
    fn without_plumbing_leaves_the_render_endpoint_visible_when_its_id_is_not_own() {
        // The render port ("kaijutsu-audio") is real gear, not the node's
        // own scanning connection — it must survive filtering as long as its
        // client id isn't itself listed in `own_clients`.
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![
                endpoint(130, 0, "kaijutsu-audio", "render"),
                endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1"),
            ],
            wires: vec![wire((130, 0), (24, 0))],
            stale: false,
        };
        // own_clients names a DIFFERENT id — the node's internal scan client.
        let cleaned = without_plumbing(&snapshot, &[999]);
        assert_eq!(cleaned.endpoints.len(), 2);
        assert_eq!(cleaned.wires, vec![wire((130, 0), (24, 0))]);
    }

    #[test]
    fn without_plumbing_drops_midi_through_by_name() {
        let snapshot = PatchGraphSnapshot {
            endpoints: vec![
                endpoint(64, 0, "Midi Through", "Midi Through Port-0"),
                endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1"),
            ],
            wires: vec![wire((24, 0), (64, 0))],
            stale: false,
        };
        let cleaned = without_plumbing(&snapshot, &[]);
        assert_eq!(cleaned.endpoints, vec![endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1")]);
        assert!(cleaned.wires.is_empty());
    }

    // ── diff (unchanged behavior against the app-side copy) ──────────────

    #[test]
    fn diff_reports_added_and_removed_wires() {
        let prev = PatchGraphSnapshot { endpoints: Vec::new(), wires: vec![wire((14, 0), (128, 0))], stale: false };
        let next = PatchGraphSnapshot { endpoints: Vec::new(), wires: vec![wire((20, 0), (128, 0))], stale: false };
        let delta = diff(&prev, &next);
        assert_eq!(delta.added_wires, vec![wire((20, 0), (128, 0))]);
        assert_eq!(delta.removed_wires, vec![wire((14, 0), (128, 0))]);
        assert!(!delta.endpoints_changed);
        assert!(!delta.is_empty());
    }

    #[test]
    fn diff_ignores_a_stale_only_flip() {
        let prev = PatchGraphSnapshot { endpoints: Vec::new(), wires: vec![wire((14, 0), (128, 0))], stale: false };
        let next = PatchGraphSnapshot { stale: true, ..prev.clone() };
        assert!(
            diff(&prev, &next).is_empty(),
            "a staleness-only change is not a topology change; the caller re-dirties text for it separately"
        );
    }

    // ── render_port_advanced ──────────────────────────────────────────────

    #[test]
    fn render_port_advanced_fires_only_on_a_strict_increase() {
        assert!(render_port_advanced(Some(3), 4), "3 -> 4 is a send");
        assert!(!render_port_advanced(Some(4), 4), "equal counts is not a send");
        assert!(!render_port_advanced(Some(5), 4), "a decrease is not a send");
        assert!(!render_port_advanced(None, 0), "no previous reading is never a pulse");
        assert!(!render_port_advanced(None, 7), "first-ever reading is a baseline, not a pulse");
    }

    // ── node choice ────────────────────────────────────────────────────

    #[test]
    fn node_dir_for_hostname_replaces_the_slash() {
        assert_eq!(node_dir_for_hostname("moltar"), "audio-moltar");
    }

    #[test]
    fn choose_node_dir_prefers_the_local_host_when_listed() {
        let dirs = vec!["audio-moltar".to_string(), "audio-zorak".to_string()];
        assert_eq!(choose_node_dir(&dirs, Some("audio-zorak")), Some("audio-zorak".to_string()));
    }

    #[test]
    fn choose_node_dir_falls_back_to_alphabetically_first() {
        let dirs = vec!["audio-zorak".to_string(), "audio-moltar".to_string()];
        assert_eq!(choose_node_dir(&dirs, None), Some("audio-moltar".to_string()));
        assert_eq!(
            choose_node_dir(&dirs, Some("audio-nowhere")),
            Some("audio-moltar".to_string()),
            "an unlisted preference falls back the same as no preference"
        );
    }

    #[test]
    fn choose_node_dir_is_none_when_nothing_is_listed() {
        assert_eq!(choose_node_dir(&[], Some("audio-moltar")), None);
    }
}

//! Pure builder for the daemon's inventory report body
//! (`docs/audio-daemon.md` "One inventory owner") — the JSON
//! `reportAudioInventory` sends and the kernel projects, unchanged except for
//! `received_epoch_ns`/`stale`, at `/run/audio/<node-dir>/inventory.json`.
//!
//! Kept free of ALSA and threading so the two rules that matter — the
//! daemon's own plumbing clients (ear, exchange, patchview) go in
//! `own_clients` and the render client never does; each endpoint's `events`
//! comes from the right counter for its kind (an input's ear-observed count,
//! the render port's own DJ-sent count) — are testable without a sequencer.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::midi_in::ObservedPort;
use crate::patch_graph::PatchGraphSnapshot;

/// Minimum time between two reports, even when something changed on every
/// tick — a busy graph must not spam the wire.
const MIN_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum time between two reports while connected — a quiet node with
/// nothing to say must still refresh its counters so the app's traffic
/// pulse doesn't read a report as permanently frozen.
const MAX_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Is a fresh report due? `elapsed_since_last` is `None` before the first
/// report of a connection (always due). `changed` says whether the graph,
/// the render count or any input's event count moved since the last report.
pub(crate) fn report_due(changed: bool, elapsed_since_last: Option<Duration>) -> bool {
    match elapsed_since_last {
        None => true,
        Some(elapsed) => (changed && elapsed >= MIN_REPORT_INTERVAL) || elapsed >= MAX_REPORT_INTERVAL,
    }
}

/// Everything [`build_report`] needs beyond the bare ALSA graph enumeration.
pub(crate) struct ReportInputs<'a> {
    pub node: &'a str,
    pub revision: u64,
    pub observed_epoch_ns: u64,
    pub backend: &'a str,
    pub state: &'a str,
    /// Every client/port and subscription on the local seq graph —
    /// `PatchGraphReader::snapshot()`. Already carries the render port and
    /// this process's own plumbing ports; nothing is pre-filtered.
    pub graph: &'a PatchGraphSnapshot,
    /// The daemon's own plumbing client ids: ear, exchange, patchview.
    /// **Never** the render client. A negative id (an exchange client not
    /// yet lazily opened) is dropped rather than reported as a bare `-1`.
    pub own_clients: &'a [i32],
    /// The ear's own observed ports — the only source of `usb_id` and
    /// `listening` for an input endpoint. A port the graph sees but the ear
    /// doesn't track (the render port, an output-only device nobody
    /// subscribed to) gets neither.
    pub observed_ports: &'a [ObservedPort],
    /// Per-source MIDI event counts the ear has observed, keyed by
    /// `"client:port"` — every INPUT endpoint's `events` field.
    pub input_events: &'a BTreeMap<String, u64>,
    /// The render port's own address and the count of events the DJ has sent
    /// out it, when the DJ's ALSA sink is open. `None` before the sink opens
    /// (MIDI disabled, or not yet opened) — the render port then reports
    /// `events: 0` like any other address nothing has counted.
    pub render: Option<((i32, i32), u64)>,
}

/// Build the report body. Field order and names follow the contract in
/// `docs/audio-daemon.md` and `kaijutsu-app`'s `InventoryReport`/`RawEndpoint`/
/// `RawWire` exactly; `received_epoch_ns`/`stale` are always the daemon's
/// placeholder values (`0`/`false`) — only the kernel may set them.
pub(crate) fn build_report(inputs: &ReportInputs) -> serde_json::Value {
    let observed_by_address: BTreeMap<&str, &ObservedPort> = inputs
        .observed_ports
        .iter()
        .map(|p| (p.facts.address.as_str(), p))
        .collect();

    let endpoints: Vec<serde_json::Value> = inputs
        .graph
        .endpoints
        .iter()
        .map(|e| {
            let address = format!("{}:{}", e.client_id, e.port_id);
            let observed = observed_by_address.get(address.as_str());
            let events = match inputs.render {
                Some((addr, count)) if addr == (e.client_id, e.port_id) => count,
                _ => inputs.input_events.get(&address).copied().unwrap_or(0),
            };
            serde_json::json!({
                "client_id": e.client_id,
                "port_id": e.port_id,
                "client_name": e.client_name,
                "port_name": e.port_name,
                "address": address,
                "is_source": e.is_source,
                "is_sink": e.is_sink,
                "listening": observed.map(|p| p.listening).unwrap_or(false),
                "events": events,
                "usb_id": observed.and_then(|p| p.facts.usb_id.clone()),
            })
        })
        .collect();

    let wires: Vec<serde_json::Value> = inputs
        .graph
        .wires
        .iter()
        .map(|w| {
            serde_json::json!({
                "src": format!("{}:{}", w.src.0, w.src.1),
                "dst": format!("{}:{}", w.dst.0, w.dst.1),
            })
        })
        .collect();

    let own_clients: Vec<i32> = inputs.own_clients.iter().copied().filter(|&id| id >= 0).collect();

    serde_json::json!({
        "node": inputs.node,
        "revision": inputs.revision,
        "observed_epoch_ns": inputs.observed_epoch_ns,
        // Placeholders only the kernel may fill in
        // (`docs/audio-daemon.md` "One inventory owner").
        "received_epoch_ns": 0,
        "stale": false,
        "backend": inputs.backend,
        "state": inputs.state,
        "own_clients": own_clients,
        "endpoints": endpoints,
        "wires": wires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi_match::PortFacts;
    use crate::patch_graph::{EndpointInfo, WireInfo};

    fn endpoint(client_id: i32, port_id: i32, client_name: &str, port_name: &str, is_source: bool, is_sink: bool) -> EndpointInfo {
        EndpointInfo {
            client_id,
            port_id,
            client_name: client_name.into(),
            port_name: port_name.into(),
            is_source,
            is_sink,
        }
    }

    fn observed(address: &str, client_name: &str, usb_id: Option<&str>, listening: bool) -> ObservedPort {
        ObservedPort {
            facts: PortFacts {
                client_name: client_name.into(),
                port_name: "port".into(),
                address: address.into(),
                usb_id: usb_id.map(str::to_string),
            },
            readable: true,
            writable: false,
            listening,
            error: None,
        }
    }

    fn graph() -> PatchGraphSnapshot {
        PatchGraphSnapshot {
            endpoints: vec![
                endpoint(24, 0, "JD-Xi", "JD-Xi MIDI 1", true, true),
                endpoint(130, 0, "kaijutsu-ear", "capture", false, true),
                endpoint(131, 0, "kaijutsu-exchange", "exchange", true, true),
                endpoint(132, 0, "kaijutsu-patchview", "patchview", false, false),
                endpoint(140, 0, "kaijutsu-audio", "render", true, false),
            ],
            wires: vec![WireInfo { src: (24, 0), dst: (140, 0) }],
        }
    }

    #[test]
    fn own_clients_carries_ear_exchange_and_patchview_never_render() {
        let g = graph();
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[130, 131, 132],
            observed_ports: &[],
            input_events: &BTreeMap::new(),
            render: None,
        };
        let report = build_report(&inputs);
        let own = report["own_clients"].as_array().unwrap();
        assert_eq!(own.len(), 3);
        assert!(own.contains(&serde_json::json!(130)), "{report}");
        assert!(own.contains(&serde_json::json!(131)), "{report}");
        assert!(own.contains(&serde_json::json!(132)), "{report}");
        assert!(!own.contains(&serde_json::json!(140)), "the render client must never appear in own_clients: {report}");
    }

    #[test]
    fn a_never_opened_exchange_client_id_is_dropped_not_reported_as_negative_one() {
        let g = graph();
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[130, -1, 132],
            observed_ports: &[],
            input_events: &BTreeMap::new(),
            render: None,
        };
        let report = build_report(&inputs);
        let own = report["own_clients"].as_array().unwrap();
        assert_eq!(own, &vec![serde_json::json!(130), serde_json::json!(132)]);
    }

    #[test]
    fn render_endpoint_events_come_from_the_dj_counter_not_the_input_map() {
        let g = graph();
        let mut input_events = BTreeMap::new();
        // Deliberately poison the input map with an entry for the render
        // address — the render count must win regardless.
        input_events.insert("140:0".to_string(), 999u64);
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[130, 131, 132],
            observed_ports: &[],
            input_events: &input_events,
            render: Some(((140, 0), 7)),
        };
        let report = build_report(&inputs);
        let render = report["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["address"] == "140:0")
            .expect("render endpoint present");
        assert_eq!(render["events"], 7, "{report}");
    }

    #[test]
    fn an_input_endpoints_events_come_from_the_ear_observed_map() {
        let g = graph();
        let mut input_events = BTreeMap::new();
        input_events.insert("24:0".to_string(), 1234u64);
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[130, 131, 132],
            observed_ports: &[observed("24:0", "JD-Xi", Some("0582:0158"), true)],
            input_events: &input_events,
            render: Some(((140, 0), 7)),
        };
        let report = build_report(&inputs);
        let jdxi = report["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["address"] == "24:0")
            .expect("JD-Xi endpoint present");
        assert_eq!(jdxi["events"], 1234, "{report}");
        assert_eq!(jdxi["usb_id"], "0582:0158");
        assert_eq!(jdxi["listening"], true);
    }

    #[test]
    fn an_endpoint_the_ear_never_observed_gets_no_usb_id_and_is_not_listening() {
        let g = graph();
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[130, 131, 132],
            observed_ports: &[],
            input_events: &BTreeMap::new(),
            render: None,
        };
        let report = build_report(&inputs);
        let jdxi = report["endpoints"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["address"] == "24:0")
            .unwrap();
        assert_eq!(jdxi["usb_id"], serde_json::Value::Null);
        assert_eq!(jdxi["listening"], false);
        assert_eq!(jdxi["events"], 0);
    }

    #[test]
    fn wires_carry_through_as_address_strings() {
        let g = graph();
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[],
            observed_ports: &[],
            input_events: &BTreeMap::new(),
            render: None,
        };
        let report = build_report(&inputs);
        assert_eq!(
            report["wires"],
            serde_json::json!([{"src": "24:0", "dst": "140:0"}])
        );
    }

    // ── report_due cadence ────────────────────────────────────────────────

    #[test]
    fn the_first_report_of_a_connection_is_always_due() {
        assert!(report_due(false, None));
        assert!(report_due(true, None));
    }

    #[test]
    fn an_unchanged_graph_is_not_due_before_the_max_interval() {
        assert!(!report_due(false, Some(Duration::from_secs(1))));
        assert!(!report_due(false, Some(Duration::from_secs(9))));
    }

    #[test]
    fn an_unchanged_graph_is_due_once_the_max_interval_elapses() {
        assert!(report_due(false, Some(Duration::from_secs(10))));
    }

    #[test]
    fn a_change_is_rate_limited_to_the_min_interval() {
        assert!(!report_due(true, Some(Duration::from_millis(200))), "must not spam the wire");
        assert!(report_due(true, Some(Duration::from_secs(1))));
    }

    #[test]
    fn received_epoch_ns_and_stale_are_always_the_daemon_placeholders() {
        let g = graph();
        let inputs = ReportInputs {
            node: "audio/test",
            revision: 1,
            observed_epoch_ns: 1,
            backend: "alsa",
            state: "ready",
            graph: &g,
            own_clients: &[],
            observed_ports: &[],
            input_events: &BTreeMap::new(),
            render: None,
        };
        let report = build_report(&inputs);
        assert_eq!(report["received_epoch_ns"], 0, "only the kernel may stamp this: {report}");
        assert_eq!(report["stale"], false, "only the kernel may stamp this: {report}");
    }
}

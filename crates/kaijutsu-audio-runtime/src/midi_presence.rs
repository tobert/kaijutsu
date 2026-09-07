//! Match local MIDI topology against kernel profiles and report accepted
//! device names, addresses and presence. Matching is independent of the backend.

use std::collections::BTreeMap;


use crate::midi_match::{MatchedPort, PortFacts};

/// Where device profiles live. The durable half of a device; `/run/midi` (the
/// kernel's ephemeral half) is written by our reports.
/// Derived from the shared path builder rather than written out, so the app
/// cannot drift from the kernel's device tree when a root moves.
pub(crate) static DEVICES_DIR: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let one = kaijutsu_types::paths::midi_device_path("x");
    one.rsplit_once('/').expect("device path has a parent").0.to_string()
});

/// This sink's platform backend, as reported to the kernel. One line to
/// change when the CoreMIDI backend lands (`docs/midi-next.md` "Platform
/// backends": two native backends behind one trait).
#[cfg(target_os = "linux")]
pub(crate) const BACKEND: &str = "alsa";
#[cfg(not(target_os = "linux"))]
pub(crate) const BACKEND: &str = "none";

/// Ceiling on the profile listing fetch. The devices namespace is a flat
/// handful of documents; a runaway listing is a bug, not a big rig.
pub(crate) const MAX_PROFILES: u32 = 512;

/// What we call the machine this sink runs on, for `kj midi list`'s "live,
/// but WHERE?" column. Same source `main.rs` already uses to nickname a share
/// offering (the `hostname` crate), so a player sees one name for one box.
///
/// Display and provenance only — the kernel binds presence to the connection,
/// so a wrong or duplicated hostname can never erase another sink's records.
/// An unnameable host reports empty rather than a placeholder: "we don't know
/// where" is a fact, "localhost" would be a fiction.
pub(crate) fn sink_host() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
}

/// The live port picture, maintained by `midi_in`'s ear drain from ALSA
/// announce events. Keyed by backend address (`"client:port"` under ALSA) —
/// the one thing a departure event carries, since a vanished port's names go
/// with it.
#[derive(Debug, Default)]
pub struct MidiPortTopology {
    ports: BTreeMap<String, PortFacts>,
    /// Bumped on every real change. The reconciler compares it against what
    /// it last matched, so a quiet rig costs nothing per frame.
    revision: u64,
}

impl MidiPortTopology {
    pub fn port_up(&mut self, facts: PortFacts) {
        if self.ports.get(&facts.address) == Some(&facts) {
            return; // announce re-reporting an identical port: not a change
        }
        self.ports.insert(facts.address.clone(), facts);
        self.revision += 1;
    }

    pub fn port_down(&mut self, address: &str) {
        if self.ports.remove(address).is_some() {
            self.revision += 1;
        }
    }

    /// Every port under a departed client. ALSA normally emits a `PortExit`
    /// per port first; this is the belt-and-braces path (and the shape a
    /// future backend that only reports device-level departure will use).
    pub fn client_down(&mut self, client_id: i32) {
        let prefix = format!("{client_id}:");
        let gone: Vec<String> = self
            .ports
            .keys()
            .filter(|a| a.starts_with(&prefix))
            .cloned()
            .collect();
        if gone.is_empty() {
            return;
        }
        for address in gone {
            self.ports.remove(&address);
        }
        self.revision += 1;
    }

    pub fn ports(&self) -> Vec<PortFacts> {
        self.ports.values().cloned().collect()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// One presence report, ready for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceReport {
    pub device: String,
    pub present: bool,
    /// `(port display name, backend address)` pairs. Empty on an absence
    /// report — there are no ports to describe.
    pub ports: Vec<(String, String)>,
}

pub fn routes_from_match(
    matched: &BTreeMap<String, Vec<MatchedPort>>,
) -> BTreeMap<String, Vec<String>> {
    matched
        .iter()
        .map(|(device, ports)| {
            (
                device.clone(),
                ports.iter().map(|p| p.facts.address.clone()).collect(),
            )
        })
        .collect()
}

/// The pure diff between "what the kernel believes" and "what we just
/// matched". Only changes are reported: a steady rig is silent on the wire.
///
/// Kept free of Bevy and RPC so the unplug rule is testable on its own — a
/// device that drops out of the match set MUST produce a `present=false`
/// report, not an omission.
pub fn diff_presence(
    reported: &BTreeMap<String, bool>,
    matched: &BTreeMap<String, Vec<MatchedPort>>,
) -> Vec<PresenceReport> {
    let mut out = Vec::new();
    for (device, ports) in matched {
        if reported.get(device) == Some(&true) {
            continue; // already live in the kernel's picture
        }
        out.push(PresenceReport {
            device: device.clone(),
            present: true,
            ports: ports
                .iter()
                .map(|p| (p.facts.port_name.clone(), p.facts.address.clone()))
                .collect(),
        });
    }
    for (device, was_present) in reported {
        if *was_present && !matched.contains_key(device) {
            out.push(PresenceReport {
                device: device.clone(),
                present: false,
                ports: Vec::new(),
            });
        }
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(client: &str, name: &str, addr: &str) -> PortFacts {
        PortFacts {
            client_name: client.into(),
            port_name: name.into(),
            address: addr.into(),
            usb_id: None,
        }
    }

    fn matched(device: &str, ports: &[PortFacts]) -> BTreeMap<String, Vec<MatchedPort>> {
        let mut m = BTreeMap::new();
        m.insert(
            device.to_string(),
            ports
                .iter()
                .map(|f| MatchedPort { facts: f.clone(), role: None })
                .collect(),
        );
        m
    }

    // ── topology ──────────────────────────────────────────────────────────

    #[test]
    fn topology_tracks_arrival_and_departure() {
        let mut topo = MidiPortTopology::default();
        topo.port_up(facts("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0"));
        assert_eq!(topo.ports().len(), 1);
        let rev = topo.revision();

        // An identical re-announce is not a change (announce races the sweep).
        topo.port_up(facts("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0"));
        assert_eq!(topo.revision(), rev, "an identical port must not churn");

        topo.port_down("24:0");
        assert!(topo.ports().is_empty());
        assert!(topo.revision() > rev);
    }

    #[test]
    fn a_departing_client_takes_all_its_ports() {
        let mut topo = MidiPortTopology::default();
        topo.port_up(facts("KeyLab mkII 88", "KeyLab mkII 88 MIDI", "28:0"));
        topo.port_up(facts("KeyLab mkII 88", "KeyLab mkII 88 DAW", "28:1"));
        topo.port_up(facts("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0"));
        topo.client_down(28);
        assert_eq!(
            topo.ports().into_iter().map(|p| p.address).collect::<Vec<_>>(),
            vec!["24:0".to_string()]
        );
        // A client id that isn't a prefix-match must not take "280:0" with it.
        let mut topo = MidiPortTopology::default();
        topo.port_up(facts("A", "a", "280:0"));
        topo.client_down(28);
        assert_eq!(topo.ports().len(), 1);
    }

    #[test]
    fn an_unknown_departure_is_not_a_change() {
        let mut topo = MidiPortTopology::default();
        topo.port_down("99:0");
        topo.client_down(99);
        assert_eq!(topo.revision(), 0);
    }

    // ── the report diff ───────────────────────────────────────────────────

    #[test]
    fn a_newly_matched_device_is_reported_live_with_its_ports() {
        let reported = BTreeMap::new();
        let m = matched(
            "keystep-pro",
            &[facts("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0")],
        );
        let out = diff_presence(&reported, &m);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device, "keystep-pro");
        assert!(out[0].present);
        assert_eq!(
            out[0].ports,
            vec![("KeyStep Pro MIDI 1".to_string(), "24:0".to_string())]
        );
    }

    /// The steady state is silent: nothing changed, nothing crosses the wire.
    #[test]
    fn an_unchanged_picture_reports_nothing() {
        let mut reported = BTreeMap::new();
        reported.insert("keystep-pro".to_string(), true);
        let m = matched("keystep-pro", &[facts("KeyStep Pro", "MIDI 1", "24:0")]);
        assert!(diff_presence(&reported, &m).is_empty());
    }

    /// Unplug is a report, never an omission — the whole honesty rule.
    #[test]
    fn a_vanished_device_is_reported_absent() {
        let mut reported = BTreeMap::new();
        reported.insert("keystep-pro".to_string(), true);
        let out = diff_presence(&reported, &BTreeMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].device, "keystep-pro");
        assert!(!out[0].present);
        assert!(out[0].ports.is_empty(), "an absence describes no ports");
    }

    /// An already-absent device is not re-reported every frame.
    #[test]
    fn an_already_absent_device_is_not_reported_again() {
        let mut reported = BTreeMap::new();
        reported.insert("keystep-pro".to_string(), false);
        assert!(diff_presence(&reported, &BTreeMap::new()).is_empty());
    }

    /// Replug: absent → live produces a fresh live report.
    #[test]
    fn a_replugged_device_goes_live_again() {
        let mut reported = BTreeMap::new();
        reported.insert("keystep-pro".to_string(), false);
        let m = matched("keystep-pro", &[facts("KeyStep Pro", "MIDI 1", "24:0")]);
        let out = diff_presence(&reported, &m);
        assert_eq!(out.len(), 1);
        assert!(out[0].present);
    }

    #[test]
    fn simultaneous_arrival_and_departure_both_report() {
        let mut reported = BTreeMap::new();
        reported.insert("keystep-pro".to_string(), true);
        let m = matched("minibrute", &[facts("MiniBrute", "MIDI 1", "26:0")]);
        let out = diff_presence(&reported, &m);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].device, "keystep-pro");
        assert!(!out[0].present);
        assert_eq!(out[1].device, "minibrute");
        assert!(out[1].present);
    }

    // ── the local routing table (docs/midi-next.md slice 1 step 4) ────────

    /// The routing table names the ports of every matched device — this is
    /// what turns `kj midi send minibrute …` into bytes at a real port.
    #[test]
    fn routes_carry_every_matched_ports_address() {
        let mut m = matched("keystep-pro", &[facts("KeyStep Pro", "MIDI 1", "24:0")]);
        m.extend(matched(
            "keylab-88-mkii",
            &[
                facts("KeyLab mkII 88", "KeyLab mkII 88 MIDI", "28:0"),
                facts("KeyLab mkII 88", "KeyLab mkII 88 DAW", "28:1"),
            ],
        ));
        let routes = routes_from_match(&m);
        assert_eq!(routes["keystep-pro"], vec!["24:0".to_string()]);
        assert_eq!(
            routes["keylab-88-mkii"],
            vec!["28:0".to_string(), "28:1".to_string()],
            "match order preserved — slice 1 routes to the first, slice 2 picks by role"
        );
    }

    /// An unplugged device must stop being routable in the same instant: the
    /// table is the whole current picture, never merged with the last one.
    /// Its address may already belong to something else.
    #[test]
    fn an_unmatched_device_has_no_route_at_all() {
        assert!(routes_from_match(&BTreeMap::new()).is_empty());
        let routes = routes_from_match(&matched("minibrute", &[facts("MiniBrute", "MIDI 1", "26:0")]));
        assert!(!routes.contains_key("keystep-pro"), "routes: {routes:?}");
    }

    /// The "where" we report is the machine's real name or nothing at all —
    /// never a fabricated stand-in, which would put a machine that doesn't
    /// exist in front of every player reading `kj midi list`.
    #[test]
    fn the_sink_host_is_the_machines_name_or_nothing() {
        let host = sink_host();
        match hostname::get().ok().and_then(|h| h.into_string().ok()) {
            Some(real) => assert_eq!(host, real),
            None => assert!(host.is_empty(), "unknowable host must report empty"),
        }
        assert!(!host.contains('\0'));
    }

}

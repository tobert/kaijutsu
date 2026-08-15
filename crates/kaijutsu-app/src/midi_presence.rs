//! Sink-side MIDI presence: match the rig against device profiles and report
//! it to the kernel (`docs/midi-next.md` "Presence is sink-fed", slice 1
//! step 3). This is what makes "plug it in and kaijutsu just works" true —
//! `kj midi list` on any node shows what is live, because every sink reports
//! into the same store.
//!
//! ```text
//!   ALSA Announce ──► midi_in's ear thread ──► MidiPortTopology
//!   (already ours;      (PortUp/PortDown/           │ address → PortFacts
//!    nothing polls)      ClientDown)                ▼
//!                                          crate::midi_match (pure)
//!                                                   │ device + role
//!                                                   ▼
//!                              reportMidiPresence ──► kernel /run/midi/<device>
//! ```
//!
//! Three deliberate properties:
//!
//! - **The app matches, the kernel records.** Profiles reach us the way any
//!   config does (a VFS read of `/etc/midi/devices/<name>`); the *matching*
//!   is a pure function ([`crate::midi_match`]) so the CoreMIDI backend
//!   reuses it; only the outcome crosses the wire.
//! - **Unplug is first class.** A device that stops matching is reported
//!   `present=false`, not dropped quietly. Stale presence that lies is worse
//!   than no presence.
//! - **Reconnect re-reports everything.** The kernel's presence store is
//!   ephemeral, so a kernel restart (invisible at the connection layer beyond
//!   the drop) leaves it knowing nothing. On every fresh connection we forget
//!   what we think the kernel knows and state the whole picture again.
//!
//! Nothing here opens an ALSA handle: the ear's existing announce
//! subscription is the one hotplug watcher, and it feeds
//! [`MidiPortTopology`].

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy::tasks::IoTaskPool;

use crate::connection::actor_plugin::{RpcActor, RpcConnectionState};
use crate::midi_match::{DeviceMatch, MatchedPort, PortFacts, match_ports, parse_profile};

/// Where device profiles live. The durable half of a device; `/run/midi` (the
/// kernel's ephemeral half) is written by our reports.
const DEVICES_DIR: &str = "/etc/midi/devices";

/// This sink's platform backend, as reported to the kernel. One line to
/// change when the CoreMIDI backend lands (`docs/midi-next.md` "Platform
/// backends": two native backends behind one trait).
#[cfg(target_os = "linux")]
const BACKEND: &str = "alsa";
#[cfg(not(target_os = "linux"))]
const BACKEND: &str = "none";

/// Ceiling on the profile listing fetch. The devices namespace is a flat
/// handful of documents; a runaway listing is a bug, not a big rig.
const MAX_PROFILES: u32 = 512;

/// What we call the machine this sink runs on, for `kj midi list`'s "live,
/// but WHERE?" column. Same source `main.rs` already uses to nickname a share
/// offering (the `hostname` crate), so a player sees one name for one box.
///
/// Display and provenance only — the kernel binds presence to the connection,
/// so a wrong or duplicated hostname can never erase another sink's records.
/// An unnameable host reports empty rather than a placeholder: "we don't know
/// where" is a fact, "localhost" would be a fiction.
fn sink_host() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default()
}

/// The live port picture, maintained by `midi_in`'s ear drain from ALSA
/// announce events. Keyed by backend address (`"client:port"` under ALSA) —
/// the one thing a departure event carries, since a vanished port's names go
/// with it.
#[derive(Resource, Debug, Default)]
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

/// Matching + reporting state. `reported` is our belief about what the
/// *kernel* holds, which is exactly why it is cleared on every reconnect.
#[derive(Resource, Default)]
pub struct MidiPresenceState {
    profiles: Vec<DeviceMatch>,
    /// True once a profile fetch has completed (even with zero profiles —
    /// a kernel with no `/etc/midi` mount is a valid, matchable-nothing rig).
    profiles_loaded: bool,
    fetch_in_flight: bool,
    /// device → last state we told the kernel about.
    reported: BTreeMap<String, bool>,
    /// Topology revision the last reconcile ran against.
    matched_revision: Option<u64>,
    /// Previous frame's connection flag, for edge detection.
    was_connected: bool,
    /// Ambiguous-match warning latch, keyed by port address, so a permanent
    /// profile collision warns once per port rather than every frame.
    warned_ambiguous: BTreeMap<String, Vec<String>>,
    /// device → the backend addresses of its currently matched ports. The
    /// *local* half of what matching produces: `reported` is what we told the
    /// kernel, this is what THIS sink can actually reach — the routing table
    /// a device-addressed control cue (`kj midi send`) resolves through
    /// (`docs/midi-next.md` slice 1 step 4).
    ///
    /// Kept beside `reported` rather than derived from it because they
    /// genuinely differ: `reported` is a *diff* against the kernel's belief
    /// (silent when nothing changed), while routing needs the whole current
    /// picture every time.
    routes: BTreeMap<String, Vec<String>>,
}

impl MidiPresenceState {
    /// What we know about a device right now (for UI/debug; the kernel is
    /// the shared truth).
    // Deliberately unused for now — no UI/debug consumer exists yet
    // (docs/issues.md candidate: wire this up or drop it).
    #[allow(dead_code)]
    pub fn reported(&self) -> &BTreeMap<String, bool> {
        &self.reported
    }

    /// device → matched port addresses on THIS machine — how `kj midi send`
    /// finds a port (`crate::dj::thread::forward_midi_routes_to_dj` ships it
    /// to the DJ thread) and how an exchange finds one
    /// (`crate::midi_exchange::forward_routes`). Empty until the first match.
    pub fn routes(&self) -> &BTreeMap<String, Vec<String>> {
        &self.routes
    }

    /// Install a routing picture directly, for tests of the consumers that
    /// read it (the DJ forward, the exchange forward). Production writes go
    /// through `reconcile_presence` and nowhere else — this is a test seam,
    /// not a second writer.
    #[cfg(test)]
    pub fn set_routes_for_test(&mut self, routes: BTreeMap<String, Vec<String>>) {
        self.routes = routes;
    }
}

/// Profile-fetch results crossing back from the IO task.
#[derive(Resource)]
struct ProfileChannel {
    tx: crossbeam_channel::Sender<ProfileFetch>,
    rx: crossbeam_channel::Receiver<ProfileFetch>,
}

impl Default for ProfileChannel {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx }
    }
}

struct ProfileFetch {
    profiles: Vec<DeviceMatch>,
    /// Per-document parse failures. Loud, never silent: an unparseable
    /// profile means a device that can never be present, which is exactly
    /// the kind of quiet lie we refuse.
    errors: Vec<String>,
}

pub struct MidiPresencePlugin;

impl Plugin for MidiPresencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MidiPortTopology>()
            .init_resource::<MidiPresenceState>()
            .init_resource::<ProfileChannel>()
            .add_systems(
                Update,
                (fetch_profiles, drain_profiles, reconcile_presence).chain(),
            );
    }
}

/// Fetch the device profiles once per connection. A reconnect refetches: the
/// profiles may have been edited, and the kernel on the other end may not
/// even be the same one.
fn fetch_profiles(
    mut state: ResMut<MidiPresenceState>,
    channel: Res<ProfileChannel>,
    conn: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    let connected = conn.connected && actor.is_some();
    if connected != state.was_connected {
        state.was_connected = connected;
        if connected {
            // Fresh connection: the kernel's presence store is ephemeral, so
            // whatever it knew is gone. Forget our belief about it and
            // re-state everything after the profiles land.
            state.profiles_loaded = false;
            state.reported.clear();
            state.matched_revision = None;
        }
    }
    if !connected || state.profiles_loaded || state.fetch_in_flight {
        return;
    }

    let handle = actor.expect("connected implies an actor").handle.clone();
    let tx = channel.tx.clone();
    state.fetch_in_flight = true;
    IoTaskPool::get()
        .spawn(async move {
            let mut profiles = Vec::new();
            let mut errors = Vec::new();
            match handle.vfs_snapshot(DEVICES_DIR, 1, MAX_PROFILES).await {
                Ok(listing) => {
                    for child in listing.root.children.iter() {
                        if child.kind != kaijutsu_client::VfsFileType::File {
                            continue;
                        }
                        let path = format!("{DEVICES_DIR}/{}", child.name);
                        match handle.vfs_read_all(path.clone()).await {
                            Ok(bytes) => match String::from_utf8(bytes) {
                                Ok(doc) => match parse_profile(&child.name, &doc) {
                                    Ok(Some(m)) => profiles.push(m),
                                    // A profile with nothing to match on is
                                    // fine — it just can't be recognised.
                                    Ok(None) => {}
                                    Err(e) => errors.push(e),
                                },
                                Err(e) => errors.push(format!("{path}: not UTF-8: {e}")),
                            },
                            Err(e) => errors.push(format!("{path}: read failed: {e}")),
                        }
                    }
                }
                Err(e) => errors.push(format!("{DEVICES_DIR}: listing failed: {e}")),
            }
            let _ = tx.send(ProfileFetch { profiles, errors });
        })
        .detach();
}

fn drain_profiles(mut state: ResMut<MidiPresenceState>, channel: Res<ProfileChannel>) {
    while let Ok(fetch) = channel.rx.try_recv() {
        state.fetch_in_flight = false;
        for e in &fetch.errors {
            warn!("MIDI presence: {e}");
        }
        info!(
            "MIDI presence: {} device profile(s) loaded from {DEVICES_DIR}",
            fetch.profiles.len()
        );
        state.profiles = fetch.profiles;
        state.profiles_loaded = true;
        // The picture must be re-matched against the new profile set.
        state.matched_revision = None;
    }
}

/// Match the live topology against the profiles and ship whatever changed.
fn reconcile_presence(
    mut state: ResMut<MidiPresenceState>,
    topology: Res<MidiPortTopology>,
    conn: Res<RpcConnectionState>,
    actor: Option<Res<RpcActor>>,
) {
    if !conn.connected || actor.is_none() || !state.profiles_loaded {
        return;
    }
    if state.matched_revision == Some(topology.revision()) {
        return;
    }
    state.matched_revision = Some(topology.revision());

    let ports = topology.ports();
    let report = match_ports(&state.profiles, &ports);

    state.routes = routes_from_match(&report.devices);

    // An ambiguous port is refused, loudly and once: two profiles claiming
    // one port is an authoring bug, and guessing would put a wrong device
    // name in front of every player on the rig.
    for (facts, devices) in &report.ambiguous {
        if state.warned_ambiguous.get(&facts.address) != Some(devices) {
            warn!(
                "MIDI presence: port '{}' ({}) matches {} — refusing to guess; \
                 narrow the profiles' match strings",
                facts.port_name,
                facts.address,
                devices.join(" and ")
            );
            state
                .warned_ambiguous
                .insert(facts.address.clone(), devices.clone());
        }
    }
    for facts in &report.unmatched {
        // Not an error: unknown gear is unknown, and the ear captures from it
        // regardless. We simply never invent a device for it.
        debug!(
            "MIDI presence: no profile for '{}' ({})",
            facts.port_name, facts.address
        );
    }

    let reports = diff_presence(&state.reported, &report.devices);
    for r in &reports {
        state.reported.insert(r.device.clone(), r.present);
    }
    if reports.is_empty() {
        return;
    }

    let handle = actor.expect("checked above").handle.clone();
    let at_ns = epoch_ns_now();
    let host = sink_host();
    IoTaskPool::get()
        .spawn(async move {
            for r in reports {
                match handle
                    .report_midi_presence(
                        r.device.clone(),
                        r.present,
                        BACKEND,
                        r.ports.clone(),
                        at_ns,
                        host.clone(),
                    )
                    .await
                {
                    Ok(()) => info!(
                        "MIDI presence: reported {} {}",
                        r.device,
                        if r.present { "live" } else { "absent" }
                    ),
                    // Loud, and deliberately not retried in a loop. The two
                    // ways this fails heal on their own: a dropped connection
                    // heals at reconnect (which forgets our belief and
                    // re-states the whole picture), and a *permanent* refusal
                    // (a device key the kernel won't take) would hot-loop if
                    // we retried it. Anything else corrects at the next
                    // topology change.
                    Err(e) => warn!("MIDI presence: report for {} refused: {e}", r.device),
                }
            }
        })
        .detach();
}

/// The local routing table for device-addressed control cues (`kj midi
/// send`, `docs/midi-next.md` slice 1 step 4): device → its matched ports'
/// backend addresses, in match order.
///
/// The WHOLE current picture, computed fresh from each match and installed
/// outright — never merged into the previous one. A device that stopped
/// matching must stop being routable in the same instant: its address may
/// already belong to different gear, and a stale route would send at whatever
/// moved in. (Contrast [`diff_presence`], which is deliberately a *diff*: the
/// kernel wants changes, the router wants the state.)
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

fn epoch_ns_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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

    // ── the plugin's wiring ───────────────────────────────────────────────

    /// The ear's drain feeds the topology resource: `midi_in` owns the one
    /// hotplug watcher and this plugin owns the interpretation.
    #[test]
    fn the_plugin_registers_the_topology_resource() {
        let mut app = App::new();
        app.add_plugins(MidiPresencePlugin);
        assert!(app.world().get_resource::<MidiPortTopology>().is_some());
        assert!(app.world().get_resource::<MidiPresenceState>().is_some());
    }

    /// With no connection there is nothing to report and nothing to fetch —
    /// and crucially no panic from the missing `RpcActor`.
    #[test]
    fn a_disconnected_app_reconciles_nothing() {
        let mut app = App::new();
        app.add_plugins(MidiPresencePlugin)
            .init_resource::<RpcConnectionState>();
        app.world_mut()
            .resource_mut::<MidiPortTopology>()
            .port_up(facts("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0"));
        app.update();
        assert!(app.world().resource::<MidiPresenceState>().reported().is_empty());
    }
}

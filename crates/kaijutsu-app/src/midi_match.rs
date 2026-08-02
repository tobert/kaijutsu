//! Device-profile matching — **pure, platform-neutral, no ALSA, no Bevy**
//! (`docs/midi-next.md` "Presence is sink-fed", slice 1 step 3).
//!
//! The app holds the platform truth (port names, USB IDs); the kernel holds
//! the profiles. This module is the join: port facts in, device names out.
//! It is deliberately free of every backend detail so the CoreMIDI sink
//! reuses it unchanged — the whole point of "match strings stay
//! backend-neutral" (`docs/midi-next.md` "Platform backends"). ALSA-specific
//! enumeration stays in the Linux-gated code in `midi_in.rs`; this file never
//! learns what a sequencer client number is.
//!
//! ## What we match on (slice 1)
//!
//! The profile's `## Identity` JSON section carries a `match` object with:
//!
//! - `usb_ids` — `["1c75:0218"]`, `vendor:product`. **Strongest signal**: a
//!   USB ID is the device, whatever the OS decided to call its ports.
//! - `port_name_substrings` — display-name substrings (`"KeyStep Pro"`).
//! - `ports.<role>.port_name_substrings` — per-port sub-matches, which is how
//!   a two-port device (KeyLab: MIDI + DAW) names its roles.
//!
//! Both levels are matched case-insensitively against the port's *display*
//! identity (client name and port name), never against a backend handle. The
//! older `alsa_client_names` key is not read at all: it is superseded, and
//! silently honouring it would keep a backend-specific handle alive in the
//! matching path.
//!
//! ## Honesty rules
//!
//! - **No profile, no presence.** An unmatched port is simply not a presence
//!   entry. We never invent a device for it (the ear still captures from it —
//!   presence and capture are independent).
//! - **Ambiguity refuses.** If two different profiles tie for best match on
//!   one port, that port matches *nothing* and the caller is told which
//!   profiles collided. A confident wrong answer is worse than no answer.
//! - **Specificity wins.** Otherwise the longest matching substring takes the
//!   port, so `"KeyLab mkII 88 DAW"` beats a hypothetical `"KeyLab"`.
//!
//! ## Deferred: USB ID enrichment
//!
//! [`PortFacts::usb_id`] is the seam for `vendor:product`, and matching
//! already prefers it — but nothing fills it in on Linux yet (that needs a
//! sysfs walk from ALSA card → USB device). Until it does, matching runs on
//! name substrings, which is what slice 1 asked for. Filling the field in a
//! backend is the entire remaining change; this module needs none.

use std::collections::BTreeMap;

/// One port as a sink sees it. Backend-neutral by construction: `address` is
/// carried for logs and identity only — matching never reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortFacts {
    /// The owning client/device's display name ("KeyStep Pro").
    pub client_name: String,
    /// The port's display name ("KeyStep Pro MIDI 1").
    pub port_name: String,
    /// The backend's own handle: `"client:port"` under ALSA, a unique id
    /// under CoreMIDI. Opaque; carried to the kernel for logs.
    pub address: String,
    /// USB `vendor:product` (lowercase hex, e.g. `"1c75:0218"`) when the
    /// backend can supply it. **Deferred seam** — see the module docs.
    pub usb_id: Option<String>,
}

impl PortFacts {
    /// The haystack a name substring is tested against: client name and port
    /// name both, lowercased. Two fields rather than one concatenation so a
    /// substring can never match across the seam between them.
    fn haystacks(&self) -> [String; 2] {
        [self.client_name.to_lowercase(), self.port_name.to_lowercase()]
    }

    /// Length of the longest of `needles` that appears in either haystack, or
    /// `None` when none do. Length is the specificity score.
    fn best_substring(&self, needles: &[String]) -> Option<usize> {
        let hay = self.haystacks();
        needles
            .iter()
            .filter(|n| !n.is_empty())
            .filter_map(|n| {
                let low = n.to_lowercase();
                hay.iter().any(|h| h.contains(&low)).then_some(low.len())
            })
            .max()
    }
}

/// One declared port role inside a profile's `match.ports`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRoleMatch {
    pub role: String,
    pub port_name_substrings: Vec<String>,
}

/// The match half of one device profile's `## Identity` section — everything
/// this module needs, and nothing else from the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMatch {
    /// Profile key: the `<name>` in `/etc/midi/devices/<name>`, which is also
    /// the `/run/midi/<name>` presence key.
    pub device: String,
    /// `vendor:product` ids, lowercased.
    pub usb_ids: Vec<String>,
    /// Device-level display-name substrings.
    pub port_name_substrings: Vec<String>,
    /// Per-port role sub-matches.
    pub ports: Vec<PortRoleMatch>,
}

/// Score floor for a USB ID hit. A USB ID *is* the device: it outranks every
/// name match, however long, because names are an OS's opinion.
const USB_MATCH_SCORE: usize = 10_000;

impl DeviceMatch {
    /// This profile's score for a port, or `None` for no match at all.
    fn score(&self, facts: &PortFacts) -> Option<usize> {
        if let Some(id) = facts.usb_id.as_ref() {
            let id = id.to_lowercase();
            if self.usb_ids.iter().any(|u| u.to_lowercase() == id) {
                // Add the name score so a two-port device still ranks its own
                // ports sensibly; the floor keeps it above any name-only hit.
                let name = facts.best_substring(&self.port_name_substrings).unwrap_or(0);
                return Some(USB_MATCH_SCORE + name);
            }
        }
        // Device-level names, plus any per-role names (a profile may name only
        // its roles — e.g. a device whose ports share no common substring).
        let device_level = facts.best_substring(&self.port_name_substrings);
        let role_level = self
            .ports
            .iter()
            .filter_map(|p| facts.best_substring(&p.port_name_substrings))
            .max();
        device_level.into_iter().chain(role_level).max()
    }

    /// Which declared role this port is, when the profile names one. `None`
    /// is normal: a single-port device needn't declare roles at all.
    fn role_for(&self, facts: &PortFacts) -> Option<String> {
        self.ports
            .iter()
            .filter_map(|p| {
                facts
                    .best_substring(&p.port_name_substrings)
                    .map(|score| (score, p.role.clone()))
            })
            .max_by_key(|(score, _)| *score)
            .map(|(_, role)| role)
    }
}

/// One port that resolved to a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPort {
    pub facts: PortFacts,
    /// The profile's role name for this port, when it declares one.
    pub role: Option<String>,
}

/// What a whole sweep of ports resolved to. Every port lands in exactly one
/// of the three buckets — nothing is silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchReport {
    /// device name → the ports that matched it, in input order.
    pub devices: BTreeMap<String, Vec<MatchedPort>>,
    /// Ports no profile claims. **Not** presence entries: unknown gear is
    /// unknown, and the ear captures from it regardless.
    pub unmatched: Vec<PortFacts>,
    /// Ports two or more profiles tie for. Refused, with the colliding device
    /// names, so the operator can fix the profiles rather than chase a lie.
    pub ambiguous: Vec<(PortFacts, Vec<String>)>,
}

/// Resolve one port against the profile set.
///
/// Returns `Ok(None)` for "no profile claims this port", `Err(devices)` for a
/// refused tie, `Ok(Some(..))` for a confident match.
#[allow(clippy::type_complexity)]
pub fn match_port<'a>(
    profiles: &'a [DeviceMatch],
    facts: &PortFacts,
) -> Result<Option<(&'a DeviceMatch, Option<String>)>, Vec<String>> {
    let mut scored: Vec<(usize, &DeviceMatch)> = profiles
        .iter()
        .filter_map(|p| p.score(facts).map(|s| (s, p)))
        .collect();
    if scored.is_empty() {
        return Ok(None);
    }
    // Deterministic order: score desc, then device name asc.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.device.cmp(&b.1.device)));
    let best = scored[0].0;
    let tied: Vec<&DeviceMatch> = scored
        .iter()
        .take_while(|(s, _)| *s == best)
        .map(|(_, p)| *p)
        .collect();
    if tied.len() > 1 {
        return Err(tied.iter().map(|p| p.device.clone()).collect());
    }
    let winner = tied[0];
    Ok(Some((winner, winner.role_for(facts))))
}

/// Resolve a whole port sweep. This is what the presence layer calls on every
/// topology change; the result is diffed against what was last reported.
pub fn match_ports(profiles: &[DeviceMatch], ports: &[PortFacts]) -> MatchReport {
    let mut report = MatchReport::default();
    for facts in ports {
        match match_port(profiles, facts) {
            Ok(Some((profile, role))) => report
                .devices
                .entry(profile.device.clone())
                .or_default()
                .push(MatchedPort { facts: facts.clone(), role }),
            Ok(None) => report.unmatched.push(facts.clone()),
            Err(devices) => report.ambiguous.push((facts.clone(), devices)),
        }
    }
    report
}

/// Parse the `match` block out of a profile document's `## Identity` section.
///
/// - `Ok(Some(m))` — a usable match set.
/// - `Ok(None)` — a well-formed profile with nothing to match on (no
///   `usb_ids`, no `port_name_substrings`). A software-only or
///   still-being-drafted profile; not an error, just unmatchable.
/// - `Err(reason)` — the document is malformed. Loud on purpose: a profile we
///   cannot parse must be reported, never silently treated as "no match".
pub fn parse_profile(device: &str, doc: &str) -> Result<Option<DeviceMatch>, String> {
    let json = identity_json_block(doc)
        .ok_or_else(|| format!("{device}: no ```json block under a '## Identity' heading"))?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| format!("{device}: '## Identity' JSON is malformed: {e}"))?;

    // The device key is the VFS name; a `device` field disagreeing with it is
    // a document bug worth surfacing, but the path is authoritative.
    let m = match value.get("match") {
        Some(m) => m,
        None => return Ok(None),
    };

    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };

    let usb_ids: Vec<String> = strings(m.get("usb_ids"))
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect();
    let port_name_substrings = strings(m.get("port_name_substrings"));

    let mut ports: Vec<PortRoleMatch> = Vec::new();
    if let Some(obj) = m.get("ports").and_then(|p| p.as_object()) {
        for (role, spec) in obj {
            // `_note`-style commentary keys are documentation, not roles.
            if role.starts_with('_') {
                continue;
            }
            let subs = strings(spec.get("port_name_substrings"));
            if !subs.is_empty() {
                ports.push(PortRoleMatch { role: role.clone(), port_name_substrings: subs });
            }
        }
        ports.sort_by(|a, b| a.role.cmp(&b.role));
    }

    if usb_ids.is_empty() && port_name_substrings.is_empty() && ports.is_empty() {
        return Ok(None);
    }
    Ok(Some(DeviceMatch {
        device: device.to_string(),
        usb_ids,
        port_name_substrings,
        ports,
    }))
}

/// The first ```` ```json ```` fence appearing after the `## Identity`
/// heading. Profiles are prose+JSON hybrids with several fenced sections
/// (Identity / Settings / Capabilities); anchoring on the heading is what
/// keeps us from parsing the Settings block by accident.
fn identity_json_block(doc: &str) -> Option<String> {
    let mut lines = doc.lines();
    // Find the heading.
    lines.find(|l| {
        let t = l.trim();
        t.starts_with('#') && t.trim_start_matches('#').trim().eq_ignore_ascii_case("identity")
    })?;
    // Then the next json fence, stopping at the next heading so a section
    // without a block can't steal the following section's.
    let mut body = String::new();
    let mut in_block = false;
    for line in lines {
        let t = line.trim();
        if !in_block {
            if t.starts_with("##") {
                return None;
            }
            if t.starts_with("```") && t.trim_start_matches('`').trim().starts_with("json") {
                in_block = true;
            }
            continue;
        }
        if t.starts_with("```") {
            return Some(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real shipped profiles, so the parser is pinned against the schema
    // that actually ships rather than a fixture that can drift from it.
    const KEYSTEP: &str = include_str!("../../../assets/defaults/midi/devices/keystep-pro.md");
    const KEYLAB: &str = include_str!("../../../assets/defaults/midi/devices/keylab-88-mkii.md");
    const TIMIDITY: &str = include_str!("../../../assets/defaults/midi/devices/timidity.md");
    const MINIBRUTE: &str = include_str!("../../../assets/defaults/midi/devices/minibrute.md");

    fn port(client: &str, name: &str, addr: &str) -> PortFacts {
        PortFacts {
            client_name: client.into(),
            port_name: name.into(),
            address: addr.into(),
            usb_id: None,
        }
    }

    fn shipped() -> Vec<DeviceMatch> {
        [
            ("keystep-pro", KEYSTEP),
            ("keylab-88-mkii", KEYLAB),
            ("timidity", TIMIDITY),
            ("minibrute", MINIBRUTE),
        ]
        .into_iter()
        .filter_map(|(name, doc)| parse_profile(name, doc).expect("shipped profile parses"))
        .collect()
    }

    // ── parsing ───────────────────────────────────────────────────────────

    #[test]
    fn every_shipped_profile_parses_on_the_new_schema() {
        let profiles = shipped();
        let names: Vec<&str> = profiles.iter().map(|p| p.device.as_str()).collect();
        assert_eq!(
            names,
            vec!["keystep-pro", "keylab-88-mkii", "timidity", "minibrute"],
            "every shipped profile must carry usb_ids/port_name_substrings"
        );
        let ksp = &profiles[0];
        assert_eq!(ksp.usb_ids, vec!["1c75:0218"]);
        assert!(ksp.port_name_substrings.contains(&"KeyStep Pro".to_string()));
        // The KSP declares one port role.
        assert_eq!(ksp.ports.iter().map(|p| p.role.as_str()).collect::<Vec<_>>(), vec!["midi"]);
    }

    #[test]
    fn a_two_port_profile_keeps_both_roles() {
        let keylab = parse_profile("keylab-88-mkii", KEYLAB).unwrap().unwrap();
        let roles: Vec<&str> = keylab.ports.iter().map(|p| p.role.as_str()).collect();
        assert_eq!(roles, vec!["daw", "midi"]);
        assert_eq!(keylab.usb_ids, vec!["1c75:02cb"]);
    }

    /// `_note`-style keys inside `match.ports` are commentary, not roles.
    #[test]
    fn underscore_keys_are_not_roles() {
        let doc = r#"# X

## Identity

```json
{ "match": { "port_name_substrings": ["X"],
  "ports": { "_note": {"port_name_substrings": ["nope"]},
             "midi": {"port_name_substrings": ["X MIDI"]} } } }
```
"#;
        let m = parse_profile("x", doc).unwrap().unwrap();
        assert_eq!(m.ports.len(), 1);
        assert_eq!(m.ports[0].role, "midi");
    }

    /// A profile with nothing to match on is unmatchable, not an error —
    /// and it must never match everything.
    #[test]
    fn a_profile_with_no_match_strings_is_none() {
        let doc = "# X\n\n## Identity\n\n```json\n{ \"match\": { \"usb_ids\": [], \
                   \"port_name_substrings\": [] } }\n```\n";
        assert_eq!(parse_profile("x", doc).unwrap(), None);
        let no_match_key = "# X\n\n## Identity\n\n```json\n{ \"v\": 1 }\n```\n";
        assert_eq!(parse_profile("x", no_match_key).unwrap(), None);
    }

    /// Malformed JSON is loud. Treating it as "no match" would quietly turn a
    /// broken profile into a device that is never present.
    #[test]
    fn malformed_identity_json_is_an_error() {
        let doc = "# X\n\n## Identity\n\n```json\n{ not json\n```\n";
        assert!(parse_profile("x", doc).is_err());
        let missing = "# X\n\nno identity section here\n";
        assert!(parse_profile("x", missing).is_err());
    }

    /// The Identity block is anchored on its heading — never the Settings
    /// block that follows it.
    #[test]
    fn identity_parsing_does_not_steal_a_later_section() {
        let doc = r#"# X

## Identity

```json
{ "match": { "port_name_substrings": ["Right"] } }
```

## Settings

```json
{ "match": { "port_name_substrings": ["Wrong"] } }
```
"#;
        let m = parse_profile("x", doc).unwrap().unwrap();
        assert_eq!(m.port_name_substrings, vec!["Right".to_string()]);
    }

    /// A section with no JSON fence at all does not fall through to the next
    /// section's block.
    #[test]
    fn an_identity_section_with_no_block_is_an_error() {
        let doc = "# X\n\n## Identity\n\nprose only\n\n## Settings\n\n```json\n{}\n```\n";
        assert!(parse_profile("x", doc).is_err());
    }

    // ── matching ──────────────────────────────────────────────────────────

    #[test]
    fn the_live_bench_ports_match_their_profiles() {
        let profiles = shipped();
        // Observed on moltar 2026-08-02.
        let ksp = port("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0");
        let (m, role) = match_port(&profiles, &ksp).unwrap().unwrap();
        assert_eq!(m.device, "keystep-pro");
        assert_eq!(role.as_deref(), Some("midi"));

        let daw = port("KeyLab mkII 88", "KeyLab mkII 88 DAW", "28:1");
        let (m, role) = match_port(&profiles, &daw).unwrap().unwrap();
        assert_eq!(m.device, "keylab-88-mkii");
        assert_eq!(role.as_deref(), Some("daw"), "the longer substring wins the role");

        let midi = port("KeyLab mkII 88", "KeyLab mkII 88 MIDI", "28:0");
        let (m, role) = match_port(&profiles, &midi).unwrap().unwrap();
        assert_eq!(m.device, "keylab-88-mkii");
        assert_eq!(role.as_deref(), Some("midi"));
    }

    /// TiMidity's client number changes across restarts — matching is by name
    /// and must not care which number it landed on.
    #[test]
    fn a_software_synth_matches_by_name_regardless_of_address() {
        let profiles = shipped();
        for addr in ["128:0", "131:0"] {
            let p = port("TiMidity", "TiMidity port 0", addr);
            let (m, _) = match_port(&profiles, &p).unwrap().unwrap();
            assert_eq!(m.device, "timidity");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        let profiles = shipped();
        let p = port("keystep pro", "KEYSTEP PRO MIDI 1", "24:0");
        assert_eq!(
            match_port(&profiles, &p).unwrap().unwrap().0.device,
            "keystep-pro"
        );
    }

    /// The USB ID is the device: it wins over a longer, wronger name match.
    #[test]
    fn a_usb_id_outranks_a_name_match() {
        let profiles = vec![
            DeviceMatch {
                device: "by-usb".into(),
                usb_ids: vec!["1c75:0218".into()],
                port_name_substrings: vec![],
                ports: vec![],
            },
            DeviceMatch {
                device: "by-a-very-long-name".into(),
                usb_ids: vec![],
                port_name_substrings: vec!["Generic USB MIDI Interface".into()],
                ports: vec![],
            },
        ];
        let mut facts = port("Generic USB MIDI Interface", "port 0", "24:0");
        facts.usb_id = Some("1C75:0218".into()); // case-insensitive
        assert_eq!(
            match_port(&profiles, &facts).unwrap().unwrap().0.device,
            "by-usb"
        );
    }

    /// Nothing is invented for gear we have no profile for.
    #[test]
    fn an_unknown_port_matches_nothing() {
        let profiles = shipped();
        let p = port("Some Rando Synth", "Some Rando Synth MIDI 1", "30:0");
        assert_eq!(match_port(&profiles, &p).unwrap(), None);
    }

    /// A tie between two profiles refuses rather than picking one. A confident
    /// wrong device name is worse than no presence at all.
    #[test]
    fn an_ambiguous_port_is_refused_with_both_names() {
        let profiles = vec![
            DeviceMatch {
                device: "alpha".into(),
                usb_ids: vec![],
                port_name_substrings: vec!["Brute".into()],
                ports: vec![],
            },
            DeviceMatch {
                device: "beta".into(),
                usb_ids: vec![],
                port_name_substrings: vec!["Brute".into()],
                ports: vec![],
            },
        ];
        let p = port("MiniBrute", "MiniBrute MIDI 1", "24:0");
        let err = match_port(&profiles, &p).unwrap_err();
        assert_eq!(err, vec!["alpha".to_string(), "beta".to_string()]);
    }

    /// Specificity, not order, decides between overlapping profiles.
    #[test]
    fn the_longer_substring_wins() {
        let profiles = vec![
            DeviceMatch {
                device: "generic".into(),
                usb_ids: vec![],
                port_name_substrings: vec!["Key".into()],
                ports: vec![],
            },
            DeviceMatch {
                device: "specific".into(),
                usb_ids: vec![],
                port_name_substrings: vec!["KeyStep Pro".into()],
                ports: vec![],
            },
        ];
        let p = port("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0");
        assert_eq!(
            match_port(&profiles, &p).unwrap().unwrap().0.device,
            "specific"
        );
    }

    /// A substring must not match across the client-name/port-name seam.
    #[test]
    fn substrings_do_not_span_the_two_name_fields() {
        let profiles = vec![DeviceMatch {
            device: "x".into(),
            usb_ids: vec![],
            port_name_substrings: vec!["Pro MIDI".into()],
            ports: vec![],
        }];
        // "KeyStep Pro" + "MIDI 1" would contain "Pro MIDI" if concatenated.
        let p = port("KeyStep Pro", "MIDI 1", "24:0");
        assert_eq!(match_port(&profiles, &p).unwrap(), None);
    }

    // ── sweeps ────────────────────────────────────────────────────────────

    #[test]
    fn a_sweep_buckets_every_port_exactly_once() {
        let profiles = shipped();
        let ports = vec![
            port("KeyLab mkII 88", "KeyLab mkII 88 MIDI", "28:0"),
            port("KeyLab mkII 88", "KeyLab mkII 88 DAW", "28:1"),
            port("KeyStep Pro", "KeyStep Pro MIDI 1", "24:0"),
            port("Some Rando Synth", "port 0", "30:0"),
        ];
        let report = match_ports(&profiles, &ports);

        assert_eq!(report.devices.len(), 2);
        let keylab = &report.devices["keylab-88-mkii"];
        assert_eq!(keylab.len(), 2, "both ports of one device group together");
        assert_eq!(
            keylab.iter().filter_map(|p| p.role.clone()).collect::<Vec<_>>(),
            vec!["midi".to_string(), "daw".into()]
        );
        assert_eq!(report.devices["keystep-pro"].len(), 1);
        assert_eq!(report.unmatched.len(), 1);
        assert!(report.ambiguous.is_empty());
        // Nothing vanished.
        let bucketed: usize = report.devices.values().map(|v| v.len()).sum::<usize>()
            + report.unmatched.len()
            + report.ambiguous.len();
        assert_eq!(bucketed, ports.len());
    }

    #[test]
    fn an_empty_sweep_reports_nothing_live() {
        let report = match_ports(&shipped(), &[]);
        assert!(report.devices.is_empty());
        assert!(report.unmatched.is_empty());
    }
}

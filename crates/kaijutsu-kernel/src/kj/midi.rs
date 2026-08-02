//! `kj midi` — read the CRDT-owned MIDI device profile library.
//!
//! Device profiles live at `/etc/midi/devices/<name>` on the same CRDT-native
//! backend that owns `/etc/rc` and `/etc/config`
//! (`docs/config-crdt-ownership.md`): the kernel is the sole owner, no host
//! file, no write-through. Embedded seeds (`assets/defaults/midi/devices/*.md`,
//! `crate::midi_seed`) bootstrap a fresh kernel once — see `docs/midi-next.md`
//! "Storage and identity" (slice 1 step 2).
//!
//! Read-only for now: `list` enumerates the devices tree, `show` prints one
//! profile document. Write verbs (`kj midi send`/`cc`/`identify`/`pull`) are
//! later slices in `docs/midi-next.md` — this noun exists so building it now
//! doesn't fragment `kj` discovery later (the `kj audio`/`kj transport`
//! precedent this file follows).
//!
//! ## Presence (slice 1 step 3)
//!
//! Both verbs carry the sink-fed presence column
//! (`docs/midi-next.md` "Presence is sink-fed"). Three states, and the third
//! is load-bearing:
//!
//! - **live** — some sink reported this profile matched a plugged-in device.
//! - **absent** — some sink reported it *gone* (an unplug is a first-class
//!   report, not an omission).
//! - **unknown** — nobody has told this kernel anything. A fresh kernel says
//!   this about everything, because presence is ephemeral: a restart with no
//!   sinks connected genuinely knows nothing. Stale presence that lies is
//!   worse than no presence, so we never soften unknown into absent.
//!
//! The column reads [`crate::midi_presence::MidiPresenceStore`] — the same
//! state the read-only `/run/midi/<device>` view renders for kai and kaish.
//! Devices with no profile are never invented: an unmatched port simply has
//! no presence entry (the ear still captures from it regardless).

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths::{MIDI_ROOT, midi_device_path};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};

#[derive(Parser, Debug)]
#[command(
    name = "midi",
    about = "CRDT-owned MIDI device profiles at /etc/midi/devices/<name>",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct MidiArgs {
    #[command(subcommand)]
    command: MidiCommand,
}

#[derive(Subcommand, Debug)]
enum MidiCommand {
    /// List known device profiles (name + title pulled from the doc).
    #[command(alias = "ls")]
    List {
        /// Emit a JSON array of {name, title} objects instead of a labelled view
        #[arg(long)]
        json: bool,
    },
    /// Print one device's profile document.
    #[command(alias = "cat")]
    Show {
        /// Device name (e.g. minibrute) or full /etc/midi/devices path
        name: String,
        /// Emit a JSON object instead of a labelled view
        #[arg(long)]
        json: bool,
        /// Emit exactly the stored document — no path/length header
        #[arg(long, conflicts_with = "json")]
        raw: bool,
    },
}

/// The `/etc/midi/devices` directory path.
fn devices_dir() -> String {
    format!("{MIDI_ROOT}/devices")
}

/// Canonicalize a user-supplied device arg to `/etc/midi/devices/<name>`.
/// Accepts a bare name (`minibrute`) or an already-full path. Rejects nested
/// paths and parent escapes — the devices namespace is flat, one document per
/// device (a future rc-style bucket widens the *reader*, not this grammar,
/// since a bucket's files still hang directly under `/etc/midi/devices/<name>/`,
/// not under a per-device leaf this canonicalizer would need to parse).
fn midi_device_canonical(name: &str) -> Result<String, String> {
    let dir = devices_dir();
    let trimmed = name.trim();
    let bare = trimmed
        .strip_prefix(&format!("{dir}/"))
        .unwrap_or(trimmed)
        .trim_matches('/');
    if bare.is_empty() {
        return Err("missing device name (e.g. minibrute)".to_string());
    }
    if bare.contains('/') || bare == ".." || bare == "." {
        return Err(format!(
            "invalid device name '{name}': {dir} is a flat namespace (one document per device)"
        ));
    }
    Ok(midi_device_path(bare))
}

/// The three presence states, rendered. `unknown` is never softened into
/// `absent`: "nobody told us" and "a sink watched it leave" are different
/// facts, and collapsing them would make a restarted kernel lie.
fn presence_label(record: Option<&crate::midi_presence::MidiPresenceRecord>) -> &'static str {
    match record {
        Some(r) if r.present => "live",
        Some(_) => "absent",
        None => "unknown",
    }
}

/// Placeholder title rendered for a directory entry under
/// `/etc/midi/devices` — the shape a future rc-style bucket device will take
/// (`docs/midi-next.md` "The core split"). Today's reader only understands a
/// single-file leaf profile; a bucket directory must render a visible row
/// instead of silently vanishing from an `is_file()` filter, so a future
/// bucket device is a "not built yet" row, never an invisible one.
const BUCKET_PLACEHOLDER_TITLE: &str = "(bucket device — kj midi bucket support not built yet)";

/// The device's display title: its first non-empty line, with a leading
/// markdown `#`/`##`/etc. stripped (every shipped profile opens with a `# …
/// device profile` heading — `docs/midi-next.md`'s prose+JSON hybrid). Falls
/// back to `(untitled)` rather than an empty string so `list` never silently
/// drops a row for a malformed document.
fn doc_title(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string())
}

impl KjDispatcher {
    pub(crate) async fn dispatch_midi(&self, argv: &[String], _caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<MidiArgs>();
        }
        let parsed = match MidiArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj midi: {e}"));
            }
        };
        match parsed.command {
            MidiCommand::List { json } => self.midi_list(json).await,
            MidiCommand::Show { name, json, raw } => self.midi_show(&name, json, raw).await,
        }
    }

    async fn midi_list(&self, json: bool) -> KjResult {
        use crate::vfs::{VfsError, VfsOps};
        let vfs = self.kernel().vfs();
        let dir = devices_dir();
        let entries = match vfs.readdir(std::path::Path::new(&dir)).await {
            Ok(e) => e,
            // Absent (no mount, nothing seeded yet) reads as an empty listing,
            // not an error — a kernel that never mounted /etc/midi still
            // answers `kj midi list` truthfully with nothing.
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => Vec::new(),
            Err(e) => return KjResult::Err(format!("kj midi list: readdir {dir}: {e}")),
        };

        let presence = self.kernel().midi_presence();
        // (name, title, presence label, record, kind) — presence is a
        // *separate* lookup per device, never derived from the document: the
        // profile is durable knowledge, presence is a live report about it.
        // `kind` distinguishes a real single-file profile ("profile") from a
        // directory entry this reader can't parse yet ("bucket") — a future
        // rc-style bucket device must show up as a visible, labelled row, not
        // vanish behind a file-only filter.
        let mut rows: Vec<(String, String, &'static str, Option<_>, &'static str)> = Vec::new();
        for entry in entries {
            use crate::vfs::FileType;
            let (title, kind) = match entry.kind {
                FileType::File => {
                    let path = midi_device_path(&entry.name);
                    let title = match vfs.read_all(std::path::Path::new(&path)).await {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(s) => doc_title(&s),
                            Err(e) => {
                                return KjResult::Err(format!(
                                    "kj midi list: '{path}' is not valid UTF-8: {e}"
                                ));
                            }
                        },
                        Err(e) => return KjResult::Err(format!("kj midi list: read {path}: {e}")),
                    };
                    (title, "profile")
                }
                FileType::Directory => (BUCKET_PLACEHOLDER_TITLE.to_string(), "bucket"),
                // Not a shape any current or planned seed uses under this
                // tree; skip rather than guess at a title.
                FileType::Symlink => continue,
            };
            let record = presence.get(&entry.name);
            let label = presence_label(record.as_ref());
            rows.push((entry.name, title, label, record, kind));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let data = serde_json::Value::Array(
            rows.iter()
                .map(|(name, title, label, record, kind)| {
                    serde_json::json!({
                        "name": name,
                        "title": title,
                        "presence": label,
                        // Null rather than 0 for unknown: a missing observation
                        // has no timestamp, and an invented one would read as
                        // "observed at the epoch".
                        "at": record.as_ref().map(|r| r.at_ns),
                        "backend": record.as_ref().map(|r| r.backend.clone()),
                        // WHERE the report came from — the whole point of a
                        // rig-wide store. Null for unknown (nobody reported)
                        // and for a sink too old to say; never a placeholder
                        // hostname, which would read as a real machine.
                        "host": record
                            .as_ref()
                            .map(|r| r.sink.host.clone())
                            .filter(|h| !h.is_empty()),
                        "kind": kind,
                    })
                })
                .collect(),
        );
        if json {
            return KjResult::ok_with_data(data.to_string(), data);
        }
        if rows.is_empty() {
            return KjResult::ok_with_data("(no device profiles)".to_string(), data);
        }
        let width = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
        let pwidth = rows.iter().map(|(_, _, p, ..)| p.len()).max().unwrap_or(0);
        // The "where" column, and only when somebody actually answered it: on
        // a rig nobody has reported into (or one whose sinks are too old to
        // name themselves) the column is pure blank padding, so it isn't
        // drawn at all.
        let host_of = |record: &Option<crate::midi_presence::MidiPresenceRecord>| -> String {
            record
                .as_ref()
                .map(|r| r.sink.host.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_default()
        };
        let hwidth = rows
            .iter()
            .map(|(_, _, _, record, _)| host_of(record).len())
            .max()
            .unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|(name, title, label, record, _)| {
                if hwidth == 0 {
                    return format!("  {name:<width$}  {label:<pwidth$}  {title}");
                }
                let host = host_of(record);
                format!("  {name:<width$}  {label:<pwidth$}  {host:<hwidth$}  {title}")
            })
            .collect();
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    async fn midi_show(&self, name: &str, json: bool, raw: bool) -> KjResult {
        use crate::vfs::{VfsError, VfsOps};
        let canonical = match midi_device_canonical(name) {
            Ok(c) => c,
            Err(e) => return KjResult::Err(format!("kj midi show: {e}")),
        };
        let vfs = self.kernel().vfs();
        let content = match vfs.read_all(std::path::Path::new(&canonical)).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    return KjResult::Err(format!(
                        "kj midi show: '{canonical}' is not valid UTF-8: {e}"
                    ));
                }
            },
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
                return KjResult::Err(format!(
                    "kj midi show: unknown device '{name}' (no profile at {canonical})"
                ));
            }
            Err(e) => return KjResult::Err(format!("kj midi show: '{canonical}': {e}")),
        };

        if raw {
            // Exactly the stored document — no header — so it round-trips
            // through a future `kj midi set`/`edit` the way `kj config show
            // --raw` does today.
            return KjResult::ok(content);
        }

        let bare = canonical.rsplit('/').next().unwrap_or(&canonical);
        // Presence rides alongside the document, never inside it: the profile
        // is durable knowledge, the presence record is an ephemeral live
        // report keyed by the same device name (`/run/midi/<device>`).
        let presence = self.kernel().midi_presence().get(bare);
        let label = presence_label(presence.as_ref());
        let record = serde_json::json!({
            "path": canonical,
            "name": bare,
            "content_length": content.len(),
            "content": content,
            "presence": label,
            "presence_record": presence.as_ref().map(|r| r.to_json()),
        });
        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }
        let ports = presence
            .as_ref()
            .filter(|r| r.present)
            .map(|r| {
                r.ports
                    .iter()
                    .map(|p| format!("{} ({})", p.name, p.address))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!("ports:   {s}\n"))
            .unwrap_or_default();
        // Which sink saw it — the answer to "live, but WHERE?" for a rig
        // spread across machines. Omitted rather than faked when unknown.
        let host = presence
            .as_ref()
            .map(|r| r.sink.host.clone())
            .filter(|h| !h.is_empty())
            .map(|h| format!("host:    {h}\n"))
            .unwrap_or_default();
        let out = format!(
            "path:    {canonical}\nlength:  {} bytes\npresent: {label}\n{host}{ports}\n{content}\n",
            content.len(),
        );
        KjResult::ok_typed_with_data(out, ContentType::Markdown, record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::KjResult;
    use crate::kj::test_helpers::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn canonical_accepts_bare_and_full_rejects_nesting() {
        assert_eq!(
            midi_device_canonical("minibrute").unwrap(),
            "/etc/midi/devices/minibrute"
        );
        assert_eq!(
            midi_device_canonical("/etc/midi/devices/timidity").unwrap(),
            "/etc/midi/devices/timidity"
        );
        assert!(midi_device_canonical("sub/device").is_err());
        assert!(midi_device_canonical("/etc/midi/devices/a/b").is_err());
        assert!(midi_device_canonical("").is_err());
        assert!(midi_device_canonical("..").is_err());
    }

    #[test]
    fn doc_title_strips_heading_and_falls_back() {
        assert_eq!(
            doc_title("# Arturia MiniBrute (original) — device profile\n\nbody"),
            "Arturia MiniBrute (original) — device profile"
        );
        assert_eq!(doc_title("\n\n   \n"), "(untitled)");
        assert_eq!(doc_title("plain first line\nmore"), "plain first line");
    }

    /// A fresh kernel (the real CRDT-native `/etc/midi` mount, seeded from
    /// embedded defaults) already carries the shipped device profiles — no
    /// separate bootstrap step needed by callers.
    #[tokio::test]
    async fn fresh_kernel_seeds_midi_devices_into_the_vfs() {
        let d = test_dispatcher_crdt_rc().await;
        use crate::vfs::VfsOps;
        let names: Vec<_> = d
            .kernel()
            .vfs()
            .readdir(std::path::Path::new("/etc/midi/devices"))
            .await
            .expect("readdir /etc/midi/devices")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"minibrute".to_string()), "names: {names:?}");
        assert!(names.contains(&"timidity".to_string()), "names: {names:?}");
        assert!(names.contains(&"keystep-pro".to_string()), "names: {names:?}");
        assert!(
            names.contains(&"keylab-88-mkii".to_string()),
            "names: {names:?}"
        );
    }

    #[tokio::test]
    async fn list_shows_all_four_shipped_devices_with_titles() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                let title_contains = |name: &str, needle: &str| {
                    let row = arr
                        .iter()
                        .find(|row| row["name"] == name)
                        .unwrap_or_else(|| panic!("{name} row present"));
                    assert!(
                        row["title"].as_str().is_some_and(|t| t.contains(needle)),
                        "{name} title: {row:?}"
                    );
                    assert_eq!(row["kind"], "profile", "{name} kind: {row:?}");
                };
                title_contains("minibrute", "MiniBrute");
                title_contains("timidity", "TiMidity");
                title_contains("keystep-pro", "KeyStep Pro");
                title_contains("keylab-88-mkii", "KeyLab mkII 88");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_non_json_renders_a_labelled_line_per_device() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                assert!(message.contains("minibrute"), "message: {message}");
                assert!(message.contains("timidity"), "message: {message}");
                assert!(message.contains("keystep-pro"), "message: {message}");
                assert!(message.contains("keylab-88-mkii"), "message: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn show_returns_the_seeded_document_content() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("minibrute"), s("--json")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["path"].as_str(), Some("/etc/midi/devices/minibrute"));
                let content = v["content"].as_str().expect("content present");
                assert!(content.contains("MiniBrute"), "content: {content}");
                assert!(content.contains("\"device\": \"minibrute\""), "content: {content}");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn show_raw_emits_exactly_the_stored_document() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("timidity"), s("--raw")], &c)
            .await;
        match result {
            KjResult::Ok { message, .. } => {
                assert!(message.starts_with("# TiMidity"), "message: {message}");
                // No decoration header leaked into the raw body.
                assert!(!message.starts_with("path:"), "message: {message}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// `kj midi show` of an unknown device fails loud, not silently — no
    /// empty-content fallback (the house crash-over-corruption stance).
    #[tokio::test]
    async fn show_unknown_device_errors_loudly() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("nonesuch")], &c)
            .await;
        match result {
            KjResult::Err(msg) => {
                assert!(msg.contains("unknown device"), "msg: {msg}");
                assert!(msg.contains("nonesuch"), "msg: {msg}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A device name that tries to escape the flat namespace is rejected at
    /// the grammar, not turned into a confusing VFS error.
    #[tokio::test]
    async fn show_rejects_nested_or_escaping_names() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("midi"), s("show"), s("../../etc/passwd")], &c)
            .await;
        match result {
            KjResult::Err(msg) => assert!(msg.contains("flat namespace"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// A directory under `/etc/midi/devices` (the shape a future rc-style
    /// bucket device will take, `docs/midi-next.md` "The core split") must
    /// render as a visible, clearly-labelled row — not silently vanish behind
    /// the old `is_file()` filter. Built through the real CRDT-native
    /// `/etc/midi` mount (same fixture `fresh_kernel_seeds_midi_devices_into_the_vfs`
    /// uses): `ConfigCrdtFs` synthesizes directories from descendant paths
    /// (see `readdir_synthesizes_virtual_directories` in
    /// `runtime::config_crdt_fs`), so writing a leaf file under
    /// `/etc/midi/devices/<bucket>/...` is enough to make `<bucket>` itself
    /// appear as a real `FileType::Directory` readdir entry — no fixture
    /// workaround needed.
    #[tokio::test]
    async fn list_renders_a_placeholder_row_for_a_bucket_directory() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/etc/midi/devices/future-bucket/S00-notes.md"),
                b"stub rc-style bucket file, not a seed",
            )
            .await
            .expect("write nested bucket file");

        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                let bucket = arr
                    .iter()
                    .find(|row| row["name"] == "future-bucket")
                    .expect("future-bucket row present");
                assert_eq!(bucket["kind"], "bucket", "row: {bucket:?}");
                assert_eq!(
                    bucket["title"],
                    BUCKET_PLACEHOLDER_TITLE,
                    "row: {bucket:?}"
                );
                // The real profiles are still there alongside it.
                assert!(
                    arr.iter().any(|row| row["name"] == "minibrute"),
                    "arr: {arr:?}"
                );
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The human (non-JSON) view carries the same placeholder — a bucket
    /// device is visible there too, not just in `--json`.
    #[tokio::test]
    async fn list_non_json_renders_the_bucket_placeholder_too() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .vfs()
            .write_all(
                std::path::Path::new("/etc/midi/devices/future-bucket/S00-notes.md"),
                b"stub rc-style bucket file, not a seed",
            )
            .await
            .expect("write nested bucket file");

        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                let line = message
                    .lines()
                    .find(|l| l.contains("future-bucket"))
                    .expect("future-bucket row");
                assert!(
                    line.contains("bucket device — kj midi bucket support not built yet"),
                    "line: {line}"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    // ── presence (docs/midi-next.md "Presence is sink-fed") ───────────────

    use crate::midi_presence::{MidiPortFact, MidiPresenceRecord, SinkAttribution};

    fn sink_report(device: &str, present: bool, at_ns: u64) -> MidiPresenceRecord {
        sink_report_from(device, present, at_ns, "moltar")
    }

    fn sink_report_from(
        device: &str,
        present: bool,
        at_ns: u64,
        host: &str,
    ) -> MidiPresenceRecord {
        MidiPresenceRecord::from_sink(
            device,
            present,
            "alsa",
            if present {
                vec![MidiPortFact {
                    name: "MiniBrute MIDI 1".into(),
                    address: "24:0".into(),
                }]
            } else {
                vec![]
            },
            at_ns,
            SinkAttribution::new(kaijutsu_types::SessionId::new(), host),
        )
    }

    /// A kernel nobody has reported to says **unknown** for every profile —
    /// never "absent". This is the fresh-kernel/restart case: presence is
    /// ephemeral, so silence means we don't know, not that nothing is there.
    #[tokio::test]
    async fn list_presence_is_unknown_until_a_sink_reports() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                for row in v.as_array().expect("array") {
                    assert_eq!(row["presence"], "unknown", "row: {row}");
                    assert!(row["at"].is_null(), "row: {row}");
                }
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The whole point of step 3: a sink report turns the column live, and an
    /// unplug turns it absent. Both are visible to `kj midi list` on any node.
    #[tokio::test]
    async fn list_presence_follows_sink_reports_through_plug_and_unplug() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let presence = d.kernel().midi_presence().clone();

        presence.record(sink_report("minibrute", true, 1_000)).unwrap();
        let row = |v: &serde_json::Value, name: &str| -> serde_json::Value {
            v.as_array()
                .expect("array")
                .iter()
                .find(|r| r["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("row {name} present"))
        };

        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        let KjResult::Ok { data: Some(v), .. } = result else {
            panic!("expected Ok with data");
        };
        assert_eq!(row(&v, "minibrute")["presence"], "live");
        assert_eq!(row(&v, "minibrute")["at"], 1_000);
        assert_eq!(row(&v, "minibrute")["backend"], "alsa");
        // An un-reported neighbour stays unknown — presence is per device.
        assert_eq!(row(&v, "timidity")["presence"], "unknown");

        // Unplug: the report flips the column, it does not vanish.
        presence.record(sink_report("minibrute", false, 2_000)).unwrap();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        let KjResult::Ok { data: Some(v), .. } = result else {
            panic!("expected Ok with data");
        };
        assert_eq!(row(&v, "minibrute")["presence"], "absent");
        assert_eq!(row(&v, "minibrute")["at"], 2_000);
    }

    /// The human view carries the column too (this is what a player reads).
    #[tokio::test]
    async fn list_non_json_renders_the_presence_column() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 1))
            .unwrap();

        let result = d.dispatch(&[s("midi"), s("list")], &c).await;
        match result {
            KjResult::Ok { message, .. } => {
                let line = message
                    .lines()
                    .find(|l| l.contains("minibrute"))
                    .expect("minibrute row");
                assert!(line.contains("live"), "line: {line}");
                let other = message
                    .lines()
                    .find(|l| l.contains("timidity"))
                    .expect("timidity row");
                assert!(other.contains("unknown"), "line: {other}");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// "Live" is only half an answer on a rig spread across machines — the
    /// other half is WHERE, and it rides the same row.
    #[tokio::test]
    async fn list_says_which_host_a_device_is_live_on() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 1, "moltar"))
            .unwrap();

        let KjResult::Ok { data: Some(v), .. } =
            d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await
        else {
            panic!("expected Ok with data");
        };
        let row = |name: &str| -> serde_json::Value {
            v.as_array()
                .expect("array")
                .iter()
                .find(|r| r["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("row {name} present"))
        };
        assert_eq!(row("minibrute")["host"], "moltar");
        // Unknown presence has no host to report — null, never a guess at
        // "this machine" (the reporting sink need not be local at all).
        assert!(row("timidity")["host"].is_null());
    }

    /// The human table grows a where-column once somebody has answered it.
    #[tokio::test]
    async fn list_non_json_renders_the_host_column() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 1, "moltar"))
            .unwrap();

        let KjResult::Ok { message, .. } = d.dispatch(&[s("midi"), s("list")], &c).await else {
            panic!("expected Ok");
        };
        let line = message
            .lines()
            .find(|l| l.contains("minibrute"))
            .expect("minibrute row");
        assert!(line.contains("live"), "line: {line}");
        assert!(line.contains("moltar"), "line: {line}");
    }

    /// With nothing reported the column is pure padding — don't draw it.
    #[tokio::test]
    async fn list_non_json_omits_the_host_column_when_nobody_reported() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let KjResult::Ok { message, .. } = d.dispatch(&[s("midi"), s("list")], &c).await else {
            panic!("expected Ok");
        };
        let line = message
            .lines()
            .find(|l| l.contains("minibrute"))
            .expect("minibrute row");
        let after = line.split("unknown").nth(1).expect("presence label");
        assert!(
            after.starts_with("  ") && !after.starts_with("   "),
            "the title must follow the label directly, with no blank \
             where-column between them: {line:?}"
        );
    }

    /// `kj midi show` carries the live record (including the port facts the
    /// sink reported) alongside the durable document.
    #[tokio::test]
    async fn show_includes_the_presence_record() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 7))
            .unwrap();

        let result = d
            .dispatch(&[s("midi"), s("show"), s("minibrute"), s("--json")], &c)
            .await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v["presence"], "live");
                assert_eq!(v["presence_record"]["present"]["value"], true);
                assert_eq!(v["presence_record"]["present"]["source"], "sink");
                assert_eq!(v["presence_record"]["ports"]["value"][0]["address"], "24:0");
                assert_eq!(v["presence_record"]["host"]["value"], "moltar");
            }
            other => panic!("expected Ok with data, got {other:?}"),
        }
    }

    /// The human view of `show` answers "where" too.
    #[tokio::test]
    async fn show_non_json_names_the_reporting_host() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        d.kernel()
            .midi_presence()
            .record(sink_report_from("minibrute", true, 7, "zorak"))
            .unwrap();

        let KjResult::Ok { message, .. } = d
            .dispatch(&[s("midi"), s("show"), s("minibrute")], &c)
            .await
        else {
            panic!("expected Ok");
        };
        assert!(message.contains("host:    zorak"), "message: {message}");
    }

    /// A sink's disconnect un-knows what it told us, everywhere it shows: the
    /// row goes back to `unknown` and `/run/midi/<device>` stops existing.
    /// This is the crashed-sink case — no unplug report is ever coming.
    #[tokio::test]
    async fn a_reaped_connection_takes_its_rows_and_run_files_with_it() {
        use crate::vfs::{VfsError, VfsOps};
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let record = sink_report_from("minibrute", true, 1, "moltar");
        let connection = record.sink.connection;
        d.kernel().midi_presence().record(record).unwrap();

        d.kernel().midi_presence().reap_connection(connection);

        let KjResult::Ok { data: Some(v), .. } =
            d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await
        else {
            panic!("expected Ok with data");
        };
        let row = v
            .as_array()
            .expect("array")
            .iter()
            .find(|r| r["name"] == "minibrute")
            .cloned()
            .expect("row minibrute present");
        assert_eq!(row["presence"], "unknown", "reaped reads as unknown");
        assert!(row["host"].is_null());
        assert!(row["at"].is_null());

        assert!(
            matches!(
                d.kernel()
                    .vfs()
                    .read_all(std::path::Path::new("/run/midi/minibrute"))
                    .await,
                Err(VfsError::NotFound(_))
            ),
            "the /run entry must be gone, not a lingering present=true file"
        );
    }

    /// `kj midi list` reads the same state the `/run/midi/<device>` view
    /// serves — one store, two readers (kai/kaish read the path, kj reads the
    /// store). If these ever disagree, presence is lying somewhere.
    #[tokio::test]
    async fn the_run_midi_view_agrees_with_the_list_column() {
        use crate::vfs::VfsOps;
        let d = test_dispatcher_crdt_rc().await;
        d.kernel()
            .midi_presence()
            .record(sink_report("minibrute", true, 5))
            .unwrap();

        let names: Vec<String> = d
            .kernel()
            .vfs()
            .readdir(std::path::Path::new("/run/midi"))
            .await
            .expect("readdir /run/midi")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["minibrute".to_string()]);

        let bytes = d
            .kernel()
            .vfs()
            .read_all(std::path::Path::new("/run/midi/minibrute"))
            .await
            .expect("read /run/midi/minibrute");
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["present"]["value"], true);
        assert_eq!(doc["present"]["source"], "sink");
        assert_eq!(doc["present"]["at"], 5);
    }

    /// `kj midi list` on a kernel with no `/etc/midi` mount at all answers
    /// truthfully with an empty listing rather than erroring — mirrors `kj
    /// config list`'s "absent mount reads as empty" contract.
    #[tokio::test]
    async fn list_with_no_midi_mount_is_an_empty_listing_not_an_error() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                assert_eq!(v.as_array().map(|a| a.len()), Some(0));
            }
            other => panic!("expected Ok with an empty array, got {other:?}"),
        }
    }
}

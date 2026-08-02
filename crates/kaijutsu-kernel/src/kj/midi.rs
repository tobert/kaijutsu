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

use clap::{Parser, Subcommand};
use kaijutsu_types::ContentType;
use kaijutsu_types::paths::MIDI_ROOT;

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
    Ok(format!("{dir}/{bare}"))
}

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

        let mut rows: Vec<(String, String)> = Vec::new();
        for entry in entries.into_iter().filter(|e| e.kind.is_file()) {
            let path = format!("{dir}/{}", entry.name);
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
            rows.push((entry.name, title));
        }
        rows.sort();

        let data = serde_json::Value::Array(
            rows.iter()
                .map(|(name, title)| serde_json::json!({ "name": name, "title": title }))
                .collect(),
        );
        if json {
            return KjResult::ok_with_data(data.to_string(), data);
        }
        if rows.is_empty() {
            return KjResult::ok_with_data("(no device profiles)".to_string(), data);
        }
        let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let lines: Vec<String> = rows
            .iter()
            .map(|(name, title)| format!("  {name:<width$}  {title}"))
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
        let record = serde_json::json!({
            "path": canonical,
            "name": bare,
            "content_length": content.len(),
            "content": content,
        });
        if json {
            return KjResult::ok_with_data(record.to_string(), record);
        }
        let out = format!(
            "path:    {canonical}\nlength:  {} bytes\n\n{content}\n",
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
    }

    #[tokio::test]
    async fn list_shows_minibrute_and_timidity_with_titles() {
        let d = test_dispatcher_crdt_rc().await;
        let c = test_caller();
        let result = d.dispatch(&[s("midi"), s("list"), s("--json")], &c).await;
        match result {
            KjResult::Ok { data: Some(v), .. } => {
                let arr = v.as_array().expect("array");
                let minibrute = arr
                    .iter()
                    .find(|row| row["name"] == "minibrute")
                    .expect("minibrute row present");
                assert!(
                    minibrute["title"]
                        .as_str()
                        .is_some_and(|t| t.contains("MiniBrute")),
                    "minibrute title: {minibrute:?}"
                );
                let timidity = arr
                    .iter()
                    .find(|row| row["name"] == "timidity")
                    .expect("timidity row present");
                assert!(
                    timidity["title"]
                        .as_str()
                        .is_some_and(|t| t.contains("TiMidity")),
                    "timidity title: {timidity:?}"
                );
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

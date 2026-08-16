//! Embedded default MIDI device profile seeds, seeded onto the kernel-owned
//! `/etc/midi/devices/<name>` tree (`docs/midi-next.md` "Storage and
//! identity", slice 1 step 2).
//!
//! Same ownership contract as rc/config (`docs/config-ownership.md`):
//! the kernel is the **sole owner** of `/etc/midi` — no host file, no
//! write-through. The bodies live as real files under
//! `assets/defaults/midi/devices/` and are embedded here via [`include_dir!`],
//! exactly like `crate::seed_scripts` embeds `assets/defaults/rc/`. The
//! embedded tree IS the manifest: dropping a new `<device>.md` file under
//! `assets/defaults/midi/devices/` is the whole seed contribution — no Rust
//! edit, no manifest to update by hand. A parallel effort is expected to add
//! device files here as gear comes on the bench (KeyStep Pro, KeyLab, …).
//!
//! ## Path shape
//!
//! Each embedded `<name>.md` seeds one canonical path:
//! `/etc/midi/devices/<name>` — the `.md` extension is dropped. Today that's
//! a single prose+JSON document per device; the mount is the same
//! directory-capable `ConfigDocFs` backend `/etc/rc` uses, so a device can
//! grow into an rc-style bucket of `SXX-*.{md,kai}` files later
//! (`docs/midi-next.md` "The core split") without a storage migration — only
//! this collector (and `kj midi list/show`'s traversal) would need to widen
//! to walk a device subdirectory instead of reading a single leaf file.
//!
//! ## Seed contract — bootstrap-once, not a floor
//!
//! Identical to rc/config: [`seed_files`] feeds
//! [`crate::runtime::config_doc_fs::ConfigDocFs::seed_entries`], which
//! writes only paths absent from the CRDT (a live edit or a deleted profile
//! is never resurrected by a later boot). There is no bulk reseed for a
//! single device yet (nothing needs `kj midi reset` today — profiles are
//! read-only in slice 1); the embedded default remains available via
//! [`seed_body`] for when a write verb lands.

use include_dir::{include_dir, Dir, DirEntry};

/// The embedded `/etc/midi/devices` seed tree — a 1:1 mirror of
/// `assets/defaults/midi/devices/`, embedded at build time.
static MIDI_DEVICE_SEED_DIR: Dir<'static> =
    include_dir!("$CARGO_MANIFEST_DIR/../../assets/defaults/midi/devices");

/// The VFS root every MIDI device profile lives under. Re-exported from
/// [`kaijutsu_types::paths::MIDI_ROOT`] — the single source of truth.
pub use kaijutsu_types::paths::MIDI_ROOT as MIDI_VFS_ROOT;

/// Strip the `/etc/midi/devices/` prefix from a canonical device path.
/// Returns `None` for a path that isn't a direct child of the devices tree
/// (including the devices dir itself, which has no seed body of its own).
fn device_name(canonical: &str) -> Option<&str> {
    let rel = canonical
        .strip_prefix(MIDI_VFS_ROOT)?
        .strip_prefix("/devices/")?;
    (!rel.is_empty() && !rel.contains('/')).then_some(rel)
}

/// Recursively collect every embedded `<name>.md` seed file as
/// `(canonical /etc/midi/devices/<name> path, body)`. Only `.md` files are
/// seeds today (the static prose+JSON profile half of the future rc-style
/// bucket split, `docs/midi-next.md`) — a stray non-`.md` asset under the
/// seed tree is skipped rather than silently seeded under the wrong shape.
fn collect_seeds(dir: &'static Dir<'static>, out: &mut Vec<(String, &'static str)>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(sub) => collect_seeds(sub, out),
            DirEntry::File(file) => {
                let rel = match file.path().to_str() {
                    Some(r) => r,
                    None => continue, // non-UTF-8 path: not a canonical seed
                };
                let Some(name) = rel.strip_suffix(".md") else {
                    continue;
                };
                let body = file
                    .contents_utf8()
                    .expect("embedded midi device seed must be valid UTF-8");
                out.push((kaijutsu_types::paths::midi_device_path(name), body));
            }
        }
    }
}

/// The embedded seed set as `(canonical path, body)` pairs, derived by
/// walking [`MIDI_DEVICE_SEED_DIR`].
pub fn seed_files() -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    collect_seeds(&MIDI_DEVICE_SEED_DIR, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The embedded seed body for one canonical device path, or `None` if no
/// seed ships for it. Not yet wired to a `kj midi` verb (there is no write
/// path in slice 1), but kept for parity with `crate::seed_scripts::seed_body`
/// / `crate::config_seed::config_seed_body` so a future `kj midi reset`
/// costs no new plumbing.
pub fn seed_body(canonical_path: &str) -> Option<&'static str> {
    let name = device_name(canonical_path)?;
    MIDI_DEVICE_SEED_DIR
        .get_file(format!("{name}.md"))
        .and_then(|f| f.contents_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four devices `assets/defaults/midi/devices/` ships today. Named
    /// once here so the test message is "these named seeds must exist," not a
    /// pile of individual `assert!`s that silently stop growing when a fifth
    /// device is added and forgotten in this test.
    const SHIPPED_DEVICES: &[&str] = &["minibrute", "timidity", "keystep-pro", "keylab-88-mkii"];

    #[test]
    fn seed_files_covers_the_shipped_devices() {
        let expected: Vec<String> = SHIPPED_DEVICES
            .iter()
            .map(|name| format!("{MIDI_VFS_ROOT}/devices/{name}"))
            .collect();
        let paths: Vec<String> = seed_files().into_iter().map(|(p, _)| p).collect();
        for want in &expected {
            assert!(
                paths.contains(want),
                "named seed must exist: {want}; paths: {paths:?}"
            );
        }
        // The .md extension is dropped from the canonical path.
        assert!(
            paths.iter().all(|p| !p.ends_with(".md")),
            "canonical device paths must not carry the .md extension: {paths:?}"
        );
    }

    #[test]
    fn every_seed_body_is_nonempty() {
        for (path, body) in seed_files() {
            assert!(!body.is_empty(), "seed body for {path} must be non-empty");
        }
    }

    #[test]
    fn seed_body_resolves_and_rejects_unknown() {
        let minibrute = seed_body("/etc/midi/devices/minibrute").expect("minibrute must seed");
        assert!(minibrute.contains("MiniBrute"));
        assert!(seed_body("/etc/midi/devices/nonesuch").is_none());
        // A bare name is not a canonical key.
        assert!(seed_body("minibrute").is_none());
        // Neither is the devices directory itself.
        assert!(seed_body("/etc/midi/devices").is_none());
    }

    /// Extract every ` ```json ` fenced block from a markdown body, in order.
    /// Deliberately dumb (line-oriented, no nesting) — every shipped seed
    /// today is a flat prose+JSON document, and this only needs to find the
    /// same fences a human reading the `.md` would.
    fn json_fences(body: &str) -> Vec<&str> {
        const OPEN: &str = "```json\n";
        let mut fences = Vec::new();
        let mut offset = 0usize;
        while let Some(start_rel) = body[offset..].find(OPEN) {
            let content_start = offset + start_rel + OPEN.len();
            let Some(end_rel) = body[content_start..].find("\n```") else {
                panic!("unterminated ```json fence starting at byte {content_start} in body");
            };
            let content_end = content_start + end_rel;
            fences.push(&body[content_start..content_end]);
            offset = content_end + "\n```".len();
        }
        fences
    }

    /// Every ` ```json ` fenced block in every shipped seed must be valid
    /// JSON. A profile with a syntax error in its JSON must fail the build's
    /// test run, not surface at runtime when `kj midi show`/the app matcher
    /// tries to parse it.
    #[test]
    fn every_json_fence_in_every_seed_parses() {
        for (path, body) in seed_files() {
            let fences = json_fences(body);
            assert!(
                !fences.is_empty(),
                "seed {path} has no ```json fenced block — every shipped profile is expected to carry at least one"
            );
            for (i, fence) in fences.iter().enumerate() {
                serde_json::from_str::<serde_json::Value>(fence).unwrap_or_else(|e| {
                    panic!("seed {path}: ```json fence #{i} failed to parse: {e}\n---\n{fence}\n---")
                });
            }
        }
    }
}

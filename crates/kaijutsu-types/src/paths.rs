//! Canonical VFS path constants, builders, and predicates for kaijutsu's
//! well-known mount points (`/etc/rc`, `/etc/config`, `/etc/client`, `/v/*`).
//!
//! This is the **one source of truth** for these strings. Every mount, gate,
//! `format!`, and regex that addresses one of these trees builds its path (or
//! tests its boundary) through here rather than re-hardcoding the prefix.
//! `kaijutsu-types` has no internal kaijutsu dependencies, so this module is
//! reachable from every crate that needs to speak these paths (kernel,
//! server, client, app).
//!
//! # Reserved names
//!
//! `/v` is a shared namespace: kaish claims some names under it for its own
//! builtins (e.g. `/v/bin`, `/v/jobs`) that are NOT kaijutsu VFS mounts. Do
//! not introduce a kaijutsu mount at a name kaish already owns — see
//! `crates/kaijutsu-kernel/src/runtime/embedded_kaish.rs` for the full `/v`
//! layout this module's `/v/*` consts slot into.
//!
//! # Boundary semantics
//!
//! Every predicate here is **component-boundary correct**: `/etc/rc` matches
//! itself and any real path-component child (`/etc/rc/foo`), never a string
//! that merely shares the prefix (`/etc/rcfoo`). Half a dozen call sites used
//! to reimplement this check by hand (correctly, as it happens); now there is
//! one implementation, so a future site can't get it wrong.

/// Root of the CRDT-owned rc lifecycle-script tree
/// (`/etc/rc/<context_type>/<verb>/SXX-name.{kai,md}`). CRDT-owned: no host
/// file, no write-through — see `docs/config-crdt-ownership.md`.
pub const RC_ROOT: &str = "/etc/rc";

/// Root of the CRDT-owned kernel-global config tree. A flat namespace:
/// `/etc/config/<name>` (e.g. `models.toml`, `theme.toml`).
pub const CONFIG_ROOT: &str = "/etc/config";

/// Root of the CRDT-owned per-client config tree. Hierarchical:
/// `/etc/client/<name>` is the shared client default; `/etc/client/<client_id>/<name>`
/// is one client's override.
pub const CLIENT_ROOT: &str = "/etc/client";

/// Root of the CRDT-owned MIDI device profile tree
/// (`docs/midi-next.md` "Storage and identity"): kernel sole owner, no host
/// file, optional embedded seeds for gear we ship knowledge of. Devices live
/// under `/etc/midi/devices/<name>` — today a single `.md` document per
/// device, but the tree is directory-capable on the same CRDT-native backend
/// as `/etc/rc`, so a device can grow into an rc-style bucket of
/// `SXX-*.{md,kai}` files later without a storage migration.
pub const MIDI_ROOT: &str = "/etc/midi";

/// Root of the kernel's **ephemeral runtime state** tree. Nothing under `/run`
/// is CRDT-owned, nothing is a host file, and nothing survives a kernel
/// restart: each tenant is a read-only view synthesized from in-memory state,
/// so a kernel that just booted with no sinks connected truthfully knows
/// nothing. The unix `/run` convention, kept deliberately (it is what
/// `docs/midi-next.md` names) rather than folded into `/v` (kaish-shared
/// virtual filesystems) or `/r` (live *client* roots).
pub const RUN_ROOT: &str = "/run";

/// Root of the sink-fed MIDI presence store (`docs/midi-next.md` "Presence is
/// sink-fed"). `/run/midi/<device>` renders one profile-matched device's
/// provenance-tagged presence facts (`{value, source, at}`) as JSON, written
/// only by app→kernel presence reports and readable by kai/kaish/`kj midi`.
/// Ephemeral by construction — see [`RUN_ROOT`].
pub const MIDI_RUN_ROOT: &str = "/run/midi";

/// Root of the read-only content-addressed object pool
/// (`/v/cas/<shard>/<hash>`).
pub const CAS_ROOT: &str = "/v/cas";

/// Root of the CRDT conversation-document view mount (kaish / file-tool read
/// surface over conversation documents).
pub const DOCS_ROOT: &str = "/v/docs";

/// Root of the CRDT input-document view mount.
pub const INPUT_ROOT: &str = "/v/input";

/// Root of the client-shares namespace (`docs/slash-r.md`) — the reverse of
/// `/v`: a sibling top-level tree (not under `/v`) because it names remote
/// *clients*, not kernel-local virtual filesystems. Layout:
/// `/r/<client-id>/<share-name>/...`, plus a synthesized `/r/index` registry
/// TSV. One `ShareFs` backend mounts here; per-client mounts are impossible
/// (the mount table freezes after bootstrap).
pub const R_ROOT: &str = "/r";

// ---------------------------------------------------------------------
// Builders — replace scattered `format!` calls at mount/write sites.
// ---------------------------------------------------------------------

/// The directory a `(context_type, verb)` pair's rc scripts live in:
/// `/etc/rc/<context_type>/<verb>`.
pub fn rc_dir(context_type: &str, verb: &str) -> String {
    format!("{RC_ROOT}/{context_type}/{verb}")
}

/// One rc script's canonical path: `/etc/rc/<context_type>/<verb>/<name>`.
/// `name` is the full filename (`SXX-name.{kai,md}`).
pub fn rc_script_path(context_type: &str, verb: &str, name: &str) -> String {
    format!("{}/{name}", rc_dir(context_type, verb))
}

/// One kernel-global config file's canonical path: `/etc/config/<name>`.
pub fn config_path(name: &str) -> String {
    format!("{CONFIG_ROOT}/{name}")
}

/// One client config file's canonical path. `client_id = None` is the shared
/// default at the mount root (`/etc/client/<name>`); `Some(id)` is that
/// client's override (`/etc/client/<id>/<name>`).
pub fn client_config_path(client_id: Option<&str>, name: &str) -> String {
    match client_id {
        Some(id) => format!("{CLIENT_ROOT}/{id}/{name}"),
        None => format!("{CLIENT_ROOT}/{name}"),
    }
}

/// One MIDI device profile's canonical path: `/etc/midi/devices/<name>`.
pub fn midi_device_path(name: &str) -> String {
    format!("{MIDI_ROOT}/devices/{name}")
}

/// One device's presence record path: `/run/midi/<device>`. The leaf name is
/// the same `<device>` key as [`midi_device_path`] — presence is keyed by
/// profile name, so `/etc/midi/devices/<name>` and `/run/midi/<name>` are the
/// durable and ephemeral halves of one device.
pub fn midi_presence_path(device: &str) -> String {
    format!("{MIDI_RUN_ROOT}/{device}")
}

/// A live client's root under `/r`: `/r/<client_id>`.
pub fn r_client_path(client_id: &str) -> String {
    format!("{R_ROOT}/{client_id}")
}

/// One share's canonical path: `/r/<client_id>/<share>`.
pub fn r_share_path(client_id: &str, share: &str) -> String {
    format!("{R_ROOT}/{client_id}/{share}")
}

// ---------------------------------------------------------------------
// Predicates — component-boundary-correct tree membership tests.
// ---------------------------------------------------------------------

/// True if `path` is `root` itself or a real path-component child of it
/// (`root` followed by `/`) — never merely a string that shares the prefix
/// (`/etc/rc` vs `/etc/rcfoo`).
fn is_or_under(path: &str, root: &str) -> bool {
    path == root || (path.starts_with(root) && path.as_bytes().get(root.len()) == Some(&b'/'))
}

/// True if `path` is under the rc tree (`/etc/rc` or `/etc/rc/...`). Writing
/// here is gated on the `rc-write` capability at the call sites that enforce
/// it (`file_tools/path.rs`, `kj/rc.rs`); this predicate only answers "is this
/// the rc tree," not "is the write allowed."
pub fn is_rc_path(path: &str) -> bool {
    is_or_under(path, RC_ROOT)
}

/// True if `path` is under the kernel-global config tree (`/etc/config` or
/// `/etc/config/...`). Does not include the per-client tree — see
/// [`is_client_path`].
pub fn is_config_path(path: &str) -> bool {
    is_or_under(path, CONFIG_ROOT)
}

/// True if `path` is under the per-client config tree (`/etc/client` or
/// `/etc/client/...`).
pub fn is_client_path(path: &str) -> bool {
    is_or_under(path, CLIENT_ROOT)
}

/// True if `path` is under the MIDI device profile tree (`/etc/midi` or
/// `/etc/midi/...`).
pub fn is_midi_path(path: &str) -> bool {
    is_or_under(path, MIDI_ROOT)
}

/// True if `path` is under the ephemeral MIDI presence store (`/run/midi` or
/// `/run/midi/...`). Disjoint from [`is_midi_path`]: the durable profile and
/// the ephemeral presence record for one device live in different trees on
/// purpose (a restart drops the latter and keeps the former).
pub fn is_midi_run_path(path: &str) -> bool {
    is_or_under(path, MIDI_RUN_ROOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc_builders_join_components() {
        assert_eq!(rc_dir("coder", "create"), "/etc/rc/coder/create");
        assert_eq!(
            rc_script_path("coder", "create", "S00-stance.kai"),
            "/etc/rc/coder/create/S00-stance.kai"
        );
    }

    #[test]
    fn config_builder_joins_the_flat_namespace() {
        assert_eq!(config_path("models.toml"), "/etc/config/models.toml");
    }

    #[test]
    fn client_config_builder_covers_shared_and_override() {
        assert_eq!(
            client_config_path(None, "metronome.toml"),
            "/etc/client/metronome.toml"
        );
        assert_eq!(
            client_config_path(Some("abc-123"), "metronome.toml"),
            "/etc/client/abc-123/metronome.toml"
        );
    }

    #[test]
    fn r_builders_join_components() {
        assert_eq!(r_client_path("c-123"), "/r/c-123");
        assert_eq!(r_share_path("c-123", "downloads"), "/r/c-123/downloads");
    }

    #[test]
    fn midi_builder_joins_the_devices_namespace() {
        assert_eq!(
            midi_device_path("minibrute"),
            "/etc/midi/devices/minibrute"
        );
    }

    #[test]
    fn midi_presence_builder_joins_the_run_namespace() {
        assert_eq!(midi_presence_path("keystep-pro"), "/run/midi/keystep-pro");
        // The durable and ephemeral halves share the device key, not the tree.
        assert_eq!(
            midi_presence_path("minibrute")
                .rsplit('/')
                .next()
                .unwrap(),
            midi_device_path("minibrute").rsplit('/').next().unwrap()
        );
    }

    #[test]
    fn predicates_match_root_and_children_only() {
        assert!(is_rc_path("/etc/rc"));
        assert!(is_rc_path("/etc/rc/coder/create/S00-stance.md"));
        assert!(!is_rc_path("/etc/rcfoo"));
        assert!(!is_rc_path("/etc"));
        assert!(!is_rc_path("/etc/passwd"));

        assert!(is_config_path("/etc/config"));
        assert!(is_config_path("/etc/config/models.toml"));
        assert!(!is_config_path("/etc/configuration"));

        assert!(is_client_path("/etc/client"));
        assert!(is_client_path("/etc/client/metronome.toml"));
        assert!(is_client_path("/etc/client/abc-123/metronome.toml"));
        assert!(!is_client_path("/etc/clientele"));

        assert!(is_midi_path("/etc/midi"));
        assert!(is_midi_path("/etc/midi/devices/minibrute"));
        assert!(!is_midi_path("/etc/midifoo"));

        assert!(is_midi_run_path("/run/midi"));
        assert!(is_midi_run_path("/run/midi/keystep-pro"));
        assert!(!is_midi_run_path("/run/midifoo"));
        assert!(!is_midi_run_path("/run"));
    }

    /// The durable profile tree and the ephemeral presence store never claim
    /// each other's paths — a `/run/midi` record must not read as config.
    #[test]
    fn presence_store_and_profile_tree_are_disjoint() {
        assert!(!is_midi_path(MIDI_RUN_ROOT));
        assert!(!is_midi_run_path(MIDI_ROOT));
        assert!(!is_config_path(MIDI_RUN_ROOT));
        assert!(!is_rc_path(MIDI_RUN_ROOT));
    }

    /// The rc/config/client/midi trees never falsely overlap each other, even
    /// though they share the `/etc` parent and near-identical names.
    #[test]
    fn trees_do_not_cross_match() {
        assert!(!is_rc_path(CONFIG_ROOT));
        assert!(!is_config_path(RC_ROOT));
        assert!(!is_client_path(CONFIG_ROOT));
        assert!(!is_config_path(CLIENT_ROOT));
        assert!(!is_midi_path(RC_ROOT));
        assert!(!is_midi_path(CONFIG_ROOT));
        assert!(!is_midi_path(CLIENT_ROOT));
        assert!(!is_rc_path(MIDI_ROOT));
        assert!(!is_config_path(MIDI_ROOT));
    }
}

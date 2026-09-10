//! Canonical VFS path constants, builders, and predicates for kaijutsu's
//! well-known mount points (`/config/rc`, `/config/kernel`, `/config/client`,
//! `/v/*`).
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
//! Every predicate here is **component-boundary correct**: `/config/rc`
//! matches itself and any real path-component child (`/config/rc/foo`), never
//! a string that merely shares the prefix (`/config/rcfoo`). Half a dozen call sites used
//! to reimplement this check by hand (correctly, as it happens); now there is
//! one implementation, so a future site can't get it wrong.

/// Parent of every configuration tree. **Has no backend of its own** — the
/// mount table lists it from the mount points beneath it, so `ls /config`
/// shows `rc`, `kernel`, `client` and `midi` with nothing serving `/config`
/// itself.
///
/// Each child is a well-known *name* whose host directory is a mount
/// declaration, not a compiled-in constant. `docs/config-namespace.md` is
/// canonical.
///
/// The four children are siblings rather than a base plus subtrees on
/// purpose: [`is_or_under`]-style predicates are component-correct but not
/// sibling-aware, so a root that contained the others would make
/// `is_config_path("/config/rc/x")` true.
pub const CONFIG_NAMESPACE_ROOT: &str = "/config";

/// Root of the rc lifecycle-script tree
/// (`/config/rc/<context_type>/<verb>/SXX-name.{kai,md}`). An ordinary host
/// directory reached through `LocalBackend` — see `docs/rc-on-disk.md`.
pub const RC_ROOT: &str = "/config/rc";

/// Root of the kernel-global config tree. A flat namespace:
/// `/config/kernel/<name>` (e.g. `theme.toml`, `mcp.toml`, `gate.toml`) —
/// the kernel's own settings, as opposed to a client's or a device's.
pub const CONFIG_ROOT: &str = "/config/kernel";

/// Root of the per-client config tree. Hierarchical:
/// `/config/client/default/<name>` is the shared default,
/// `/config/client/<client_id>/<name>` is one client's override. The
/// `default/` level exists so a segment is never ambiguously a filename or a
/// client id — see [`client_config_path`].
pub const CLIENT_ROOT: &str = "/config/client";

/// Root of the MIDI device profile tree (`docs/midi-next.md` "Storage and
/// identity"), with optional embedded seeds for gear we ship knowledge of.
/// Devices live under `/config/midi/devices/<name>` — today a single `.md`
/// per device, but the tree is an ordinary directory, so a device can grow
/// into an rc-style bucket of `SXX-*.{md,kai}` files later without a storage
/// migration.
pub const MIDI_ROOT: &str = "/config/midi";

/// Every configuration tree, in mount-declaration order. The one list a
/// caller iterates to answer "what does `/config` contain" — a fifth tree is
/// added here and nowhere else.
pub const CONFIG_TREES: [&str; 4] = [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT];

/// The single path component naming a config tree: `/config/rc` → `rc`. This
/// is also its default directory name under the config root, which is what
/// makes "declare nothing and every tree is an ordinary subdirectory" true
/// without a second table mapping one to the other.
///
/// `None` for any path that is not one of [`CONFIG_TREES`].
pub fn config_tree_name(tree_root: &str) -> Option<&'static str> {
    CONFIG_TREES
        .into_iter()
        .find(|t| *t == tree_root)
        .and_then(|t| t.strip_prefix(&format!("{CONFIG_NAMESPACE_ROOT}/")))
}

/// Root of the kernel's **ephemeral runtime state** tree. Nothing under `/run`
/// is kernel-owned, nothing is a host file, and nothing survives a kernel
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

/// Root of the sink-fed audio inventory store (`docs/audio-daemon.md` "One
/// inventory owner"). `/run/audio/<node-dir>/inventory.json` renders one
/// audio daemon's accepted inventory report — every observed ALSA endpoint
/// and wire, plus the daemon's own plumbing client ids — written only by
/// app→kernel `reportAudioInventory` calls. Ephemeral by construction — see
/// [`RUN_ROOT`].
pub const AUDIO_RUN_ROOT: &str = "/run/audio";

/// Root of the live roster view (`crates/kaijutsu-kernel/src/roster.rs`) —
/// who's around right now, agents and humans alike. `/run/roster/index` is a
/// generation-stamped TSV of every current row; `/run/roster/<entity_kind>-
/// <entity_id>/` holds one fact-per-file for that entity. A materialized
/// view over `kernel.db`, but still ephemeral in the `/run` sense: liveness
/// itself is never trusted as a stored fact across a restart (see the module
/// doc), so this is exactly as much "current runtime state, not truth that
/// survives unquestioned" as `/run/midi`.
pub const ROSTER_RUN_ROOT: &str = "/run/roster";

/// Root of the read-only content-addressed object pool
/// (`/v/cas/<shard>/<hash>`).
pub const CAS_ROOT: &str = "/v/cas";

/// Root of the conversation-document view mount (kaish / file-tool read
/// surface over conversation documents).
pub const DOCS_ROOT: &str = "/v/docs";

/// Root of the input-document view mount.
pub const INPUT_ROOT: &str = "/v/input";

/// Root of the read-only dirty-file-buffer ("swap") view mount
/// (docs/file-buffers.md). Mirrors each unsaved buffer's real path under
/// `/v/swap/<kernel_id>/...` — the kernel-id segment (`KernelId::to_hex()`)
/// says whose disk a swap belongs to, so a swap stays findable in a backup,
/// a copied data dir, or a view spanning more than one kernel.
pub const SWAP_ROOT: &str = "/v/swap";

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
/// `/config/rc/<context_type>/<verb>`.
pub fn rc_dir(context_type: &str, verb: &str) -> String {
    format!("{RC_ROOT}/{context_type}/{verb}")
}

/// One rc script's canonical path: `/config/rc/<context_type>/<verb>/<name>`.
/// `name` is the full filename (`SXX-name.{kai,md}`).
pub fn rc_script_path(context_type: &str, verb: &str, name: &str) -> String {
    format!("{}/{name}", rc_dir(context_type, verb))
}

/// One kernel-global config file's canonical path: `/config/kernel/<name>`.
pub fn config_path(name: &str) -> String {
    format!("{CONFIG_ROOT}/{name}")
}

/// The directory name holding the shared client default, as opposed to one
/// client's override. A real path segment so a client id can never collide
/// with a config filename.
pub const CLIENT_DEFAULT_DIR: &str = "default";

/// One client config file's canonical path. `client_id = None` is the shared
/// default (`/config/client/default/<name>`); `Some(id)` is that client's
/// override (`/config/client/<id>/<name>`).
///
/// Both forms are `<root>/<segment>/<name>`. The shared default used to sit at
/// `<root>/<name>`, which made a segment after the root a filename OR a client
/// id depending on which happened to be there — survivable on kernel
/// documents, a collision waiting to happen on a real filesystem.
pub fn client_config_path(client_id: Option<&str>, name: &str) -> String {
    match client_id {
        Some(id) => format!("{CLIENT_ROOT}/{id}/{name}"),
        None => format!("{CLIENT_ROOT}/{CLIENT_DEFAULT_DIR}/{name}"),
    }
}

/// One MIDI device profile's canonical path: `/config/midi/devices/<name>`.
pub fn midi_device_path(name: &str) -> String {
    format!("{MIDI_ROOT}/devices/{name}")
}

/// One device's presence record path: `/run/midi/<device>`. The leaf name is
/// the same `<device>` key as [`midi_device_path`] — presence is keyed by
/// profile name, so `/config/midi/devices/<name>` and `/run/midi/<name>` are the
/// durable and ephemeral halves of one device.
pub fn midi_presence_path(device: &str) -> String {
    format!("{MIDI_RUN_ROOT}/{device}")
}

/// `/run/audio`'s node-directory encoding: a peer nick such as
/// `"audio/moltar"` with every `/` replaced by `-`, giving `"audio-moltar"`.
/// The nick's own `audio/` prefix already makes the directory name
/// self-describing; this is the one place the encoding is defined, so a
/// daemon's own node-directory name and a reader's lookup can never drift
/// apart.
pub fn audio_node_dir(nick: &str) -> String {
    nick.replace('/', "-")
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
/// (`/config/rc` vs `/config/rcfoo`).
fn is_or_under(path: &str, root: &str) -> bool {
    path == root || (path.starts_with(root) && path.as_bytes().get(root.len()) == Some(&b'/'))
}

/// True if `path` is under the rc tree (`/config/rc` or `/config/rc/...`).
pub fn is_rc_path(path: &str) -> bool {
    is_or_under(path, RC_ROOT)
}

/// True if `path` is under the kernel-global config tree (`/config/kernel`
/// or `/config/kernel/...`). Does not include the per-client tree — see
/// [`is_client_path`].
pub fn is_config_path(path: &str) -> bool {
    is_or_under(path, CONFIG_ROOT)
}

/// True if `path` is under the per-client config tree (`/config/client` or
/// `/config/client/...`).
pub fn is_client_path(path: &str) -> bool {
    is_or_under(path, CLIENT_ROOT)
}

/// True if `path` is under the MIDI device profile tree (`/config/midi` or
/// `/config/midi/...`).
pub fn is_midi_path(path: &str) -> bool {
    is_or_under(path, MIDI_ROOT)
}

/// True if `path` is under one of the four `/config` trees — rc,
/// kernel-global config, per-client config, MIDI device profiles. Each keeps
/// a `FileDocumentCache` shadow behind the kaish `cat`/file-tool read path,
/// so a writer that changes one of these paths must invalidate that shadow or
/// the next read serves stale text.
///
/// All four are `LocalBackend` mounts over host directories
/// (`docs/config-namespace.md`); this predicate exists for call chains that
/// cannot reach the mount table (no async, or no mount table in scope), and a
/// fifth such tree needs a fifth arm here. See `docs/file-buffers.md`.
pub fn is_config_doc_root(path: &str) -> bool {
    is_rc_path(path) || is_config_path(path) || is_client_path(path) || is_midi_path(path)
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
        assert_eq!(rc_dir("coder", "create"), "/config/rc/coder/create");
        assert_eq!(
            rc_script_path("coder", "create", "S00-stance.kai"),
            "/config/rc/coder/create/S00-stance.kai"
        );
    }

    #[test]
    fn config_builder_joins_the_flat_namespace() {
        assert_eq!(config_path("theme.toml"), "/config/kernel/theme.toml");
    }

    #[test]
    fn client_config_builder_covers_shared_and_override() {
        // Both forms are `<root>/<segment>/<name>`: the shared default gets a
        // real `default/` segment so a client id can never be mistaken for a
        // filename, or a filename for a client id.
        assert_eq!(
            client_config_path(None, "metronome.toml"),
            "/config/client/default/metronome.toml"
        );
        assert_eq!(
            client_config_path(Some("abc-123"), "metronome.toml"),
            "/config/client/abc-123/metronome.toml"
        );
        assert_eq!(
            client_config_path(None, "metronome.toml").matches('/').count(),
            client_config_path(Some("abc-123"), "metronome.toml").matches('/').count(),
            "shared and override must sit at the same depth, or a segment is ambiguous"
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
            "/config/midi/devices/minibrute"
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
        assert!(is_rc_path("/config/rc"));
        assert!(is_rc_path("/config/rc/coder/create/S00-stance.md"));
        assert!(!is_rc_path("/config/rcfoo"));
        assert!(!is_rc_path("/config"));
        assert!(!is_rc_path("/etc/rc"), "the host's /etc is not ours any more");

        assert!(is_config_path("/config/kernel"));
        assert!(is_config_path("/config/kernel/theme.toml"));
        assert!(!is_config_path("/config/kernelish"));

        assert!(is_client_path("/config/client"));
        assert!(is_client_path("/config/client/default/metronome.toml"));
        assert!(is_client_path("/config/client/abc-123/metronome.toml"));
        assert!(!is_client_path("/config/clientele"));

        assert!(is_midi_path("/config/midi"));
        assert!(is_midi_path("/config/midi/devices/minibrute"));
        assert!(!is_midi_path("/config/midifoo"));

        // The four are SIBLINGS, never nested: a root that contained the
        // others would make every predicate below it true. This is why the
        // kernel-global tree is `/config/kernel` and not `/config` itself.
        for path in [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT] {
            let others = [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT]
                .into_iter()
                .filter(|r| *r != path)
                .filter(|r| is_or_under(path, r))
                .collect::<Vec<_>>();
            assert!(others.is_empty(), "{path} must not sit under {others:?}");
        }

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

    /// `is_config_doc_root` must enumerate exactly the four `/config` trees that
    /// keep a `FileDocumentCache` shadow — one list, one place. A fifth tree
    /// added to one side and not the other is exactly the drift that reverted
    /// an edit under this predicate's predecessor (`docs/file-buffers.md`).
    #[test]
    fn is_config_doc_root_covers_exactly_the_four_config_trees() {
        for root in [RC_ROOT, CONFIG_ROOT, CLIENT_ROOT, MIDI_ROOT] {
            assert!(is_config_doc_root(root), "{root} must be a config root");
        }
        assert!(is_config_doc_root("/config/rc/coder/create/S00.kai"));
        assert!(is_config_doc_root("/config/kernel/theme.toml"));
        assert!(is_config_doc_root("/config/client/default/metronome.toml"));
        assert!(is_config_doc_root("/config/midi/devices/minibrute"));
        // The namespace parent has no backend and is not itself a root.
        assert!(!is_config_doc_root(CONFIG_NAMESPACE_ROOT));
        assert!(!is_config_doc_root("/etc"));
        assert!(!is_config_doc_root("/etc/passwd"));
        assert!(!is_config_doc_root(MIDI_RUN_ROOT), "the ephemeral presence tree is not a doc root");
        assert!(!is_config_doc_root("/home/atobey/src/kaijutsu/notes.md"));
    }
}

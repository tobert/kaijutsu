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

/// Root of the read-only content-addressed object pool
/// (`/v/cas/<shard>/<hash>`).
pub const CAS_ROOT: &str = "/v/cas";

/// Root of the CRDT conversation-document view mount (kaish / file-tool read
/// surface over conversation documents).
pub const DOCS_ROOT: &str = "/v/docs";

/// Root of the CRDT input-document view mount.
pub const INPUT_ROOT: &str = "/v/input";

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
    }

    /// The rc/config/client trees never falsely overlap each other, even
    /// though they share the `/etc` parent and near-identical names.
    #[test]
    fn trees_do_not_cross_match() {
        assert!(!is_rc_path(CONFIG_ROOT));
        assert!(!is_config_path(RC_ROOT));
        assert!(!is_client_path(CONFIG_ROOT));
        assert!(!is_config_path(CLIENT_ROOT));
    }
}

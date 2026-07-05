//! Embedded default config-file bodies + the config seed manifest.
//!
//! The config TOMLs (`theme.toml`, `models.toml`, `mcp.toml`) and the system
//! prompt (`system.md`) are **CRDT-owned**, exactly like `/etc/rc`: a fresh
//! kernel seeds them from these compiled-in defaults into a [`ConfigCrdtFs`]
//! mounted at [`CONFIG_VFS_ROOT`], and the CRDT is the sole owner thereafter
//! (no host file, no write-through). See `docs/config-crdt-ownership.md`.
//!
//! These consts used to live on `ConfigCrdtBackend`; that disk-coupled backend
//! was deleted in slice 2. The bodies moved here so the embedded defaults — the
//! one thing still needed — survive independently of any backend.
//!
//! [`ConfigCrdtFs`]: crate::runtime::config_crdt_fs::ConfigCrdtFs

/// Embedded default theme content (TOML).
pub const DEFAULT_THEME: &str = include_str!("../../../assets/defaults/theme.toml");

/// Embedded default models configuration (LLM providers + embedding, TOML).
pub const DEFAULT_MODELS_CONFIG: &str = include_str!("../../../assets/defaults/models.toml");

/// Alias for backwards compatibility.
pub const DEFAULT_LLM_CONFIG: &str = DEFAULT_MODELS_CONFIG;

/// Embedded default MCP server configuration (TOML).
pub const DEFAULT_MCP_CONFIG: &str = include_str!("../../../assets/defaults/mcp.toml");

/// Embedded default system prompt.
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../../../assets/defaults/system.md");

/// Embedded default metronome click config (TOML). The shared *client* default;
/// see [`CLIENT_VFS_ROOT`] and `docs/config-crdt-ownership.md` "Per-client config".
pub const DEFAULT_METRONOME: &str = include_str!("../../../assets/defaults/metronome.toml");

/// The VFS mount root the kernel-wide config singletons live under. Parallel to
/// [`crate::seed_scripts::RC_VFS_ROOT`] (`/etc/rc`).
pub const CONFIG_VFS_ROOT: &str = "/etc/config";

/// The VFS mount root for **per-client** config (`docs/config-crdt-ownership.md`
/// "Per-client config"). Client-facing config that is machine-local — the
/// metronome click, later the patch bay — lives here, cascading
/// `/etc/client/<client-id>/<file>` → `/etc/client/<file>` → embedded. The
/// files seeded here (via [`client_seed_files`]) are the **shared defaults** at
/// the mount root; per-client overrides at `<client-id>/…` are never seeded
/// (there is no client id at build time), only written lazily.
pub const CLIENT_VFS_ROOT: &str = "/etc/client";

/// The embedded config seed manifest: `(canonical /etc/config path, body)`.
///
/// Mirrors [`crate::seed_scripts::seed_files`] for the config namespace, so the
/// same `ConfigCrdtFs::seed_entries` absent-only, fail-loud seeding serves both.
/// Unlike rc (a directory tree), config is a fixed, flat set, so the manifest is
/// hand-listed here rather than walked from an embedded directory.
pub fn config_seed_files() -> Vec<(String, &'static str)> {
    vec![
        (format!("{CONFIG_VFS_ROOT}/theme.toml"), DEFAULT_THEME),
        (format!("{CONFIG_VFS_ROOT}/models.toml"), DEFAULT_MODELS_CONFIG),
        (format!("{CONFIG_VFS_ROOT}/mcp.toml"), DEFAULT_MCP_CONFIG),
        (format!("{CONFIG_VFS_ROOT}/system.md"), DEFAULT_SYSTEM_PROMPT),
    ]
}

/// The embedded **shared-default** client config manifest: `(canonical
/// /etc/client path, body)`. Only the mount-root shared defaults are seeded;
/// per-client overrides (`/etc/client/<id>/…`) carry no compiled-in default.
pub fn client_seed_files() -> Vec<(String, &'static str)> {
    vec![(format!("{CLIENT_VFS_ROOT}/metronome.toml"), DEFAULT_METRONOME)]
}

/// The embedded default body for a canonical config path (`/etc/config/<file>`
/// or a `/etc/client/<file>` shared default), or `None` when the path ships no
/// built-in seed. Used by `kj config reset` and the parse-fail safety valve
/// (reset-to-embedded). A per-client override path (`/etc/client/<id>/<file>`)
/// resolves to the shared default's body so `reset` on an override restores it
/// to the shipped click.
pub fn config_seed_body(canonical_path: &str) -> Option<&'static str> {
    config_seed_files()
        .into_iter()
        .chain(client_seed_files())
        .find(|(p, _)| p == canonical_path)
        .map(|(_, body)| body)
        // A per-client override path has no seed of its own — fall back to the
        // shared client default with the same file name.
        .or_else(|| {
            let file = canonical_path.rsplit('/').next()?;
            let shared = format!("{CLIENT_VFS_ROOT}/{file}");
            (canonical_path.starts_with(&format!("{CLIENT_VFS_ROOT}/")) && canonical_path != shared)
                .then(|| client_seed_files().into_iter().find(|(p, _)| *p == shared))
                .flatten()
                .map(|(_, body)| body)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_manifest_covers_the_four_config_files() {
        let files = config_seed_files();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(names.contains(&"/etc/config/theme.toml"));
        assert!(names.contains(&"/etc/config/models.toml"));
        assert!(names.contains(&"/etc/config/mcp.toml"));
        assert!(names.contains(&"/etc/config/system.md"));
        assert_eq!(files.len(), 4, "exactly the four known config files");
    }

    #[test]
    fn every_seed_body_is_nonempty() {
        for (path, body) in config_seed_files() {
            assert!(!body.is_empty(), "seed body for {path} must be non-empty");
        }
    }

    #[test]
    fn seed_body_round_trips_and_rejects_unknown() {
        assert_eq!(config_seed_body("/etc/config/models.toml"), Some(DEFAULT_MODELS_CONFIG));
        assert!(config_seed_body("/etc/config/nonesuch.toml").is_none());
        // Bare names are not canonical keys — must be the full /etc/config path.
        assert!(config_seed_body("models.toml").is_none());
    }

    #[test]
    fn client_seed_manifest_is_the_metronome_shared_default() {
        let files = client_seed_files();
        assert_eq!(files.len(), 1, "just the metronome shared default for now");
        assert_eq!(files[0].0, "/etc/client/metronome.toml");
        assert_eq!(files[0].1, DEFAULT_METRONOME);
    }

    #[test]
    fn metronome_default_parses_and_carries_the_click_knobs() {
        let v: toml::Value = toml::from_str(DEFAULT_METRONOME).expect("metronome default is TOML");
        for key in ["enabled", "note", "channel", "velocity", "gate_ms"] {
            assert!(v.get(key).is_some(), "metronome default carries {key}");
        }
        assert_eq!(v["note"].as_integer(), Some(84), "ships the C6 click");
    }

    #[test]
    fn seed_body_resolves_client_shared_and_reset_of_an_override_falls_back() {
        // The shared client default resolves to the metronome body.
        assert_eq!(config_seed_body("/etc/client/metronome.toml"), Some(DEFAULT_METRONOME));
        // A per-client override path carries no seed of its own, so reset-to-embedded
        // restores it to the shared client default (same file name).
        assert_eq!(
            config_seed_body("/etc/client/abc-123/metronome.toml"),
            Some(DEFAULT_METRONOME),
            "resetting a per-client override restores the shared default",
        );
        // An override of an unknown client file still has nothing to reset to.
        assert!(config_seed_body("/etc/client/abc-123/nonesuch.toml").is_none());
    }

    #[test]
    fn theme_default_parses_as_toml() {
        let v: toml::Value = toml::from_str(DEFAULT_THEME).expect("theme default is valid TOML");
        assert!(v.get("bg").is_some(), "theme default carries bg");
    }
}

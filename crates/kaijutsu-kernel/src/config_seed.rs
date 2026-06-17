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

/// The VFS mount root the config files live under. Parallel to
/// [`crate::seed_scripts::RC_VFS_ROOT`] (`/etc/rc`).
pub const CONFIG_VFS_ROOT: &str = "/etc/config";

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

/// The embedded default body for a canonical `/etc/config/<file>` path, or
/// `None` when the path ships no built-in seed. Used by `kj config reset` and
/// the parse-fail safety valve (reset-to-embedded).
pub fn config_seed_body(canonical_path: &str) -> Option<&'static str> {
    config_seed_files()
        .into_iter()
        .find(|(p, _)| p == canonical_path)
        .map(|(_, body)| body)
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
    fn theme_default_parses_as_toml() {
        let v: toml::Value = toml::from_str(DEFAULT_THEME).expect("theme default is valid TOML");
        assert!(v.get("bg").is_some(), "theme default carries bg");
    }
}

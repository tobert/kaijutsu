//! Embedded default config-file bodies + the config seed manifest.
//!
//! The config TOMLs (`theme.toml`, `mcp.toml`, `gate.toml`) seed
//! [`CONFIG_VFS_ROOT`], an ordinary host directory reached
//! through `LocalBackend` (`docs/config-namespace.md`), exactly like
//! `/config/rc`: [`seed_entries_into_dir`] writes each compiled-in default
//! only while the tree is empty, and after that the directory is the
//! content — a file a human edited survives, one they deleted stays deleted.
//!
//! These consts used to live on `ConfigDocBackend`; that disk-coupled backend
//! was deleted in slice 2. The bodies moved here so the embedded defaults — the
//! one thing still needed — survive independently of any backend.

/// Embedded default theme content (TOML).
pub const DEFAULT_THEME: &str = include_str!("../../../assets/defaults/theme.toml");

// NOTE: there is deliberately no `models.toml` here. Model configuration is
// SQL-native — `backends` / `backend_models` / `casts` / `cast_slots` /
// `model_aliases` / `llm_defaults` / `embedding_config` in `kernel_db.rs` —
// and the embedded floor lives in `crate::seed_backends`, not in an asset.
// The `/config/kernel/models.toml` document and its loader were demolished; do
// not reintroduce a TOML in this path.

/// Embedded default MCP server configuration (TOML).
pub const DEFAULT_MCP_CONFIG: &str = include_str!("../../../assets/defaults/mcp.toml");

/// Embedded default gate policy (TOML): the allow/ask/deny tiers
/// `kj::gate_policy` reads beneath the ledger's rules.
pub const DEFAULT_GATE_CONFIG: &str = include_str!("../../../assets/defaults/gate.toml");

/// Embedded drift-briefing instruction. The host file is the live body.
pub const DEFAULT_DISTILLATION_PROMPT: &str =
    include_str!("../../../assets/defaults/prompts/distillation.md");

/// Embedded compact-fork continuation instruction. The host file is the live
/// body.
pub const DEFAULT_CONTINUATION_PROMPT: &str =
    include_str!("../../../assets/defaults/prompts/continuation.md");

/// Embedded default metronome click config (TOML). The shared *client* default;
/// see [`CLIENT_VFS_ROOT`] and `docs/config-namespace.md`.
pub const DEFAULT_METRONOME: &str = include_str!("../../../assets/defaults/metronome.toml");

/// Embedded default mouse-wheel scroll-gain config (TOML). The shared
/// *client* default; see [`CLIENT_VFS_ROOT`] and
/// `docs/config-namespace.md`.
pub const DEFAULT_SCROLL: &str = include_str!("../../../assets/defaults/scroll.toml");

/// The VFS mount root the kernel-wide config singletons live under. Parallel to
/// [`crate::seed_scripts::RC_VFS_ROOT`] (`/config/rc`). Re-exported from
/// [`kaijutsu_types::paths::CONFIG_ROOT`] — the single source of truth.
pub use kaijutsu_types::paths::CONFIG_ROOT as CONFIG_VFS_ROOT;

/// The VFS mount root for **per-client** config (`docs/config-namespace.md`).
/// Client-facing config that is machine-local — the
/// metronome click, mouse-wheel scroll gains, later the patch bay — lives here, cascading
/// `/config/client/<client-id>/<file>` → `/config/client/default/<file>` →
/// embedded. The files seeded here (via [`client_seed_files`]) are the
/// **shared defaults** at `<root>/default/…`; per-client overrides at
/// `<client-id>/…` are never seeded (there is no client id at build time),
/// only written lazily. Re-exported from [`kaijutsu_types::paths::CLIENT_ROOT`]
/// — the single source of truth.
pub use kaijutsu_types::paths::CLIENT_ROOT as CLIENT_VFS_ROOT;

/// The embedded config seed manifest: `(canonical /config/kernel path, body)`.
///
/// Mirrors [`crate::seed_scripts::seed_files`] for the config namespace, so the
/// same [`seed_entries_into_dir`] absent-only, fail-loud seeding serves both.
/// Unlike rc (a directory tree), config is a fixed, flat set, so the manifest is
/// hand-listed here rather than walked from an embedded directory.
pub fn config_seed_files() -> Vec<(String, &'static str)> {
    use kaijutsu_types::paths::config_path;
    vec![
        (config_path("theme.toml"), DEFAULT_THEME),
        (config_path("mcp.toml"), DEFAULT_MCP_CONFIG),
        (config_path("gate.toml"), DEFAULT_GATE_CONFIG),
        (config_path("distillation.md"), DEFAULT_DISTILLATION_PROMPT),
        (config_path("continuation.md"), DEFAULT_CONTINUATION_PROMPT),
    ]
}

/// The embedded **shared-default** client config manifest: `(canonical
/// /config/client/default path, body)`. Only the shared defaults are seeded;
/// per-client overrides (`/config/client/<id>/…`) carry no compiled-in default.
pub fn client_seed_files() -> Vec<(String, &'static str)> {
    vec![
        (kaijutsu_types::paths::client_config_path(None, "metronome.toml"), DEFAULT_METRONOME),
        (kaijutsu_types::paths::client_config_path(None, "scroll.toml"), DEFAULT_SCROLL),
    ]
}

/// Write embedded seed entries into the host directory backing `tree_root`,
/// install-if-absent.
///
/// The file-backed counterpart of the document seeding these trees used
/// before the melt (`docs/config-namespace.md`), and the flat sibling of
/// [`crate::seed_scripts::reseed_rc_files`] — these trees carry no symlinks,
/// so there is no composition to reconstruct.
///
/// An entry that exists is left alone: the directory is the content, so a file
/// you edited survives and one you deleted stays deleted. Returns how many
/// were written.
///
/// Per the crash-over-corruption stance this surfaces I/O errors rather than
/// swallowing them — a half-written config tree is corruption.
pub fn seed_entries_into_dir(
    tree_root: &str,
    entries: Vec<(String, &'static str)>,
    dir: &std::path::Path,
) -> std::io::Result<usize> {
    let prefix = format!("{tree_root}/");
    let mut written = 0;
    for (canonical, body) in entries {
        let Some(rel) = canonical.strip_prefix(&prefix) else {
            // A manifest entry outside the tree it is being seeded into is a
            // defect in the manifest, not something to route around.
            return Err(std::io::Error::other(format!(
                "seed entry {canonical} is not under {tree_root}"
            )));
        };
        let dest = dir.join(rel);
        if dest.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, body)?;
        written += 1;
    }
    Ok(written)
}

/// The embedded default body for a canonical config path
/// (`/config/kernel/<file>` or a `/config/client/default/<file>` shared
/// default), or `None` when the path ships no built-in seed. Used by `kj
/// config reset` and the parse-fail safety valve (reset-to-embedded). A
/// per-client override path (`/config/client/<id>/<file>`) resolves to the
/// shared default's body so `reset` on an override restores it to the shipped
/// click.
pub fn config_seed_body(canonical_path: &str) -> Option<&'static str> {
    config_seed_files()
        .into_iter()
        .chain(client_seed_files())
        .find(|(p, _)| p == canonical_path)
        .map(|(_, body)| body)
        // A per-client override path has no seed of its own — fall back to the
        // shared client default with the same file name. The shared default
        // lives at `<root>/default/<file>`, so it is built through
        // `client_config_path` rather than by joining the root, or an override
        // would resolve against a path no seed occupies.
        .or_else(|| {
            let file = canonical_path.rsplit('/').next()?;
            let shared = kaijutsu_types::paths::client_config_path(None, file);
            (canonical_path.starts_with(&format!("{CLIENT_VFS_ROOT}/")) && canonical_path != shared)
                .then(|| client_seed_files().into_iter().find(|(p, _)| *p == shared))
                .flatten()
                .map(|(_, body)| body)
        })
}

async fn load_auxiliary_prompt(
    vfs: &dyn crate::vfs::VfsOps,
    name: &str,
) -> Result<String, String> {
    let path = kaijutsu_types::paths::config_path(name);
    let bytes = vfs
        .read_all(std::path::Path::new(&path))
        .await
        .map_err(|e| format!("read {path}: {e}"))?;
    let body = String::from_utf8(bytes).map_err(|e| format!("{path} is not UTF-8: {e}"))?;
    if body.trim().is_empty() {
        return Err(format!("{path} is empty"));
    }
    Ok(body)
}

/// Read the live drift-briefing instruction from the ordinary config tree.
pub async fn load_distillation_prompt(vfs: &dyn crate::vfs::VfsOps) -> Result<String, String> {
    load_auxiliary_prompt(vfs, "distillation.md").await
}

/// Read the live compact-fork continuation instruction from the ordinary
/// config tree.
pub async fn load_continuation_prompt(vfs: &dyn crate::vfs::VfsOps) -> Result<String, String> {
    load_auxiliary_prompt(vfs, "continuation.md").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::paths::{client_config_path, config_path};

    #[test]
    fn config_seed_manifest_excludes_system_prompt() {
        let files = config_seed_files();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(names.contains(&config_path("theme.toml").as_str()));
        assert!(names.contains(&config_path("mcp.toml").as_str()));
        assert!(names.contains(&config_path("gate.toml").as_str()));
        assert!(names.contains(&config_path("distillation.md").as_str()));
        assert!(names.contains(&config_path("continuation.md").as_str()));
        assert!(
            !names.contains(&config_path("system.md").as_str()),
            "system prompts are selected by rc, never seeded as kernel config"
        );
    }

    #[test]
    fn models_toml_is_gone_from_the_config_surface() {
        // Model config is SQL-native (see crate::seed_backends). A seed entry
        // here would resurrect a second, silently-diverging source of truth.
        let files = config_seed_files();
        assert!(
            files.iter().all(|(p, _)| !p.ends_with("models.toml")),
            "models.toml must not be a config document: {files:?}"
        );
        assert!(config_seed_body(&config_path("models.toml")).is_none());
    }

    #[test]
    fn every_seed_body_is_nonempty() {
        for (path, body) in config_seed_files() {
            assert!(!body.is_empty(), "seed body for {path} must be non-empty");
        }
    }

    #[test]
    fn auxiliary_prompt_seeds_are_available_for_reset() {
        assert_eq!(
            config_seed_body(&config_path("distillation.md")),
            Some(DEFAULT_DISTILLATION_PROMPT)
        );
        assert_eq!(
            config_seed_body(&config_path("continuation.md")),
            Some(DEFAULT_CONTINUATION_PROMPT)
        );
    }

    #[test]
    fn seed_body_round_trips_and_rejects_unknown() {
        assert_eq!(config_seed_body(&config_path("theme.toml")), Some(DEFAULT_THEME));
        assert!(config_seed_body(&config_path("nonesuch.toml")).is_none());
        // Bare names are not canonical keys — must be the full /config/kernel path.
        assert!(config_seed_body("theme.toml").is_none());
    }

    #[test]
    fn client_seed_manifest_is_the_metronome_and_scroll_shared_defaults() {
        let files = client_seed_files();
        assert_eq!(files.len(), 2, "the metronome + scroll shared defaults");
        assert_eq!(files[0].0, client_config_path(None, "metronome.toml"));
        assert_eq!(files[0].1, DEFAULT_METRONOME);
        assert_eq!(files[1].0, client_config_path(None, "scroll.toml"));
        assert_eq!(files[1].1, DEFAULT_SCROLL);
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
    fn scroll_default_parses_and_carries_the_gain_knobs() {
        let v: toml::Value = toml::from_str(DEFAULT_SCROLL).expect("scroll default is TOML");
        for key in ["line_gain", "pixel_gain"] {
            assert!(v.get(key).is_some(), "scroll default carries {key}");
        }
    }

    #[test]
    fn seed_body_resolves_client_shared_and_reset_of_an_override_falls_back() {
        // The shared client default resolves to the metronome body.
        assert_eq!(
            config_seed_body(&client_config_path(None, "metronome.toml")),
            Some(DEFAULT_METRONOME)
        );
        // A per-client override path carries no seed of its own, so reset-to-embedded
        // restores it to the shared client default (same file name).
        assert_eq!(
            config_seed_body(&client_config_path(Some("abc-123"), "metronome.toml")),
            Some(DEFAULT_METRONOME),
            "resetting a per-client override restores the shared default",
        );
        // An override of an unknown client file still has nothing to reset to.
        assert!(config_seed_body(&client_config_path(Some("abc-123"), "nonesuch.toml")).is_none());
    }

    #[test]
    fn scroll_seed_body_resolves_client_shared_and_reset_of_an_override_falls_back() {
        // The shared client default resolves to the scroll body.
        assert_eq!(
            config_seed_body(&client_config_path(None, "scroll.toml")),
            Some(DEFAULT_SCROLL)
        );
        // A per-client override path carries NO seed of its own — reset-to-embedded
        // restores it to the shared client default (same file name).
        assert_eq!(
            config_seed_body(&client_config_path(Some("abc-123"), "scroll.toml")),
            Some(DEFAULT_SCROLL),
            "resetting a per-client scroll override restores the shared default",
        );
    }

    #[test]
    fn theme_default_parses_as_toml() {
        let v: toml::Value = toml::from_str(DEFAULT_THEME).expect("theme default is valid TOML");
        assert!(v.get("bg").is_some(), "theme default carries bg");
    }
}

#[cfg(test)]
mod auxiliary_prompt_loader_tests {
    use super::*;
    use crate::vfs::{LocalBackend, MountTable};
    use kaijutsu_types::paths::CONFIG_ROOT;
    use std::sync::Arc;

    async fn config_vfs() -> (Arc<MountTable>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vfs = Arc::new(MountTable::new());
        vfs.mount(CONFIG_ROOT, LocalBackend::new(dir.path())).await;
        (vfs, dir)
    }

    #[tokio::test]
    async fn auxiliary_prompt_loader_reads_the_live_host_body() {
        let (vfs, dir) = config_vfs().await;
        std::fs::write(dir.path().join("distillation.md"), "LIVE-DRIFT-BRIEF").unwrap();
        std::fs::write(dir.path().join("continuation.md"), "LIVE-CONTINUATION").unwrap();

        assert_eq!(load_distillation_prompt(vfs.as_ref()).await.unwrap(), "LIVE-DRIFT-BRIEF");
        assert_eq!(load_continuation_prompt(vfs.as_ref()).await.unwrap(), "LIVE-CONTINUATION");
    }

    #[tokio::test]
    async fn missing_auxiliary_prompt_fails_instead_of_reviving_an_embedded_body() {
        let (vfs, _dir) = config_vfs().await;

        let err = load_distillation_prompt(vfs.as_ref()).await.unwrap_err();
        assert!(err.contains("/config/kernel/distillation.md"));
    }
}

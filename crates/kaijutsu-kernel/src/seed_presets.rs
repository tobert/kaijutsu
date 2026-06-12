//! Factory presets — reserved-name patches seeded into the DB on kernel init.
//!
//! The preset analogue of the rc `seed_scripts` floor pattern, but for
//! `presets` rows rather than files (presets are DB-native). See
//! `docs/fork-filters.md` ("Presets = patch recall"). The three factory
//! presets move **no model knobs**; they differ only in their fork `base`
//! selector — the selection base the recall builds at fork time:
//!
//! - `full`   → everything (a fresh full-power lineage / new KV cache)
//! - `window` → prefix + sliding tail from the parent's hydration policy row
//! - `spawn`  → ~nothing (a lean player birth; rc rebuilds setup)
//!
//! Stored normalized as `preset_args(verb="fork", arg_name="base",
//! arg_value=<selector>)`, so a user preset can extend the same shape (e.g. a
//! `player` patch = base=spawn + a local model). Recall (slice 3c) reads
//! `base` to construct the `IntervalSet`.

use kaijutsu_types::{ConsentMode, PresetId, PrincipalId};

use crate::kernel_db::{KernelDb, KernelDbResult, PresetArg, PresetRow};

/// A factory preset definition: a reserved label + its fork `base` selector.
struct FactoryPreset {
    label: &'static str,
    description: &'static str,
    /// The `base` arg value under verb `fork` — the selection base selector.
    base: &'static str,
}

const FACTORY_PRESETS: &[FactoryPreset] = &[
    FactoryPreset {
        label: "full",
        description: "All blocks — a fresh full-power lineage (new KV cache).",
        base: "full",
    },
    FactoryPreset {
        label: "window",
        description: "Prefix + sliding tail from the parent's hydration policy (KV reuse).",
        base: "window",
    },
    FactoryPreset {
        label: "spawn",
        description: "~Nothing — a lean player birth; rc rebuilds setup.",
        base: "spawn",
    },
];

/// True when `label` is a reserved factory preset name. Users may not
/// save/overwrite these (the guard lives in the `kj preset save`/`remove`
/// path); `ensure_factory_presets` bypasses it by inserting directly.
/// Case-insensitive, matching the label UNIQUE semantics.
pub fn is_reserved_preset_label(label: &str) -> bool {
    FACTORY_PRESETS
        .iter()
        .any(|p| p.label.eq_ignore_ascii_case(label))
}

/// Idempotently seed the factory presets — the floor pattern: a preset is
/// created only when its label is absent, so a user's later edits to a factory
/// preset survive a reseed-floor pass (force-restore is `reseed_factory_presets`).
/// Returns how many were newly created.
pub fn ensure_factory_presets(db: &mut KernelDb, created_by: PrincipalId) -> KernelDbResult<usize> {
    let mut created = 0;
    for fp in FACTORY_PRESETS {
        if db.get_preset_by_label(fp.label)?.is_some() {
            continue;
        }
        insert_factory_preset(db, fp, created_by)?;
        created += 1;
    }
    Ok(created)
}

/// Force-restore the factory presets from the embedded definitions: delete any
/// existing same-label preset (its `preset_args` cascade with it) and reinsert.
/// The `kj preset reseed` path. Returns how many were restored.
pub fn reseed_factory_presets(db: &mut KernelDb, created_by: PrincipalId) -> KernelDbResult<usize> {
    for fp in FACTORY_PRESETS {
        if let Some(existing) = db.get_preset_by_label(fp.label)? {
            db.delete_preset(existing.preset_id)?;
        }
        insert_factory_preset(db, fp, created_by)?;
    }
    Ok(FACTORY_PRESETS.len())
}

fn insert_factory_preset(
    db: &mut KernelDb,
    fp: &FactoryPreset,
    created_by: PrincipalId,
) -> KernelDbResult<()> {
    let preset_id = PresetId::new();
    db.insert_preset(&PresetRow {
        preset_id,
        label: fp.label.to_string(),
        description: Some(fp.description.to_string()),
        provider: None,
        model: None,
        system_prompt: None,
        consent_mode: ConsentMode::Collaborative,
        created_at: kaijutsu_types::now_millis() as i64,
        created_by,
    })?;
    db.set_preset_args(
        preset_id,
        "fork",
        &[PresetArg {
            arg_name: "base".to_string(),
            arg_value: fp.base.to_string(),
        }],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_of(db: &KernelDb, label: &str) -> String {
        let preset = db.get_preset_by_label(label).unwrap().expect("preset exists");
        let args = db.get_preset_args(preset.preset_id, "fork").unwrap();
        let base = args
            .iter()
            .find(|a| a.arg_name == "base")
            .expect("base arg present");
        base.arg_value.clone()
    }

    #[test]
    fn ensure_is_idempotent_and_seeds_three_bases() {
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::new();

        assert_eq!(ensure_factory_presets(&mut db, who).unwrap(), 3, "first run seeds all three");
        assert_eq!(ensure_factory_presets(&mut db, who).unwrap(), 0, "second run is a no-op");

        assert_eq!(base_of(&db, "full"), "full");
        assert_eq!(base_of(&db, "window"), "window");
        assert_eq!(base_of(&db, "spawn"), "spawn");
        // Factory presets move no model knobs.
        let full = db.get_preset_by_label("full").unwrap().unwrap();
        assert!(full.model.is_none() && full.provider.is_none());
    }

    #[test]
    fn reserved_labels_are_case_insensitive() {
        for l in ["full", "window", "spawn", "FULL", "Window", "SpAwN"] {
            assert!(is_reserved_preset_label(l), "{l} must be reserved");
        }
        assert!(!is_reserved_preset_label("player"));
        assert!(!is_reserved_preset_label("windowed"));
    }

    #[test]
    fn reseed_force_restores_after_user_edit() {
        let mut db = KernelDb::in_memory().unwrap();
        let who = PrincipalId::new();
        ensure_factory_presets(&mut db, who).unwrap();

        // Simulate a clobbered factory preset: replace window's base.
        let win = db.get_preset_by_label("window").unwrap().unwrap();
        db.set_preset_args(
            win.preset_id,
            "fork",
            &[PresetArg { arg_name: "base".into(), arg_value: "garbage".into() }],
        )
        .unwrap();
        assert_eq!(base_of(&db, "window"), "garbage");

        // Reseed force-restores the embedded definition.
        assert_eq!(reseed_factory_presets(&mut db, who).unwrap(), 3);
        assert_eq!(base_of(&db, "window"), "window");
        // Still exactly three factory presets (no duplicates from re-insert).
        let labels: Vec<_> = db
            .list_presets()
            .unwrap()
            .into_iter()
            .filter(|p| is_reserved_preset_label(&p.label))
            .collect();
        assert_eq!(labels.len(), 3);
    }
}

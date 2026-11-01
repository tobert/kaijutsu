use super::*;

fn arg(name: &str, value: &str) -> PresetArg {
    PresetArg { arg_name: name.into(), arg_value: value.into() }
}

#[test]
fn preset_args_roundtrip_verb_scoped_and_dedup() {
    let mut db = KernelDb::temporary().unwrap();
    let pid = insert_test_preset(&db, "window");

    assert!(db.get_preset_args(pid, "fork").unwrap().is_empty());

    // Repeated --exclude plus a duplicate that must collapse via the PK.
    db.set_preset_args(
        pid,
        "fork",
        &[
            arg("exclude", "10:12"),
            arg("exclude", "0:5"),
            arg("exclude", "0:5"),
            arg("window", "16"),
        ],
    )
    .unwrap();

    // Ordered by (arg_name, arg_value); the dup is gone.
    assert_eq!(
        db.get_preset_args(pid, "fork").unwrap(),
        vec![arg("exclude", "0:5"), arg("exclude", "10:12"), arg("window", "16")]
    );

    // Verb-scoped: another verb is independent and doesn't disturb fork.
    db.set_preset_args(pid, "context", &[arg("model", "x")]).unwrap();
    assert_eq!(db.get_preset_args(pid, "fork").unwrap().len(), 3);

    // Replace semantics: re-setting fork wipes the prior set.
    db.set_preset_args(pid, "fork", &[arg("include", "end-5:")]).unwrap();
    assert_eq!(db.get_preset_args(pid, "fork").unwrap(), vec![arg("include", "end-5:")]);

    // Empty clears.
    db.set_preset_args(pid, "fork", &[]).unwrap();
    assert!(db.get_preset_args(pid, "fork").unwrap().is_empty());
}

#[test]
fn preset_args_cascade_on_preset_delete() {
    let mut db = KernelDb::temporary().unwrap();
    let pid = insert_test_preset(&db, "spawn");
    db.set_preset_args(pid, "fork", &[arg("include", ":0")]).unwrap();
    assert_eq!(db.get_preset_args(pid, "fork").unwrap().len(), 1);

    assert!(db.delete_preset(pid).unwrap());
    assert!(
        db.get_preset_args(pid, "fork").unwrap().is_empty(),
        "preset_args must cascade-delete with the preset"
    );
}

#[test]
fn preset_lifecycle() {
    let db = KernelDb::temporary().unwrap();
    let creator = PrincipalId::new();
    let now = now_millis();

    let mut preset = PresetRow {
        preset_id: PresetId::new(),
                    label: "opus-research".into(),
        description: Some("Deep research preset".into()),
        created_at: now,
        created_by: creator,
    };

    db.insert_preset(&preset).unwrap();

    // Get by label
    let loaded = db
        .get_preset_by_label("opus-research")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.description.as_deref(), Some("Deep research preset"));

    // Update
    preset.description = Some("Shallow research preset".into());
    db.update_preset(&preset).unwrap();
    let loaded = db.get_preset(preset.preset_id).unwrap().unwrap();
    assert_eq!(loaded.description.as_deref(), Some("Shallow research preset"));

    // Delete
    assert!(db.delete_preset(preset.preset_id).unwrap());
    assert!(db.get_preset(preset.preset_id).unwrap().is_none());
}

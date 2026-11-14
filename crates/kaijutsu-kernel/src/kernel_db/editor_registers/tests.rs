use super::*;
use kaijutsu_types::editor::RegisterShape;

fn cell(name: char, text: &str, shape: RegisterShape) -> RegisterText {
    RegisterText { name, text: text.to_string(), shape }
}

#[test]
fn rows_round_trip_per_principal() {
    let db = KernelDb::temporary().unwrap();
    let (amy, model) = (PrincipalId::new(), PrincipalId::new());
    let rows = [
        (1, cell('0', "yanked\n", RegisterShape::Linewise)),
        (2, cell('-', "word", RegisterShape::Charwise)),
        (3, cell('a', "col\ncol", RegisterShape::Blockwise)),
    ];
    db.write_editor_registers(amy, &rows).unwrap();
    db.write_editor_registers(model, &[(1, cell('0', "theirs", RegisterShape::Charwise))]).unwrap();

    let mut got = db.editor_registers(amy).unwrap();
    got.sort_by_key(|(seq, _)| *seq);
    assert_eq!(got, rows);
    assert_eq!(db.editor_registers(model).unwrap(), [(1, cell('0', "theirs", RegisterShape::Charwise))]);
    assert!(db.editor_registers(PrincipalId::new()).unwrap().is_empty());
}

#[test]
fn a_later_write_replaces_the_row_of_its_register() {
    let db = KernelDb::temporary().unwrap();
    let amy = PrincipalId::new();
    db.write_editor_registers(amy, &[(1, cell('0', "first", RegisterShape::Charwise))]).unwrap();
    db.write_editor_registers(amy, &[(2, cell('0', "second\n", RegisterShape::Linewise))]).unwrap();
    assert_eq!(db.editor_registers(amy).unwrap(), [(2, cell('0', "second\n", RegisterShape::Linewise))]);
}

/// Loaded rows rebuild the store with the unnamed register equal to the
/// latest row: a delete that shifted the ring, then a yank.
#[test]
fn loaded_rows_rebuild_the_unnamed_register_in_seq_order() {
    let db = KernelDb::temporary().unwrap();
    let amy = PrincipalId::new();
    let mut ring: Vec<(u64, RegisterText)> = ('1'..='9')
        .map(|n| (2, cell(n, if n == '1' { "deleted\n" } else { "" }, RegisterShape::Linewise)))
        .collect();
    ring.push((1, cell('0', "older yank\n", RegisterShape::Linewise)));
    db.write_editor_registers(amy, &ring).unwrap();
    let regs = kaijutsu_editor::Registers::from_rows(db.editor_registers(amy).unwrap()).unwrap();
    assert_eq!(regs.cell('"').map(|c| c.text).as_deref(), Some("deleted\n"));

    db.write_editor_registers(amy, &[(3, cell('0', "newest\n", RegisterShape::Linewise))]).unwrap();
    let regs = kaijutsu_editor::Registers::from_rows(db.editor_registers(amy).unwrap()).unwrap();
    assert_eq!(regs.cell('"').map(|c| c.text).as_deref(), Some("newest\n"));
}

/// A batch is one transaction: a row the table refuses leaves none of the
/// batch behind.
#[test]
fn a_refused_row_writes_nothing() {
    let db = KernelDb::temporary().unwrap();
    let amy = PrincipalId::new();
    let err = db
        .write_editor_registers(amy, &[(1, cell('0', "ok", RegisterShape::Charwise)), (2, cell('"', "unnamed", RegisterShape::Charwise))])
        .unwrap_err();
    assert!(err.to_string().contains("'\"'"), "{err}");
    assert!(db.editor_registers(amy).unwrap().is_empty());
}

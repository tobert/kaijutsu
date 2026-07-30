//! Structural regression tests for ABC engraving output.
//!
//! Snapshots the deterministic (glyph, geometry-vertex) data
//! `text::msdf::music_bridge`'s `collect_music_glyphs`/`collect_music_geometry`
//! produce for each ABC fixture — NOT rendered pixels. A pixel-perfect
//! comparison stopped being meaningful once ABC moved off vello: noteheads
//! render as MSDF atlas quads (an async GPU pipeline a plain `cargo test`
//! can't drive) and geometry renders as flat-colored triangles with no
//! path rasterizer at all — there is no `vello::Scene` left to rasterize.
//!
//! What IS deterministic and testable without a GPU is the position data
//! both pipelines consume: glyph identity/x/y/font_size and geometry
//! triangle vertices. Pinning that catches the same class of regression the
//! old pixel goldens caught (octave mapping, beam/stem placement, barline
//! counts, staff alignment) with MORE precision (no antialiasing/RMSE fuzz,
//! no rasterizer-version drift) and less machinery (no headless GPU, no
//! wgpu, no PNG — and no more silent `SKIP` when no GPU adapter is
//! available, which the old harness could do even in CI).
//!
//! Text elements (titles/chord symbols) aren't covered here — none of the
//! fixture tunes use `T:`/`C:` fields (see each `.abc` file), and
//! `collect_music_text_glyphs` needs a real shaped `VelloFont`, which isn't
//! worth plumbing into a unit test for fixtures that don't exercise it.
//!
//! Set `UPDATE_GOLDENS=1` to regenerate a snapshot after an intentional
//! engraving change — inspect the diff before trusting it.

use bevy::prelude::{Assets, Image};
use kaijutsu_abc::engrave::{layout, EngravingOptions};
use kaijutsu_abc::parse;
use peniko::{Brush, Color};
use std::path::PathBuf;

use crate::text::msdf::{collect_music_geometry, collect_music_glyphs, MsdfAtlas};

fn goldens_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/text/abc_goldens")
}

/// Deterministic text snapshot of a tune's engraved glyph + geometry data,
/// in block-local pixel space at `block_scale = 1.0` (i.e. the raw IR
/// units, origin-shifted) — the same transform every render path applies,
/// just without a particular block's width/scale-to-fit folded in, so the
/// snapshot is independent of any UI layout decision.
fn snapshot(source: &str) -> String {
    let parsed = parse(source);
    assert!(
        !parsed.has_errors(),
        "ABC parse errors:\n{:#?}",
        parsed.errors().collect::<Vec<_>>()
    );
    let tune = parsed.value.first().expect("ABC source produced no tunes");

    let opts = EngravingOptions::default();
    let elements = layout::engrave(tune, &opts);
    let bounds = crate::text::abc::compute_engraving_bounds(&elements, opts.margin);
    let origin = (bounds.origin_x, bounds.origin_y);

    let brush = Brush::Solid(Color::WHITE);
    let mut images = Assets::<Image>::default();
    let mut atlas = MsdfAtlas::new(&mut images, 64, 64);

    let glyphs = collect_music_glyphs(&elements, (0.0, 0.0), origin, 1.0, &brush, &mut atlas);
    let geometry = collect_music_geometry(&elements, (0.0, 0.0), origin, 1.0, &brush);

    let mut out = String::new();
    out.push_str(&format!("bounds: {:.3} x {:.3}\n", bounds.width, bounds.height));

    out.push_str(&format!("glyphs: {}\n", glyphs.len()));
    for g in &glyphs {
        out.push_str(&format!(
            "  glyph_id={} x={:.2} y={:.2} size={:.3}\n",
            g.key.glyph_id, g.x, g.y, g.font_size,
        ));
    }

    let triangles = geometry.len() / 3;
    out.push_str(&format!(
        "geometry: {} vertices ({} triangles)\n",
        geometry.len(),
        triangles,
    ));
    for tri in geometry.chunks_exact(3) {
        out.push_str(&format!(
            "  ({:.2},{:.2}) ({:.2},{:.2}) ({:.2},{:.2})\n",
            tri[0].x, tri[0].y, tri[1].x, tri[1].y, tri[2].x, tri[2].y,
        ));
    }

    out
}

/// Compare `actual` against the golden at `goldens_dir()/<name>.snapshot.txt`.
///
/// - `UPDATE_GOLDENS=1` set → overwrite the golden, succeed.
/// - golden missing → write it, fail with a clear message (so CI can't
///   silently accept a new golden).
/// - mismatch → fail with a line-level diff summary.
fn assert_matches_golden(name: &str, actual: &str) {
    let path = goldens_dir().join(format!("{name}.snapshot.txt"));
    let update = std::env::var_os("UPDATE_GOLDENS").is_some();

    if update {
        std::fs::write(&path, actual).expect("write updated golden");
        eprintln!("UPDATE_GOLDENS: wrote {}", path.display());
        return;
    }

    if !path.exists() {
        std::fs::write(&path, actual).expect("write initial golden");
        panic!(
            "golden missing — wrote initial {}. Inspect it, then re-run.",
            path.display()
        );
    }

    let golden = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read golden {}: {e}", path.display()));

    if golden != actual {
        let golden_lines: Vec<&str> = golden.lines().collect();
        let actual_lines: Vec<&str> = actual.lines().collect();
        let mut diff = String::new();
        for i in 0..golden_lines.len().max(actual_lines.len()) {
            let g = golden_lines.get(i).copied().unwrap_or("<missing>");
            let a = actual_lines.get(i).copied().unwrap_or("<missing>");
            if g != a {
                diff.push_str(&format!("  line {i}: golden={g:?} actual={a:?}\n"));
            }
        }
        panic!(
            "golden mismatch for {name} (set UPDATE_GOLDENS=1 to regenerate \
             after inspecting this diff):\n{diff}"
        );
    }
}

fn run_case(name: &str) {
    let source = std::fs::read_to_string(goldens_dir().join(format!("{name}.abc")))
        .unwrap_or_else(|e| panic!("read {name}.abc: {e}"));
    let actual = snapshot(&source);
    assert_matches_golden(name, &actual);
}

#[test]
fn single_bar_c_major_quarter_notes() {
    run_case("single_bar");
}

#[test]
fn chord_with_accidentals() {
    run_case("chord_accidentals");
}

#[test]
fn beamed_eighths_sixteenths() {
    run_case("beamed_eighths");
}

/// Pure pitch→Y regression: chromatic quarter notes from C4 up to C6 and
/// back (sharps ascending, flats descending), single treble staff, no
/// beams or slurs. Spans one ledger below the staff to two above, so a
/// notehead height or ledger-count shift — the octave bug that put every
/// note an octave too low — shows immediately. Beaming/slurs are covered
/// by other goldens; this one isolates the mapping.
#[test]
fn chromatic_run_octave_regression() {
    run_case("chromatic_run");
}

/// Multi-voice regression: a 4-bar SATB hymn across four stacked staves
/// (S/A treble, T/B bass). This is the only golden that exercises the
/// multi-staff path in `layout::engrave` — independent per-voice layout,
/// cross-voice staff-width normalization, and barline alignment between
/// staves of equal per-bar duration. It also mixes half/quarter/eighth
/// values (one beamed eighth pair in the soprano) so a per-voice cursor
/// or duration-width regression that drifts the staves out of vertical
/// alignment shows up immediately.
#[test]
fn hymn_four_part_satb() {
    run_case("hymn_satb");
}

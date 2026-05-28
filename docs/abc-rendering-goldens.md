# ABC Rendering — Golden-Image Test Harness

Status: **scaffolding landed, three baseline goldens, several visible bugs
queued for follow-up.** This doc is the pickup point.

## What's here

A headless rasterizer that re-uses the exact `vello::Scene` the app builds
for ABC blocks, so you can iterate on `kaijutsu-abc` engraving without the
screenshot loop.

```
crates/kaijutsu-app/
├── Cargo.toml                       # added `pollster = "0.4"` dev-dep (image was already there)
└── src/text/
    ├── abc.rs                       # render_engraving_to_scene  (unchanged behaviour)
    │                                # + `#[cfg(test)] mod golden_tests;` at bottom
    ├── abc/
    │   └── golden_tests.rs          # the harness + 3 tests
    └── abc_goldens/
        ├── single_bar.abc           # X:1 M:4/4 L:1/4 K:C  CDEF|
        ├── single_bar.png
        ├── chord_accidentals.abc    # [C^EG] [_BDF] ^c =F
        ├── chord_accidentals.png
        ├── beamed_eighths.abc       # CDEF GABc | cBAG FEDC  (L:1/8)
        └── beamed_eighths.png
```

## Running

```bash
# normal: compare against goldens (RMSE ≤ 0.5%, every-channel delta ≤ 2)
cargo test -p kaijutsu-app text::abc::golden_tests

# bless current output as the new golden (use after intentional changes)
UPDATE_GOLDENS=1 cargo test -p kaijutsu-app text::abc::golden_tests
```

On mismatch the test dumps `<name>.actual.png` next to the golden so you
can `feh src/text/abc_goldens/*.actual.png` and compare.

## Design choices worth re-reading before extending

- **Same Scene the app uses.** The test calls `text::abc::render_engraving_to_scene`
  directly. No code-path duplication — anything the test catches is real,
  anything it misses is real too. Compositing-level concerns (block texture
  scaling, MSDF text overlay) are deliberately outside scope.
- **wgpu init via `OnceLock`.** First test pays ~80ms shader compile, the rest
  are free. Adapter picked via `wgpu::util::initialize_adapter_from_env_or_default`
  so `WGPU_BACKEND=vulkan` etc. still works. If no adapter is available the
  tests print `SKIP` and pass — they shouldn't gate CI on a GPU we don't have.
- **4× render scale.** Engraving IR coords are tiny (`staff_spacing = 10.0`).
  We scale the scene 4× before rasterizing so subpixel geometry bugs read
  visibly in goldens. Tradeoff is golden size (~40–90 KB each); seemed fine.
- **Tight tolerance.** RMSE 0.5%, per-channel ≤ 2. Catches a 2px stem shift;
  tolerates GPU/driver fuzz. Loosen carefully — most ABC bugs are geometric
  and large.
- **No text-field snippets yet.** All three baselines have no `T:`/`C:` lines,
  so we never need a `VelloFont` (Text-element branch in `render_engraving_to_scene`
  silently no-ops with `font: None`). Adding a lyric/title golden means
  either constructing a `VelloFont` in the test or refactoring the Text
  branch to use raw Parley.
- **Where this lives.** `kaijutsu-app` has no `lib.rs` (it's a binary-only
  crate), so an integration test in `tests/` can't import internals. The
  test is a `#[cfg(test)] mod golden_tests` submodule of `text::abc` — a bit
  unusual, but zero structural change to the crate.

## Bugs visible in the baseline goldens

These are what I noticed at a glance; pick whichever to chase next.

| Golden | Symptom | Hypothesis |
|---|---|---|
| `single_bar.png` | CDEF quarter noteheads sit ~3 staff-spacings *below* the staff with very long stems; horizontal slashes through each stem look like ghost beams. | Pitch-to-Y mapping is off by an octave (or the staff Y origin is wrong); the "slashes" may actually be ledger lines stacked too densely because the notes are wrongly far below the staff. Look at `engrave/layout.rs` — pitch-to-staff-position function and the ledger-line emitter. |
| `chord_accidentals.png` | Sharp glyph sits to the *right* of its notehead instead of to the left; chord stems show multiple horizontal bars (as if quarter chord is being beamed). | Accidental placement is post-positioning the head instead of pre-positioning. The "bars" likely tied to the same ledger/Y issue as `single_bar`. |
| `beamed_eighths.png` | Beam slope is far steeper than the contour of the heads warrants. | Beam-Y computed from extremes (first/last head Y), no slope clamping. Standard rule: slope ∝ Δpitch, clamped to ±~0.25 of beam length. |

The Y-mapping bug is likely the contributing factor for several of these
— fixing it first will probably re-bless all three goldens at once.

## Iterating on a bug

1. Eyeball `src/text/abc_goldens/<name>.png`. Identify what's wrong.
2. Fix it in `kaijutsu-abc/src/engrave/layout.rs` (or wherever).
3. `cargo test -p kaijutsu-app text::abc::golden_tests` — see the failure
   diff path, open the `.actual.png`.
4. Iterate. When happy, `UPDATE_GOLDENS=1 cargo test …` and commit the
   new golden alongside the fix.

## Extending the suite

To add a snippet `foo`:

1. Drop `src/text/abc_goldens/foo.abc`.
2. Add `#[test] fn foo() { run_case("foo"); }` to `golden_tests.rs`.
3. `UPDATE_GOLDENS=1 cargo test …` to mint `foo.png`.
4. Eyeball the PNG, then commit.

Candidate snippets to add when the existing three are clean:

- Repeats + 1st/2nd endings (barlines, bracket geometry)
- Chromatic run through the staff (regression for the Y bug above)
- Multi-voice / voice overlay
- A snippet with a title (forces VelloFont plumbing decision)

## Known limitations

- Single-line ABC only — `layout::engrave` doesn't line-break yet, and
  neither does the harness.
- Whatever the app does to *fit* the scene into a block texture (the
  `w_scale.min(h_scale)` dance in `view::block_render.rs`) is out of scope
  here. Test the engraving; don't test the compositor.
- If wgpu init fails (no adapter) the tests skip silently. If you want CI
  to fail in that case, replace the `eprintln!("SKIP …")` in `run_case`
  with a `panic!`.

## Pointers

- Pipeline: `parse → engrave::layout::engrave → render_engraving_to_scene → vello::Renderer::render_to_texture`
- IR: `crates/kaijutsu-abc/src/engrave/ir.rs` (`EngravingElement`)
- Layout: `crates/kaijutsu-abc/src/engrave/layout.rs` (1.9k lines — pitch→Y, beams, accidental placement live here)
- Vello scene builder: `crates/kaijutsu-app/src/text/abc.rs`
- App-side caller (for the production path): `crates/kaijutsu-app/src/view/block_render.rs:497`

# Bevy 0.18.1 → 0.19 upgrade plan

**Status: PLANNING ONLY.** Amy asked for a plan while she reads the drift UX
doc; nothing here is executed. Written 2026-08-12.

Planned against the **104 official migration guides** in the `v0.19.0` tag
(`_release-content/migration-guides/`), cross-referenced against actual usage
in `kaijutsu-app` (149 files, ~73k lines) by four parallel sweeps. Every
finding below carries a `file:line`, and every load-bearing claim was
re-verified by the lead against source before landing here.

## Premises (verified, and three of them are traps)

1. **Bevy 0.19.0 is released.** `refs/tags/v0.19.0` exists upstream.
2. **`~/src/bevy` was five months stale** — last fetched 2026-03-02, sitting
   at 0.18.1. The `v0.19.0` tag is now fetched. Planning off that checkout
   without fetching would have concluded 0.19 did not exist.
3. **`bevy_brp_extras = "0.19"` is NOT Bevy 0.19** — and this is worse than
   cosmetic, see BLOCKER 1.
4. **No toolchain blocker.** MSRV 1.89.0 → **1.95.0**; local toolchain is
   1.96.0. Edition stays 2024.
5. Model training data predates 0.18's own event-system rename, so any 0.19
   claim sourced from memory is suspect by construction. In practice one
   confident-sounding claim per sweep needed correcting.

## Bottom line

**This is a small upgrade wearing a scary hat.** 0.19's headline reworks —
text-stack rewrite onto parley, resources-as-components, render-graph-as-
systems, extract refactor — are all near-misses for this app, because we
never used the APIs they changed. Of 104 guides, **7 touch us**.

The real work is concentrated in four places, and only one of them is large:

| # | Item | Kind | Effort |
|---|---|---|---|
| 1 | `bevy_brp_extras` pin | blocker, dependency | 1 line |
| 2 | ~~vello 0.7.0 → 0.9.0~~ — MOOT, vello retired from the app 2026-08-12 | n/a | 0 |
| 3 | 4 mechanical compile fixes (imports + one field rename) | blocker, code | ~9 files, ~20 min |
| 4 | `Assets::get_mut` needs `mut` bindings | mechanical, code | 24 sites / 15 files, 1–2 h |
| 5 | **rodio 0.20 vs 0.22 divergence** | **decision + possible rewrite** | 0 to ~2 days |
| 6 | Bloom re-tune | visual QA | 30–60 min |

Everything except (5) is a day's work with testing. (5) is the only item that
deserves a real decision, and it is invisible from the Bevy guides.

## Blockers — nothing compiles until these are done

### 1. `bevy_brp_extras` pin does not resolve (dependency)

`bevy_brp_extras = "0.19"` (workspace `Cargo.toml:118`) resolves to a release
whose own manifest pins **`bevy = "0.18.1"`**. Verified from the cached
crates: `bevy_brp_extras 0.19.0 → bevy 0.18.1`, `0.21.0 → bevy 0.19.0`.

So the crate whose version number reads "0.19" is the one thing pinning us to
0.18. Cargo cannot resolve two incompatible `bevy` requirements, so the build
fails at resolution time, before any code error. **Bump to `"0.21"` or
`"0.22"`.**

This matters beyond the app: BRP is how agents drive the live GUI for testing
(`contrib/kaijutsu-runner.sh`). A broken BRP costs the autonomous testing
loop, not just a feature.

### 2. vello — MOOT, retired from the app (was: "must move in the same commit")

This whole blocker no longer exists. It was written against a checkout where
`view/vello_rasterizer.rs` handed Bevy's wgpu device straight into
`vello::Renderer::new` (needing vello's wgpu pin to match Bevy's exactly,
hence "atomic with the bump" below) — but vello was retired from
`kaijutsu-app` entirely on 2026-08-12 (docs/issues.md), independently of this
plan and before it was executed. `vello_rasterizer.rs` is deleted, the vello
half of `ui_rtt.rs` (`extract_vello_scenes`/`render_vello_scenes`,
`UiVectorScene`) is deleted, and `vello` is absent from
`kaijutsu-app/Cargo.toml` and the whole workspace's `Cargo.lock` (verified
via `cargo tree -i vello`). The dock chrome that was vello's last consumer
now renders through the same MSDF pipeline block cells use. `kurbo`/`peniko`
stay as direct dependencies (Parley's own types), unaffected by this either
way. No GPU smoke test of a vello rasterizer is needed for this upgrade —
there is no vello rasterizer left to test.

### 3. Four mechanical compile fixes

- **`AlphaMode` moved to `bevy_material`.** Reached today via
  `use bevy::prelude::*`; in 0.19 it is exported only from `bevy_material` and
  is *not* re-exported through `bevy::prelude` or `bevy_pbr::prelude`. Add
  `use bevy::material::AlphaMode;` to 6 files: `shaders/chord_material.rs:48`,
  `trace_glow_material.rs:67`, `well_card_material.rs:75`,
  `terrace_ring_material.rs:80`, `track_ray_material.rs:49`,
  `well_rings_material.rs:58`.
- **`Hdr` moved to `bevy_camera`.** `main.rs:333` imports
  `bevy::render::view::Hdr`; becomes `bevy::camera::Hdr` (not in that crate's
  prelude, so the explicit path is required).
- **`push_constant_ranges` → `immediate_size: u32`.** *Not in the guide set* —
  found by direct source comparison. `text/msdf/renderer.rs:202` and
  `music_geometry_renderer.rs:99` both pass `vec![]`; becomes
  `immediate_size: 0`.
- **`TextFont` field types.** `ui/tiling_reconciler.rs:539-543` — `font` needs
  `.into()` (now `FontSource`), `font_size: 14.0` becomes `FontSize::Px(14.0)`.

### 4. `Assets::get_mut` returns `AssetMut<A>`

Needs a `mut` binding to `DerefMut`, so `if let Some(x) = materials.get_mut(..)`
becomes `if let Some(mut x) = ...`. **24 sites across 15 files** — the widest
mechanical change in the upgrade. Concentrated in `view/time_well/*` (9),
`text/msdf/atlas.rs` (3), `view/fsn/*`, `view/room/mod.rs`, `shaders/mod.rs`,
and others. The compiler finds every one; no semantic risk.

## The one real decision: rodio

**This is the biggest scoping risk and it is invisible from the Bevy guides.**
`rodio_0_22.md` documents only Bevy's own feature-flag churn.

Bevy 0.19's `bevy_audio` requires **rodio ^0.22**. rodio 0.22 **deleted**
`OutputStream`, `OutputStreamHandle` and `Sink` outright, replacing them with
`DeviceSinkBuilder` / `MixerDeviceSink` / `Player`.

We hold a **direct** `rodio = "0.20"` (`kaijutsu-app/Cargo.toml:34`), and
`audio_sched.rs` (868 lines — the entire scheduled-playback engine) is built
on exactly the deleted API: `audio_sched.rs:37` (imports), `:306-312`
(`Sink::try_new`, `sink.append`), `:381` (`OutputStream::try_default`), plus
`live: Vec<Sink>` as the cancel/flush mechanism.

Critically, the manifest comment at `Cargo.toml:32-33` states the design
invariant — *"bevy still pulls rodio 0.20 transitively; same semver range, so
cpal/rodio dedupe to one copy."* **0.19 falsifies that comment.** Two paths:

- **A — hold at 0.20.** Accept two rodio/cpal copies in the tree. Almost
  certainly still compiles (our code uses our copy; `bevy_audio`'s plugin is
  disabled at `main.rs:179` so it never opens a device). Cost: larger binary,
  longer builds, and a documented architecture invariant that is now false and
  must be rewritten rather than left to mislead. **Verify no double ALSA
  device open** before accepting.
- **B — move to 0.22.** Restores the single-copy invariant, but is a genuine
  rewrite of `audio_sched.rs`'s device-open / sink-fire / flush core around
  `Player` or raw `Mixer::add` — not a mechanical rename. Needs a design
  decision on which replaces the `Sink`-per-cue + `Vec<Sink>` cancel model.

**Recommendation: A now, B as its own scoped piece of work.** Bundling a
playback-engine rewrite into a framework bump makes both harder to review and
harder to bisect. But A is only acceptable if the stale comment is corrected
in the same commit — leaving it is precisely the model-facing-drift failure we
promoted a constraint about this morning.

## Visual regression: bloom

0.19 fixes Karis-average luma to compute in linear rather than sRGB space,
which dims saturated bloom. We use `Bloom` heavily with by-eye-tuned values —
`main.rs:352-360` (threshold explicitly a literal "on purpose") and
hot-reloadable theme presets at `view/scene_palette.rs:280-293`. The
"HDR-tell boundary" the well-card glow depends on (`main.rs:341-344`) is
exactly the saturated-color trick this fix changes.

No code change; budget a by-eye re-tune pass. **This is the one item that
needs Amy at the controls on moltar**, since it is a taste judgment about how
the instrument looks.

## What is NOT affected (the good news, recorded so nobody re-derives it)

- **Text/parley: near-zero.** We already depend on `parley = "0.7.0"` directly
  and have **zero** `bevy_text` usage in `src/text/`. Own asset loader, own
  atlas, own `PositionedGlyph`/`FontId`, direct-to-MSDF glyphs (no vello in
  the path at all, since 2026-08-12). Bevy's native text appears in exactly
  one place app-wide.
- **resources_as_components: 4 lines.** `Res`/`ResMut` sugar retained, so
  **687** use sites across 57 files need no change. Only
  `insert_non_send_resource` → `insert_non_send` (deprecated, not removed) at
  `midi_in.rs:60,711,750` and `view/patch_bay/mod.rs:402`. Every dangerous
  edge — dual `Component`+`Resource` derives, generic `<R: Resource>` bounds,
  broad queries, manual `Access`/`ReflectResource` — verified **zero** in 73k
  lines.
- **Render graph / extract: zero.** No `ViewNode`, no `RenderGraph`, no
  `ExtractComponent`. Our render-world code (`ui_rtt.rs:340-356`,
  `block_render.rs:328-398`) is already written the 0.19 way: plain systems on
  `ExtractSchedule`/`Render`.
- **UI/input: zero.** Hand-rolled UI throughout; no Bevy widgets, no
  `InputFocus` (we own `FocusArea`), no `ViewportNode`, no feathers/BSN. All
  30 `Node { .. }` literals use `..default()`, so the new `direction` field is
  free.
- **HiDPI units: unaffected.** Explicitly checked, because this is our known
  silent-breakage trap — no 0.19 guide changes `ComputedNode` field types,
  `GlobalTransform`, `Val::Px`, font sizes or `ScrollPosition`. The one
  `ComputedNode` change removes `stack_index`, which we never read.
- **Bevy scenes, gltf, animation, morph targets, atmosphere, skybox, gizmos,
  occlusion culling, android, wasm:** all unused.

**Trap recorded:** `set_serif_family` / `set_monospace_family` at
`text/plugin.rs:209-211` look like Bevy font-fallback hits. They are
`usvg::fontdb::Database` calls. A naive grep flags them as migration work.

## MIDI / presence: unaffected (Amy's specific question)

0.19 does **not** move the MIDI or presence code. MIDI I/O is ALSA-direct
(`midi_in.rs:361+`) and `main.rs:179` disables `bevy::audio::AudioPlugin`
outright, so the audio churn has no path to us. The only MIDI-adjacent edit in
the entire upgrade is the `insert_non_send` rename.

**Consequence: presence work (drift gap #5) and this upgrade are
independent.** Neither blocks the other.

## Drift-lane interaction: none — the lanes are independent

Checked directly. `connection/drift.rs` has **no Bevy UI surface at all** — it
is state-only, setting `DriftState.notification`; the toast renders from
`ui/dock.rs:1516`, and `dock.rs`'s `Node` literals and text measurement are
already 0.19-clean.

So **the wake-shape work (gap #2, shapes B/D) has no ordering dependency on
this upgrade**, in either direction. If Amy rules on drift (c) tomorrow, that
work can proceed on 0.18 and survive the bump untouched. The upgrade does not
need to be sequenced around the drift lane, and the drift lane does not need
to wait.

The one shared resource is *Amy's attention on moltar*: the bloom re-tune and
any live drift-wake testing both want the GUI and her eyes. Sequence those on
her availability, not on technical dependency.

## Suggested sequence

Each step should build and test before the next; steps 2–4 are one atomic
commit because there is no compiling state between them.

1. **Prep, no Bevy change.** Fix the 24 `Assets::get_mut` sites and the 4
   `insert_non_send_resource` calls. Both are valid on 0.18 (the latter is
   only *deprecated* in 0.19), so this de-risks the big commit and can be
   reviewed on its own.
2. **The bump, atomically:** `bevy` 0.18→0.19,
   `bevy_brp_extras` 0.19→0.21+, `bevy_remote` and `bevy_mesh` 0.18→0.19,
   `cargo update -p bevy_tweening` (upstream already merged 0.19 support).
3. **Same commit:** the four mechanical compile fixes (`AlphaMode` ×6, `Hdr`,
   `immediate_size` ×2, `TextFont`).
4. **Same commit:** correct the now-false rodio dedupe comment at
   `Cargo.toml:32-33` under path A.
5. **Verify on moltar:** BRP round-trip (the testing loop depends on it);
   MIDI in/out unchanged; then Amy's bloom re-tune pass.
6. **Separately, later:** the rodio 0.22 migration (path B), if we want the
   single-copy invariant back.

## Open questions for Amy

1. **rodio path A or B?** A is a one-line comment fix plus accepting two
   copies; B is a scoped rewrite of `audio_sched.rs`'s core. My
   recommendation is A now, B later and on its own.
2. **Is the bloom re-tune yours or ours?** It is a taste call about how the
   instrument looks; I would rather hand you a build and a knob than guess.
3. **When?** Nothing here is urgent — 0.18.1 is working. The strongest
   argument for going sooner is that the ecosystem crates already moved, so we
   are the laggard, and `bevy_brp_extras` will keep drifting away from a pin
   that reads deceptively current.

# VFS Landscape — Station Design (the fsn world)

2026-07-07, expanded 2026-07-12 after concept waves 1–5. Station #5 of the
scenes charter (`docs/scenes/README.md`). The vocabulary is **baked**
(Amy, wave-5 cull); implementation is sequenced behind the time-well HUD
melt landing. Canonical concepts: `docs/scenes/concepts/43-vfs-fsn-landscape.png`
(lineage) and `44..49-vfs-world-*.png` (the world vocabulary — keepers of
waves 1–5; the full wave story lives in the session galleries + git history
of the culled set in `~/kj-junk/scenes-mockups-2026-07/`).

**Lineage** (Amy, 2026-07): **fsn**, SGI's experimental 3D file browser for
IRIX — the "It's a UNIX system! I know this!" scene in Jurassic Park. A
flying view over a dark plane where directories are pedestals, files are
blocks, and luminous avenues connect parent to child. The homage is
deliberate; the palette is ours (synthwave, `docs/color.md`).

## The world reading

The flat fsn plane grew into a **world** (Amy, wave 1): the octagon room is
a *ship* hovering over the filesystem — a Dyson shell of panels, addressed
like paths, lit like cities. The plane survives intact as the near-LOD
reading of the curved world. The N archway stays the door; the exterior
view is the station's ambient reading (the archway's "horizon glow tracks
VFS churn" grown into a whole world). The ship reads at world scale by its
neon panel trim alone (44) — and from the surface, the ship overhead is the
orientation landmark: you can never be lost, look up. One unprompted gift
kept as a design rule candidate: **avenues arc INTO the ship** — the room's
floor traces continuing out into the world's avenue graph (floor-to-world
continuity).

## The claims (held constant across waves 1–5, all confirmed)

1. **Address ≠ area.** A directory's cell is its stable address; size and
   activity encode as **light and height, never area** — the one decoupling
   that keeps the world recognizable while the tree churns (the whole
   stable-treemap literature in one move).
2. **Path = cell-ID prefix.** S2-style cube-sphere quadtree: six faces,
   4-way subdivision, containment is a prefix check — the sphere's address
   space and the VFS namespace are the same structure. LOD chunks fall out
   for free.
   *Why not hex addressing:* hexagons don't nest — H3's aperture-7 children
   poke outside their parents (containment goes approximate), and a
   hex-tiled sphere is a Goldberg polyhedron carrying exactly 12 pentagon
   defects. Quad keeps prefix containment exact. Goldberg/`hexasphere`
   (already in Bevy's dep tree) stays available as *global rendering
   flavor* only.
3. **Enumeration on demand is the LOD scheduler.** Approach = subdivide =
   `readdir` deeper. Unenumerated subtrees are literally unbuilt shell — a
   Dyson sphere is canonically under construction.
4. **Churn is geology.** Layout changes are local, animated, slow (Sondag's
   local moves); hotspots ride the kernel event stream as city lights. A
   changing world is fine — it changes *where the work is happening*.
5. **Wireframe = not-yet-enumerated** (added wave 5). Solid = materialized.
   The render ladder and the enumeration ladder are the same ladder (46).

## The ground vocabulary: relaxed Voronoi (adopted 2026-07-12)

Amy's geological-hexagon vision (columnar basalt, Giant's Causeway) and the
quadtree reconcile because real basalt isn't a hex grid — it's a **relaxed
Voronoi pattern** from cooling cracks: hexagon-dominant with fives and
sevens mixed in. That pattern falls out of the design already on record:

**The world layout is a pure function of the namespace.** No layout state
exists anywhere — not on the wire, not on disk. Every client computes the
same world from the same names. Per directory cell:

1. **Seed** — each child gets a point inside the parent's quad cell at
   `hash(name)`. Deterministic, stable for the child's life. Cells are
   sized with slack so growth lands in gaps instead of forcing reflows.
2. **Voronoi** — diagram of the seeds, clipped to the cell boundary
   (Delaunay-dual; crates exist).
3. **Relax** — k = 2–3 Lloyd iterations, *fixed*. Each iteration only lets
   a seed feel its immediate Voronoi neighbors, so **k iterations bound the
   blast radius to k neighbor-rings**: a new file is one new seed whose
   arrival visibly cracks and resettles only its neighborhood — Sondag's
   local moves and literal cooling-joint physics from the same mechanism.
   Determinism needs only: fixed k, name-sorted processing order.
4. **Extrude** — each cell becomes a prism. Footprint area is *incidental
   and meaningless by design* (claim 1 — never power-diagram weighting).
   **Height is the free encoding channel** — wanted ("mix some height
   elements in", Amy) but unmapped; candidates: block count, recency, tree
   depth. Decide against real data at slice 0.
5. **Joints are the activity substrate** — the crack network between
   columns. **Magenta-violet at rest** (decided, 47 — the app's ScenePalette
   identity; wave 3's amber was placeholder warmth), warming toward
   gold/HDR with activity per the room's existing tiering rule. Churn
   storms are *content*: `target/` is **not** ignored — a compile fractures
   and glows its district like a rift (49), log-scaled so builds read as
   storms, not floodlights. Gitignored wastes get weather.

**Subdir grammar — bloom = the quiet square** (decided via 48): at rest a
subdirectory is an organic cluster of taller columns in the parent's field.
On approach/selection the cluster **parts like petals** and the child's own
field rises flush with the floor; the square footprint (its quad cell)
appears only as a hair-thin seam. No platform, no pedestal — the address
recedes, contents lead (the boring-nameplates philosophy applied to
geometry). Esc reverses the bloom. The polygon→square emphasis of earlier
drafts was explicitly rejected (Amy: shape may be square, don't emphasize).

## Implementation starts at the wireframe (Amy, wave-5 cull)

Frame 45 is the implementation target: Voronoi prisms as neon violet
edge-lines with magenta vertex points, quad seams as faint dotted
boundaries — **line-list meshes + point sprites, no textures**. This makes
wireframe-first the *cheapest* possible slice 0, and it's semantically
honest under claim 5: the world begins as hologram scaffold and
materializes where attention lands. Solid obsidian + live joints becomes an
upgrade tier, not a prerequisite. Height elements mix in from the start if
a dimension is mapped.

**LOD ladder** (= enumeration ladder): sparse points + faint gridlines
(unenumerated, far) → wireframe prisms (listed-not-visited, mid) → solid
columns + live joints (materialized, near/attended). The wave-3 alternative
mid-tier — the joint network baked to a crack-veined emissive plateau
texture — stays on record for far-but-solid districts.

## Kernel plumbing: enumeration + fsnotify (designed 2026-07-12)

Staged: **(0)** gitignore-glob snapshot → **(1)** poll/diff listing RPC,
generation-stamped per cell → **(2)** inotify → kernel event stream.
Greenfield: no `notify` dep in the workspace yet.

- **The kernel owns the one watcher and all subscriptions**; every client
  shares it through the shared kernel (thin-client rule). fsnotify is noisy;
  raw events never cross the wire.
- **Wire = per-cell activity digests**: ABSOLUTE counters/heat published at
  a bounded cadence — state snapshots, never deltas, so the stream is
  lossy-safe (a missed digest costs nothing). Digest granularity keys on
  cell-ID prefix depth, so **notification granularity ≡ render LOD** — a
  client subscribes at the depth it renders.
- **Heat self-heals**: clients decay heat locally (the room's
  `BearingActivity` decay model) and digests refresh it. No periodic resync
  on this plane; a missed digest is premature cooling, corrected next tick.
- **Structure resyncs by generation, not schedule**: listings carry
  generation stamps (`DocumentEntry::version()` pattern from `slash-v.md`);
  digests carry the current generation per cell, so a stale cached listing
  is detected from the digest itself and re-pulled *per cell*. Full sweep
  only on reconnect (rides the app's existing post-reconnect resync).
  Kernel side: inotify `IN_Q_OVERFLOW` → rescan the affected subtree and
  bump generations — the stage-1 poll/diff machinery doubles as the
  rescanner. An optional low-cadence sweep may exist as a config knob;
  it is insurance, not the mechanism.
- **Chill / low-power toggle** (design now, build later — Amy): a client's
  subscription is just parameters against the kernel's rolling counters —
  depth + cadence; the extreme is poll-on-demand. Serves flaky links and
  battery; an app-side "low power" mode later bundles this with render
  throttling (reactive update mode exists).

## In the shell

The landscape is the type specimen for the shell's **archway** pattern
(`docs/scenes/shell.md`): the north bearing is a doorway showing a live
glimpse — horizon glow tracks VFS churn. Diving steps *through* the arch
and is a **fall**: detail blooms on approach (the transition IS the LOD
demo); Esc is the reverse shot. Interior wiring never leaks onto the world
— the brass sill is the seam. Open (wave-2 frame 11, unresolved): flanking
wall panels rendering the world as content — "a window is a panel whose
content is outside" — fits panels-as-screens cleanly but is undecided.

## Ownership zones and object states

- **Zones follow the mount table** (`docs/mounts.md`,
  `docs/config-crdt-ownership.md`). The pre-world reading — CRDT-owned
  regions warm amber, read-only regions cool cyan behind glass
  (`read_only_shell` made visible) — predates the magenta-violet-at-rest
  decision. Proposed reconciliation, to test in a future wave: at-rest
  joint hue stays magenta-violet everywhere; *zone identity tints the
  boundary/glass treatment* (read-only = cool glass, CRDT-owned = warm seam
  accents); activity always warms toward gold. Not yet decided.
- **rc scripts are objects with visible state**: `kj rc list` already knows
  `[in-sync]` / `[differs from seed]` — staleness renders as a material
  seam of off-color light on the column, not a label. Symlinks
  (`DocKind::Symlink`) render as ghost columns tethered to their targets by
  a light thread. Selection/heat bloom regardless of cell size or depth —
  light doesn't need area.

## Interaction sketch (to be concepted further)

- **Travel by intent**: jump to a path (type it, or follow an avenue); the
  camera flies the route so the tree's shape registers.
- **Select** a column → floating hologram preview; **dive** → the vi editor
  session on that file (`docs/vi.md`) — the landscape is plausibly vi's
  spatial front door. Subdirs use the bloom grammar above.
- Camera keeper (wave 4, frame 19's lesson): monumental
  parent-columns-in-foreground framing when standing at a daughter cell.

## Open questions

1. **Height channel** — wanted, unmapped: block count / recency / depth.
   Decide against real data when slice 0 renders the real tree.
2. **Subdir slack math** — the bloom decides the *look*; the address math
   (how a subdir's quad sub-cell is assigned inside the parent, how much
   slack, collision policy) needs its own design pass.
3. **Windows** (frame 11) — ANSWERED YES, slice 1 (Amy 2026-07-13): the N
   wall renders the world via render-to-texture; retuned same day from two
   flanking portholes to ONE panel-spanning portal (see Status).
4. **Zone tint reconciliation** (above).
5. **Search**: `/` over the landscape (labels/paths, maybe
   `search_similar`) — flying to results vs teleporting.
6. **Truth seams**: the kaish `/v/blobs` overlay shadowing
   (`gotcha_kaish_v_blobs_shadow`) is exactly the kind of split the
   landscape must not paper over — if two surfaces disagree, show the seam.

## Status

**Vocabulary baked** (waves 1–5, 2026-07-11/12; Amy's culls recorded in
place above). Sequencing: the time-well HUD melt lands first. **Slice 0 =
the wireframe world**: real tree from a snapshot listing (stage-0/1
plumbing), quadtree layout + hash-seeded relaxed-Voronoi fields rendered as
line-list wireframe + vertex points, three LOD tiers live, fly + select
only — no interaction beyond that, no solid tier, no fsnotify.

**Layout math (Lane A) SHIPPED 2026-07-12** (`feat/fsn-layout`):
`kaijutsu-viz::fsn` — `CellId` quadtree addressing (explicit-level u64,
cube-sphere-ready, prefix containment), FNV-1a seeds, voronator-backed
Voronoi with fixed k=2 Lloyd (blast radius bounded + trajectory-tested),
`FsnCell::edges()` for line-list meshes. Determinism scope: per compiled
binary (FMA/predicate caveat in the module doc). CellId assignment for
subdir sub-cells is a documented placeholder pending open question 2.

**Kernel plumbing (stage 0 + stage-1 groundwork) SHIPPED 2026-07-12**
(`feat/fsn-snapshot`): `Vfs.snapshot` RPC + `MountTable::snapshot`
(`crates/kaijutsu-kernel/src/vfs/mount.rs`), per-directory listing-generation
stamps, `kj vfs snapshot <path>`, client wrapper. `ignored` classification is
real for LocalBackend-backed subtrees, best-effort precision (see
`docs/issues.md`). Stage 2 (inotify → event stream) not started. Lane C (the
Bevy world renderer) consumes this next.

**Bevy world renderer (Lane C) SHIPPED 2026-07-12** (`feat/fsn-world`,
`crates/kaijutsu-app/src/view/fsn/`): `layout` (pure — world placement,
`height_channel`'s first-candidate mapping, prism/seam/point mesh vertex
builders, the LOD-tier decision, camera clamps; unit-tested), `sync` (the
`vfs_snapshot` poll → `FsnState` cache, one in-flight request at a time,
enumeration-on-demand per claim 3), `scene` (spawn/despawn, the fly camera,
per-field mesh entities, LOD gating, selection). N ("DATA HORIZON") is a
genuine `Screen::Fsn` transition from the room, not a `RoomState::zoomed`
station — the "N stays a dive-THROUGH door" reasoning already recorded in
`room::mod`'s `station_is_zoomable` doc. Reused `StandardMaterial`
(unlit, HDR `base_color`) for the `LineList`/point meshes — no new shader.
Height channel: files by `log2(size)`, directories by capped child count,
symlinks a flat stub (`layout::height_channel`'s own doc — Amy's first
candidate against the real tree, per this doc's Open Question 1). Follow-ups
(staleness invalidation, the seam-grid simplification, subdir bloom, search,
zone tint) tracked in `docs/issues.md`.

**Slice 1 — the ambient world — SHIPPED 2026-07-13** (merges `680ac984` Lane K,
`92d677f0` Lane A, stitch `5cdaf773`; live-verified on zorak). Amy's reframe
set the scope: the scene is **ambient instrumentation, not a file browser** —
the space the vessel inhabits, surfacing the filesystem's ambient data. What
landed:

- **Kernel-native heat (stage "1.5" — the digest stream without inotify)**:
  `MountTable` keeps per-directory ABSOLUTE activity totals bumped at the same
  chokepoint as listing generations (write/truncate/setattr count as heat;
  generations stay structure-only), and `Kernel.subscribeVfsActivity` pushes
  bounded-cadence per-directory digests from a per-connection timer bridge —
  the subscription is literally "parameters against rolling counters" as this
  doc designed (cadence now; depth later). Absolute totals make the stream
  lossy-safe end to end: cap drops, failed sends, torn reads, reconnects, and
  kernel restarts all self-heal. Digest entries carry the directory's current
  listing-generation (the per-cell stale-detection groundwork above). Debug
  surface: `kj vfs activity [path]`.
- **App heat**: `FsnHeat` (decaying, ancestor-attenuated, log-scaled weights —
  storms not floodlights) warms each field's wireframe material toward gold
  and lifts gain; the digest's global delta warms the room's **N archway**
  (`BearingActivity` North) so churn reaches you without diving. First-contact
  and restart digests baseline silently — no false gold storm on connect.
- **Recency glow**: `mtime` (already on the wire) bakes into meshes as
  per-cell vertex-color tints under one composition law — `vertex_tint ×
  material = lerp(base_hue, gold, recency)` — so recency (per-cell, static)
  and heat (per-field, decaying) compose without a second material writer.
- **The vessel tie**: gold octagon **vessel silhouette** (brightens with
  whole-tree heat) — since the 2026-07-13 retune no longer a static
  landmark over the world center: it IS the octagon room, riding the same
  `orbit_pose` the portal camera flies (level, yaw-locked, nose/window
  facing the world), so from a view on/in the fsn you see an octagon
  orbiting, and what it sees is exactly the portal's render. Absent from
  the portal view itself — you can't see yourself out your own window. And
  **the portal** — open
  question 3 answered YES: the N face renders a sparse world impression from
  an off-screen orbiting camera (the app's first true second-camera
  render-to-texture, own render layer, resident exactly while the room is).
  Retuned 2026-07-13 (Amy, same day it shipped): the original two flanking
  portholes both sampled the same square texture — the room saw two squeezed
  copies of a miniature. Now ONE panel-spanning portal (880×470 of the ~954
  visible face) with the render texture matched to the glass's aspect, a
  lower ~25° orbit for a horizon read, and a 1.5× world-footprint bump
  (`ROOT_WORLD_SIZE` 3000, orbit scaled with it) so districts get air. The
  "DATA HORIZON" nameplate and the N marker pylon are gone — both stood in
  front of the view they advertised; `Station::Vfs` joined
  `station_is_room_furniture` (the portal IS the station's face). Diving is
  de-emphasized for now: the world rotating past the glass is the primary
  FSN surface (the `Screen::Fsn` dive still exists behind Enter).

Deprioritized by the ambient reframe (still on record above): subdir bloom,
dive-to-vi, `/` search, staleness-as-correctness. Host weather (cargo-build
storms) still awaits stage-2 inotify — today the world lights where *the
kernel* works, which is the truer embodiment anyway.

Research trail: S2 cell-ID prefix containment · GosperMap · EvoStreets ·
Sondag stable treemaps via local moves · `hexasphere` (Bevy dep tree) ·
H3 aperture-7 (rejected for addressing).

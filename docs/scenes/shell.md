# The Shell — Tardis Room (design)

2026-07-07. The shell is the charter's connective tissue made concrete: the
one continuous room that holds the stations (`docs/scenes/README.md`,
"Topology: shell + dives"). It is **not a station** — it has no data shape of
its own; its job is orientation, ambient awareness, and travel. Canonical
concept: `docs/scenes/concepts/01-shell-atrium.png` (Amy: "the atrium
*rules*") and `06-shell-radiators-nano.png` (the assembled room: bearings
occupied, engraved nameplates, the rainbow trace floor bowing around the
console, violet information radiators between stations — one actively
drawing). Other companion mockups live in the story archive; everything
they taught is prose in this doc.

## The Tardis reading

Amy named it: the room is **Tardis-like**, and the metaphor earns its keep in
four specific ways —

1. **Bigger on the inside.** The app window is the police-box door: a modest
   2D frame that opens into the kernel's interior, a space obviously larger
   than the application around it. The shell should *feel* impossible in
   exactly this way — a vaulted room a desktop window has no business
   containing.
2. **The console room.** One central console everything orbits: the **time
   well is the console**, seated at the room's center (mockup 01 got this
   right unprompted). The well keeps everything it already is
   (`docs/timewell.md` — rings, rank, gate, dives); the shell grows *around*
   it rather than replacing or wrapping it.
3. **Doors to elsewhere.** A Tardis interior has doors that open onto
   whole other spaces. Stations whose worlds are too big for the room —
   the fsn landscape is the type specimen — are **archways at a bearing**,
   showing a live glimpse; diving steps through. Bounded stations (the
   patch-bay circle) can instead be *at* their bearing as furniture.
4. **Reconfigurable interior.** New stations dock at free bearings as they
   are designed and built; the room accretes. (The inventory lives in the
   charter; the shell just gives each entry a place to stand.)

The wave-3 finding stays recorded for a future VR return: at VR scale the
grand atrium naturally compresses to an intimate console room — same
stations, walls of one chamber. Desktop keeps the grand scale.

## Geometry and bearings

**An octagonal chamber** (Amy, 2026-07-10 — superseding the circular-room
draft): eight flat wall panels, faces centered on the four cardinals and
four diagonals, under the vault dome. The walls are **content surfaces,
not architecture for its own sake** — each panel is taken over by what it
carries (a bearing's ambient at the cardinals, information radiators on
the diagonals), with a thin neon edge-trim in the face's identity hue and
dark glass beneath — the room leans **cyberpunk**: emissive-on-dark,
restrained LDR at rest. Refined 2026-07-12 (Amy): **the overall vibe goal
is more synthwave than anything** — the reference feel is the time well's
HDR-forward interior (bloom-bright neon that *earns* its dark ground), not
the current octagon's murk; the room should read saturated-neon-confident,
not muted. The color-management pass (issues.md) is the vehicle: today
brightness/emissive gains are scattered per-shader while `palette.rs` only
governs hues + the glow-discipline caps, and "muted" is the accidental sum
of those local decisions rather than a chosen exposure. The camera orbits *outside* the shell and sees in
through a **cutaway**: every wall element is a single-sided inward-facing
mesh with back-face culling, so the near walls simply vanish from any
outside angle (dollhouse render, no shaders). Anything added to the shell
must keep that rule — a solid cuboid or `cull_mode: None` in the wall
breaks the cutaway.

**The panels are 16:9 screens** (Amy, 2026-07-10 evening — "the walls are
16:9 screens, and diving IS fullscreening a panel"): the octagon apothem is
tuned (`palette::WALL_APOTHEM`, 1200) so a panel's own width against the
fixed wall height reads as a 994:560 ≈ 16:9 frame. This reframes what
"diving" means for a **bounded** station — one whose whole instrument
already stands in the room as furniture (the patch-bay wheel, Tardis
reading #3): Enter/Down no longer cuts to a dedicated screen, it eases the
camera to a pose that fills the vertical frustum with exactly that
station's panel, edge to edge (`room::fullscreen_pose`) — the room's own
camera, not a scene cut, so the cutaway and the chamber never leave the
picture. **N stays a dive-THROUGH door**, not a panel to fill the frame
with: an unbounded world (the fsn landscape) is too big to stand as
furniture, so Tardis reading #3's *other* half — the archway you step
through — is still the future shape for it.

Center: the well, on its table (the table edge is already spoken for — the
Stage-2 rank rail, `docs/timewell.md`). Around the walls, stations at
**hand-assigned, stable compass bearings** — few enough to hand-place,
permanent enough to learn (the track-rays lesson: a bearing you learn is
an address). A bearing whose station stands *in the room* as furniture
carries **no marker pylon and no nameplate** — the instrument is the
station (Amy: boring labels, plates recede; the wheel proved it first):

| Bearing | Occupant | Shell rendering (as built / planned) |
|---|---|---|
| center | Time well (station #1) | itself — the console, on its table |
| W | Patch bay | **the wheel itself, mounted ON the W panel** (built 2026-07-10, wall-mount retune same day): the live circle at 0.42 scale, re-oriented face-out into the room, its chords the W ambient; floor traces terminate at the wall base under it; no pylon, no plate. **Fullscreens** on Enter/Down (the fullscreen-panel pivot, same evening) instead of cutting to a dedicated screen |
| E | Track transport | the vertical beat highway (mockup 33) on/before the E panel; notes fall toward the strike bar only while a track plays |
| N | VFS / rc library | an **archway** opening onto the fsn landscape (mockup 43), dimmed to horizon glow — returns when the fsn scene exists |
| S | *(reserved)* | future: MCP broker switchboard / LLM engine room |
| diagonals | Information radiators | **the four diagonal wall panels themselves** (built 2026-07-10): violet thread-columns on the glass — the free-floating slabs retired into the walls |
| overhead | Drift / mailboxes | not a bearing — **the air**. PAUSED (Amy, 2026-07-10): decide what information rides it before building; a kernel-activity-responsive point cloud is the live candidate vs the aurora arcs below |

The conversation view (station #0) has **no bearing**: it is reached by
diving *through a context card* (the Join transition, mockup 34 of the
time-well set) — conversations live inside contexts, and the geometry says
so.

## The floor is the wiring (Amy, 2026-07-07)

The floor's inscribed geometry is **functional, not decorative**:
station-to-station flows render as glowing circuit-board traces inlaid in
the black floor — a rainbow board, one hue family per fabric (crimson
MIDI, cyan PCM, more hues as new flow types earn them) — every trace
routed *around* the central well, never under it: the patch bay's
open-center rule at room scale. The two flow layers never mix (the
time-well's oldest visual-grammar rule): **the air carries drift**
(context-to-context blocks, sparse aurora arcs), **the floor carries
plumbing** (station-to-station system flows — audio routing first,
broker/tool traffic later if it earns a hue). At rest a trace is an LDR
engraving; it lights HDR and runs only while its flow is live.

**Faint-glow amendment (Amy, 2026-07-10):** decoration may carry a *faint,
slowly moving* glow — a traveling crest or slow breath whose peak just
crosses the bloom threshold (capped at `palette::GLOW_CREST`) while the
time-average stays comfortably LDR. The concepts' boards shimmer; a
dead-static etching read as unfinished, not calm. Strong *sustained* HDR
remains reserved for live activity — the tiering rule still holds, the
floor just breathes under it.

## Ambient telemetry rules

The shell's entire job at rest is glanceable truth, under the charter's
budget discipline:

- Every bearing renders its station's **activity as light**, never its
  detail: bright/HDR = live action now (the well's tiering rule), LDR = calm
  structure. The patch-bay wall flickers per event; the highway's notes fall
  only while a track plays; the archway's horizon brightens with VFS churn;
  drift arcs cross the air only when blocks actually move.
- All ambience rides the **existing kernel-wide event stream** the well
  already ingests (`view/time_well/live.rs`) — the shell adds renderers,
  not wire.
- **The walls between bearings are information radiators** (Amy,
  2026-07-07): tall slim panels of dark glass carrying live telemetry —
  sparklines, scrolling model output, status readouts — all in a single
  **violet** hue family reserved for radiators (crimson = MIDI, cyan =
  PCM, gold = the well, violet = information). A radiator that is
  actively drawing shows it the HDR way: the newest data point drags a
  bright bloom trail; idle radiators dim to calm LDR etchings. (The
  canonical render exaggerates the trail into a comet leaving the glass —
  in-scene the trail stays on the panel; the bloom is the tell, not a
  projectile.) Content
  rides the same kernel-wide event stream as everything else — candidates:
  per-track beat/token sparklines, the selected context's tail, a model's
  output streaming as it generates. Since the octagon rebuild the radiators
  ARE the four diagonal wall panels ("the surface gets taken over by its
  content").
- **First real radiator content candidate — a message wall** (Amy,
  2026-07-10): one diagonal panel renders **MSDF text, messages flowing
  through** — recent block/drift traffic as a scrolling violet ticker, the
  newest line carrying the HDR bloom trail. Everything it needs exists:
  the MSDF panel pipeline (`create_msdf_panel`/`MsdfBlockGlyphs` — plates,
  cards, and the editor already render text-to-texture on 3D quads; a wall
  panel is just a bigger quad), the kernel-wide event stream (already
  ingested by `ingest_room_activity`), and cheap motion (commit glyphs
  event-driven, scroll UVs in the shader — no per-frame relayout). A
  natural next wave after the trace-glow layer proves the moving-shader
  idiom on the walls.
- The shell never loads station detail. Dives cut to dedicated scenes
  (charter dive-through rule); Esc returns to the room, camera continuity
  both ways.

## Navigation

- **Levels — the arrows continue** (Amy, 2026-07-08): the room extends the
  well's existing arrow grammar one level upward. **Up/Down move between
  detail levels; Left/Right move within a level.** From the well's top
  (mouth) ring, Up exits to the room — through a **speedbump**: the first
  Up at the edge arms the exit, a second Up within the double-tap window
  fires it (the existing 500ms InterruptState pattern, pointed upward), so
  a habitual Up during ring nav never ejects you. In the room, Left/Right
  cycle the stations — well, patch bay, tracks, radiators, … (unbuilt
  stations ride the carousel as dimmed nameplates) — and Down/Enter dives
  into the focused one. Esc always walks up one level, everywhere. The
  grammar is unchanged by the fullscreen-panel pivot below — only what a
  *bounded* station's own dive/surface pair DOES changed, not the arrows
  that reach it.
- **Enter fullscreens, Esc pulls back** (2026-07-10 evening, superseding the
  earlier `Screen::PatchBay` state): for a bounded station
  (`station_is_zoomable`), Enter/Down eases the camera to fill the frame
  with that station's own panel — a camera pose plus a
  `RoomState::zoomed` write, not a screen transition — and Esc/Up eases it
  back out to the room-scale approach pose. The zoomed station's own keys
  (the wheel's Left/Right wire-cycling, `r` rescan) own the keyboard while
  zoomed; Left/Right at room scale still steps the carousel underneath, but
  not while zoomed. The well still cuts to its own dedicated screen — only a
  bounded, furnished station fullscreens in place.
- **Entry**: Ctrl+W keeps meaning *the well* (muscle memory is sacred);
  the room sits one speedbumped Up above it.
- **Travel by intent** (charter): the camera dollies continuously between
  stations and levels so spatial relationships register. No free-flight,
  no walking.
- A future minimap/orientation surface renders the same geometry as an
  overhead compass-rose floor plan — station markers at their bearings,
  the well glowing at center, the player's position a dot. A rendering of
  the one model, never a second model.

## Learnings from the combined-composition wave (2026-07-07)

A four-model shootout rendered the full room — every bearing occupied, one
identical prompt — and settled several things:

- **The composition works.** Well-as-console at center, thread-wall W,
  highway E, archway N with the data-city horizon, aurora overhead: all
  four models produced a coherent, legible room from that spec. The
  bearings table above is buildable as written.
- **Drift is aurora, not web.** The air layer reads right as a few slow,
  high arcs; one model rendered it as a spidery mid-air web and the room
  immediately felt haunted rather than alive. Keep drift sparse, high,
  and slow.
- **Station signage, adopted as an idea**: one model spontaneously hung
  in-world nameplates over the bearings ("BEAT HIGHWAY"). Nameplates at
  each bearing — instrument-engraved, not floating UI — are the cheap
  learnability device for a first-time visitor, and they match the well's
  existing nameplate grammar. Fold into slice A.
- **The rank rail keeps being rediscovered.** Multiple models, unprompted,
  furnished the well table's edge with control panels — external
  confirmation that the table-edge control ring (mockup 27's insight, the
  Stage-2 rank rail in `docs/timewell.md`) is where hands expect controls
  to live.
- **The dial on the kept frame** (Amy): the Imagen render of the combined
  room was "otherwise perfect" except an overgrown modular-synth cable
  jungle dominating the west wall — the shell-bearing read of the patch
  bay should be *restrained* (a few elegant threads over jack fields),
  with the routing story moved down into the floor traces instead. That
  dial produced the floor-is-the-wiring direction above — and the kept
  render (mockup 06, after a radiator-wall dial) confirmed it composes:
  the patch panel's wall threads drop and *become* floor traces
  (wall-to-floor continuity worth building literally), and the model
  volunteered engraved station nameplates with names good enough to
  steal — "PATCH BAY WEST", "DATA HORIZON NORTH", "RHYTHM GATE EAST"
  (naming candidates, not decisions).

## Build path

The pragmatic claim: **the shell is a camera pull-back from the existing
well scene**, not a new world. Slices, each shippable alone:

- **Slice A — the room exists** (built, `view/room/`): the circular chamber —
  a dark floor disc with etched crimson/cyan trace channels bowing around the
  console, a subtle gradient vault dome, the gold console emblem at center,
  marker pylons + engraved nameplates at the cardinal bearings, and violet
  radiator panels between them. The camera dollies to face the focused bearing
  (the well's eased tween). Ambient glow rides the existing event stream — no
  new wire: the tracks (E) marker breathes with the well's beat phasors
  (`WellBeats::global_envelope`) and the console glows with context chatter
  (`room::activity::BearingActivity`), HDR only on live activity. Everything
  else is procedural LDR (built-in `StandardMaterial`, `unlit` with brightness
  in `base_color` so HDR blooms). The console is the slice-A stand-in for the
  live well (shared vs separate scene graph, open question 3, stays deferred to
  slice B). Acceptance met: a jam breathes the tracks bearing without visiting
  it.
- **Slice B — first real bearing** (built 2026-07-09): the patch-bay circle
  stands at W as **room furniture** (Tardis reading #3), and diving is a
  *continuous camera descent* onto it — no scene cut, one camera, one clear
  colour (open question 3, decided). The furniture rides ONE Amy-tunable
  placement transform (`STATION_W_PLACEMENT` in `view/patch_bay/mod.rs`:
  translation to W, uniform scale, yaw) so re-placing it — Amy's **pending
  alternative, the table as the room floor** — is a transform edit, not a
  rebuild. The dive dims the room chrome and shows the patch bay's own label/
  tick/card LOD; at room scale the bare chords over the socket rings are the W
  ambient. For now that live-chord glow **absorbs** the bearings-table's
  wall-of-threads read — the elegant thread-wall over jack fields is deferred
  and may return later as pure backdrop mood behind the table. This pairs the
  first station dive with the first bearing — the whole grammar proven end to
  end.
- **Furnishing + enclosure (built 2026-07-10)**: the concept-approach wave —
  the ~35-route deterministic circuit-board floor with inscribed gold ring
  (the floor-is-the-wiring made literal), the well table under the console
  rings, pylon plinths/caps, and then the **octagon shell** (geometry
  section above): eight content-surface wall panels with the camera
  cutaway, radiators retired into the diagonal panels, and (first cut) the
  patch wheel seated on a floor dais as the W station itself — sign and
  pylon gone. One style across the family: `view/palette.rs` holds the
  shared hues, and the **all-unlit discipline** went scene-family-wide (the
  patch bay's point light + lit metals deleted; a ~1%-albedo metallic
  surface swallows any lamp — the tuning-pass lesson).
- **Wall-mount retune (same day, 2026-07-10)**: the dais was a first cut —
  Amy's call, later that day, was to mount the wheel ON the W panel itself
  ("the surface gets taken over by its content"; studio patch bays are wall
  panels, not tables; concept 06 draws the station wall-mounted with
  threads dropping into the floor traces). The dais and its furniture
  builder are gone; `patch_bay::STATION_W_PLACEMENT` gained a pitch+yaw
  composition that re-orients the wheel face-out and seats it flush against
  the panel `spawn_walls` already builds. `view/palette.rs`'s station-W
  contract now holds `WALL_APOTHEM` (moved there — a cross-file datum) plus
  the mount height/proudness/scale; the room side needs no furniture for W
  at all any more.
- **The fullscreen-panel pivot (2026-07-10 evening)**: "the walls are 16:9
  screens, and diving IS fullscreening a panel." The octagon apothem grew
  800 → 1200 so a panel's own aspect reads 16:9 (Geometry section above);
  `Screen::PatchBay` — the second screen slice B introduced to hold the
  dive — is **dissolved entirely**: `view::room::RoomState` gained a
  `zoomed: Option<Station>` field, `room::room_keyboard` sets/clears it on
  Enter/Down and Esc/Up, and `room::fullscreen_pose` computes the camera
  pose that fills the frame with the zoomed station's panel, independent of
  the station's own local placement transform. This deletes the whole
  dive-exit special-casing slice B needed (the `OnExit(Screen::PatchBay)`
  branch reading the *target* state during `OnExit(Screen::Room)`, and its
  mirror in `exit_patch_bay`): `exit_room`'s teardown is now
  **unconditional** — there is only one screen left for this scene graph to
  occupy, so there is only one way out of it to get right. The patch bay's
  own LOD and keyboard gate on `RoomState::zoomed` instead of
  `in_state(Screen::PatchBay)`.
- **Slice C+ — bearings accrete**: highway ambient at E when tracks-station
  work starts; the N archway when the fsn scene exists. No fixed order.

## Open questions

1. Bearing assignments — **first concrete placement landed with slice A**
   (`room::bearing`): console = center, PatchBay = W, Tracks = E, VFS = N,
   reserved = S (a dim unlabeled marker), radiators on the four diagonals. The
   `Radiators` carousel entry faces the NE panel. Still provisional — judge and
   re-place now that the camera exists.
2. Entry key for the room (Esc-from-well is decided; a direct binding is
   not), and whether the app's `Screen` formalization (timewell.md appendix,
   "ViewSpec + the kj→app seam") should land with slice A.
3. **DECIDED (Amy, 2026-07-09): one shared scene graph.** Diving is continuous
   camera travel inside the persistent room, not a scene cut — the room and its
   station furniture never despawn/respawn on a dive. Camera continuity won over
   the budget argument because a bounded station (the patch-bay circle) is cheap
   to hold resident as furniture (Tardis reading #3), and the budget discipline
   is recovered instead through **LOD**: the dive's detail layer (labels, ticks,
   the inspection card) hides at room scale and the room chrome dims on the dive,
   so only one station's *detail* is ever drawn. Built in slice B (below).
   **Taken one step further (2026-07-10 evening):** the "one shared scene
   graph" decision made the second screen (`Screen::PatchBay`) it was built
   alongside redundant — diving was already continuous camera travel with
   nothing to cut to, so a `RoomState` field does the same job as the
   screen did, without a second exit path to keep in sync (the fullscreen-panel
   pivot, Build path above).
4. The vault itself: what does the dome show? (Starfield is the lazy
   default; a slowly rotating glyph firmament could carry kernel-wide state
   — deferred, taste call.)
5. Where the Tilt-Brush-style tool tray (mockup 04, hand-free) docks — per
   dive scene, or a shell-level fixture that travels with the camera?

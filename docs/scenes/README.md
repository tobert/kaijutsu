# Scenes — the Kernel Interior (charter)

2026-07-07. The founding document for kaijutsu's 3D scene family: the world
model, the principles every scene obeys, the station inventory, and the
concepting process. The conversation view and the time well came first and are
retroactively stations of this world (see "Relationship to the other docs").
This doc is for the builders (Opus/Sonnet sessions) who will take a concepted
station from image to Bevy scene — it holds the *decisions*; each station gets
its own design doc as it matures (`docs/scenes/<station>.md`).

## Why space at all — the No Man's Sky lesson

NMS survives an enormous data-driven feature set by using **space as an
information architecture**, and that is the part we take (the flight sim
stays on the shelf):

1. **Different data lives in different places**, so each interface only has
   to be a *bounded view*. Nobody builds the god-dashboard.
2. **Stations are interfaces docked in the world** — the world isn't the UI;
   the world *holds* the UIs and gives them addresses.
3. **Spatial memory is nearly free and very large.** You remember *where*
   the patch bay is long after you'd forget which menu tab it lives in.
   (Already proven in-repo: track rays at name-hashed bearings — "a bearing
   you learn.")
4. **Travel is a legible transition** — but we tune the cost curve instead of
   copying it. Kaijutsu travel is **by intent**: name a destination
   (keystroke, verb, click) and the camera moves *continuously* to it.
   Continuity buys spatial memory; intent keeps it fast.

## Founding principle: no scene-only truth

**Every scene is a rendering of kernel-owned symbolic state.** The scene's
manipulable state — a patch wire, a playhead, a promoted card — lives in the
kernel (thin-client rule, as everywhere). Consequences, in order of weight:

- **Appendage symmetry.** Amy's keyboard/mouse/gamepad input maps to kernel
  verbs; a model's forward pass calls the *same verbs* over kj/MCP. Human and
  model are peers on one instrument by construction, and a third player's app
  is just another renderer.
- **Shared symbolic score, per-player rendering** (the chameleon stance,
  generalized). The human sees the scene rasterized with bloom; a model reads
  the same state as structured data and takes a BRP screenshot when it wants
  the human's view. If a scene knows something the kernel doesn't, that's a
  bug.
- **Per-client state stays local** — camera pose, focus, quality tier — the
  same split `/etc/client/` already made.
- Multiplayer *presence* (rendering whose hand moved a wire) is deferred, but
  this principle makes it cheap later: "player X is touching Y" is a row, not
  a rendering trick.

## Topology: shell + dives (decided 2026-07-07)

A **hybrid**: one continuous shell room for orientation and ambient
telemetry; **diving into a station cuts to a dedicated scene** tuned to that
station's data shape. The shell's own design doc: `docs/scenes/shell.md`
(the Tardis room). This is the time well's two-level rule ("overview never
overloads; detail is opt-in") promoted to the whole world, and it is NMS's
actual structure — open space plus docked interfaces.

- The shell shows every station at its bearing as **ambient telemetry** —
  glow tracks activity (the patch bay wall flickers when MIDI flows, even
  when you're not there). No station renders its detail into the shell.
- Dives are **dive-through transitions** (the Join-transition grammar from
  the time well, mockup 34): you pass *through* something — a card, a wire,
  a doorway of light — so the cut still builds a mental map. Esc backs out
  one level, always.
- **Budget discipline is a design input, not a polish pass.** Target
  hardware includes battery-powered laptops now and tablets someday. The
  shell stays calm and cheap (low overdraw, instanced ambience); per-scene
  complexity is capped by construction because only one station's detail
  exists at a time. Spend the GPU where the player is looking.

## Presence: camera-only (decided 2026-07-07; hands rejected after wave 3)

**Camera-as-presence** — no body, no walking, and after seeing the wave-3
ghost-hand variants: **no hands**. Manipulations animate the *thing itself*
(the wire plugs itself in, the card blooms), never a disembodied limb.

- **Tilt-Brush-style tool palette** stays open as a *hand-free* floating
  tray: a diegetic tool tray of implements floating low in the player's
  view — each tool seated in its own inscribed socket — never a
  screen-space HUD.
- **VR is shelved for now** (wave 3 verdict). Durable findings kept from the
  wave: the edge-HUD grammar translates cleanly to curved world-space glass,
  and at VR scale the grand atrium naturally shrinks to a Tardis
  console-room — same stations as walls of one intimate chamber. Desktop
  keeps the **grand Tardis atrium** (mockup 01 direction, confirmed).

Full multiplayer presence (seeing *other* players' actions attributed) is
explicitly deferred — kept cheap by the no-scene-only-truth principle, not
designed yet; when it arrives it follows the same rule: attributed motion of
the things themselves, not avatar limbs.

## Aesthetic: Arcane Techmage (decided 2026-07-07)

Cyberpunk is just real now, so we do **cyberpunk arcane**: the kernel
interior as a techmage's studio. Guidance for concepting and building:

- **Megumin's vibe, not the occult's inventory** — crimson/ember explosion-
  spell energy, chuuni flair, HDR bloom (the built well's grammar) — but go
  easy on pentagrams. Prefer concentric rings, radial ticks, inscribed
  geometry, glyph script.
- Hardware where hands touch: brushed black metal, brass, jacks, levers —
  instrument hardware inside arcane framing.
- **Style frames the interface, then gets out of the way.** The real magic
  is usable interfaces: at every conflict between lore and legibility,
  legibility wins. Bright = live action (the well's HDR tiering rule);
  decoration stays LDR and calm.

## Station inventory

Each station earns its place by having a genuinely different **data shape**
(the test: if two stations want the same interface, they're one station).
Existing views slot in as stations #0 and #1.

| # | Station | Data shape | Status |
|---|---------|-----------|--------|
| 0 | Conversation | Append-only block sequence | Shipped (2D view; joined to the well by the dive-through) |
| 1 | Time well | Context forest by recency/placement | Shipped; `docs/timewell.md` |
| 2 | Patch bay | Relation: endpoint edges, declared intent vs live reality | **Designed** — `docs/scenes/patchbay.md`; viz-first (see below); circle-matrix direction (Amy, waves 1–3) |
| 3 | Drift / mailboxes | Flows in flight between contexts; queues pooling | Concepting |
| 4 | Track transport / score | Cyclic time: playhead, tempo, attached musicians | Concepting — **vertical beat highway** (Guitar Hero / DDR lineage), not a score view; a score view comes later as a 2D special view, not core (Amy, wave 1). Wire slice shipped (`TrackInfo`) |
| 5 | VFS / rc library | Tree with ownership zones; scripts as objects | **Designed (early)** — `docs/scenes/vfs.md`: fsn / Jurassic Park lineage with LOD-on-approach as a primitive (Amy, waves 1–2); interaction UX still to concept |
| — | MCP broker, LLM routing, … | Registry + in-flight invocations; providers/spend | Uninventoried; add when a station is designed |

### Patch bay: viz-first, backend later (Amy, 2026-07-07)

The patch bay **backend does not exist yet** and patching stays **CLI-only
for a good long time**; if the backend gets complex, that's the signal to
lean into PipeWire control rather than overbuild our own. The *viz* comes
first anyway — it will look cool and guide the journey:

- **Slice 0 renders observed reality only**: the live ALSA seq / PipeWire
  graph (readable today — pawlsa reads the PipeWire side; `aconnect -l` the
  ALSA side), no declared-intent layer, nothing writable from the scene.
- The declared-vs-observed duality is still the design target: ghost wire =
  declared but down, lit wire = live, a neighbor's manual patch rendered
  *warm* — additive-by-default, never an alarm (crosstalk is a feature).
- Design homes for the backend when it matures:
  `docs/config-crdt-ownership.md` (per-client namespace), `docs/mounts.md`
  (the wire-ownership note), `docs/midi.md` (distribute intent, not pulses).

## Concept findings (running)

Lessons the mockup waves surfaced; each moves into its station's design doc
when that doc exists.

- **The center stays open** (wave 2, from a mockup getting it wrong): in the
  patch bay circle, chords must bow *around* the center, never through it —
  routing through the middle creates a false hub and untraceable edges. Same
  rule as the well's open mouth. Chord-bundling by fabric is the scaling
  move when edge count grows.
- **Edge-HUD grammar is a keeper** (wave 2, mockup 17): thin diegetic
  ribbons/panels at the screen edges — breadcrumb top, smoked-glass detail
  right, legend bottom — carried the dive-scene UX; the rest of that mockup
  didn't. Inherits the time well's edge-HUD precedent.
- **fsn wants LOD as a design primitive, not an optimization** (wave 2,
  mockup 44): distant platforms render as cheap glowing blocks, detail
  upgrades as the viewer approaches. This is also the budget-discipline rule
  made concrete for the one station whose population is unbounded.
- **VR wave verdict** (wave 3): declined for now — the hands metaphor in
  particular (see Presence). What survived the wave, now specified in
  prose: the patch bay's dive-scene UI grammar (open-center table,
  breadcrumb ribbon, detail panel, inspection card at the wire — see
  `patchbay.md`, minus the hand); LOD-as-interaction (a touched platform
  blooming to full detail while the rest stays cheap — see `vfs.md`); and
  the console-room scale finding, kept for a future VR return.

## Concepting process

Same process that produced the time well (`docs/time-well-concepts.md`):
generate mockup waves with gpal (`generate_image`; Imagen Ultra + Nano
Banana Pro), browse, argue, keep the winners. Concepts land in
`docs/scenes/concepts/`, numbered by decade so waves stay browsable:

- `0x` shell room · `1x` patch bay · `2x` drift/mailboxes · `3x` tracks
  dais · `4x` VFS library · `5x+` new stations as inventoried
- **The repo keeps one canonical image per decided station** (the
  time-well precedent — it kept only mockup 27). Everything a culled
  mockup taught must be carried by the design docs' *prose* before it
  goes: the docs are what guide the builder models. Culled mockups move
  to Amy's story archive (`~/kj-junk`), not the trash; keeper prompts are
  recorded in the station's design doc for regeneration.

## Relationship to the other docs

- `docs/timewell.md` — station #1's forward plan; its two-level navigation
  rule and Join transition are this charter's precedents. Nothing here
  reorders that plan.
- `docs/time-well-concepts.md` — the process template and the visual-grammar
  record this world inherits (two flow systems never mixed; hub-and-spoke;
  bright = live).
- `docs/chameleon.md` — the shared-symbolic-score stance that "no scene-only
  truth" generalizes.
- `docs/instrument-design.md` — shared trust, crosstalk-as-feature; why a
  neighbor's manual patch renders warm.

# Patch Bay — Station Design

2026-07-07. First station doc under the scenes charter
(`docs/scenes/README.md`, station #2). The patch bay is kaijutsu's routing
surface: the local MIDI/audio fabric made visible and, eventually, patchable.

**Viz-first** (Amy, 2026-07-07): the patching backend does not exist yet and
patching stays **CLI-only for a good long time** — the scene's first job is
to render observed reality, look cool, and guide the backend's design by
being looked at. If our backend ever grows complex, that is the signal to
lean into PipeWire control rather than overbuild our own. Backend design
homes when it matures: `docs/config-crdt-ownership.md` (per-client
namespace), `docs/mounts.md` (the wire-ownership note), `docs/midi.md`
(distribute intent, not pulses).

## What the station shows

- **Two fabrics, one instrument.** MIDI rides ALSA seq (`aconnect`); PCM
  rides PipeWire. **ALSA-MIDI first** — it's the live pain (the
  re-run-`aconnect`-after-every-restart papercut); PipeWire is the superset
  later.
- **Per-client, by nature.** The audio graph is machine-local; each client's
  patch bay shows *its own* fabric. Anything durable is **ClientId-keyed**
  (decided during the metronome/per-client-config work — don't re-litigate;
  precedent: `client_views` in `kernel_db.rs`, namespace: `/etc/client/`).
- **Endpoints are symbolic names, resolved live.** The app's own source
  endpoint is the ALSA seq port opened by `MidiOut::open()` in
  `crates/kaijutsu-app/src/midi.rs` (client `"kaijutsu-app"`, port
  `"render"` — carries both music cues and the metronome click today). Synth
  client numbers are **dynamic** (TiMidity is 128/129/whatever per boot) —
  always resolve by name, never store a number.

## The ground-truth split (the charter's one honest nuance)

The charter says *no scene-only truth: every scene renders kernel-owned
state*. The patch bay is the station where that meets its nuance: the
observed audio graph is **edge-local reality the kernel does not hold**. The
split follows existing doctrine — **the kernel holds intent, the edge holds
reality** ("distribute intent, not pulses" applied to routing):

- *Observed* wires (slice 0) are read by the app from its own local
  ALSA/PipeWire graph. No kernel state involved.
- *Declared* wires (slice 2) are kernel-owned, ClientId-keyed; an app-side
  reconciler realizes them against the local graph, **additively** — create
  missing declared wires, never tear down undeclared ones. A neighbor's
  manual `aconnect` patch is a feature, not drift to correct.
- Appendage symmetry is completed later (slice 4) by shipping observed-graph
  snapshots kernel-ward, so models and remote peers can see the same fabric
  a local player sees. (Today a model only sees the graph if it runs on the
  same box — e.g. pawlsa.)

## The scene (decided direction, waves 1–3)

Canonical reference: `docs/scenes/concepts/14-patchbay-circle.png` (the
form — Amy's pick). Two culled mockups live on in prose: a VR frame
contributed the dive scene's UI grammar (specified below — everything
carried over except the hand), and a wall-of-jacks mockup set the
*shell-bearing* read: brass jack fields under sigil labels, strung with a
living thread-web — mood at distance, never the dive interface
(`docs/scenes/shell.md`, bearings table).

- **A circle matrix on a round table.** Brass endpoint sockets around the
  rim, short ALL-CAPS holographic labels (RENDER, METRONOME, GM SYNTH,
  SPEAKERS…), etched gold ring-and-tick geometry, deep indigo dark.
- **The center stays open** (charter finding, wave 2): the table has a hole
  in the middle; chords **bow around** the center, never through it — routing
  through the middle creates a false hub and untraceable edges. When edge
  count grows, bundle chords by fabric.
- **Wire grammar** — hue is fabric, state is solidity:
  - hue: **crimson = MIDI**, **cyan = PCM/audio** (matches the shell's
    palette rules: bright = live).
  - solid lit = observed live; **ghost-translucent = declared but down**
    (slice 2+); **warm amber = observed but undeclared** — a neighbor's
    hand patch, rendered *warm*, never as an alarm.
  - Slice 0 has no declared layer, so every wire renders observed-solid.
- **Selection**: a chord highlights and a small inspection card blooms at
  the chord itself — source → dest plus live facts, e.g.
  `SYNTH → RENDER · clip SYNTH_01 · 4s · −3 dB`. Edge HUD in the well's
  grammar: breadcrumb ribbon top (`HOME > STUDIO > PATCH BAY > SYNTH`),
  smoked-glass detail panel right, legend strip bottom — all curved
  world-space glass at comfortable angles, never flat screen-space
  overlays.
- **No hands** (charter, post-wave-3): interaction animates the wire itself —
  a wire being connected draws itself socket-to-socket; disconnection
  dissolves it.
- **In the shell**: the patch bay's bearing shows the wall-of-threads as
  ambient telemetry — threads flicker when MIDI flows, even unvisited.
  Diving cuts to the circle scene (charter dive-through rule). The routing
  state also reaches the shell **floor**: live wires render as
  circuit-board traces inlaid in the room floor, bowing around the central
  well — this station's open-center rule at room scale (`shell.md`, "The
  floor is the wiring").
- **Live activity**: traffic pulses travel the chord when events flow
  (same event-driven material-lane trick as the well's chatter/beat lanes —
  no texture rebuilds).

## Slices

Each shippable alone, in order; 1 is independent and can land any time.

### Slice 0 — observed ALSA graph, read-only (the viz)

- App-side ALSA seq **graph reader** beside the existing `MidiOut`: clients,
  ports, subscriptions — polled (the well's poll/layout-tick pattern);
  seq announce events can tighten latency later.
- The circle scene renders observed wires only; selection + inspection card
  + edge HUD; no write path of any kind.
- **Entry** (updated 2026-07-08): slice 0 ships *with* the nav skeleton —
  a blockout room level (station carousel: nameplates at bearings, no
  full shell art yet). Up-Up from the well's top ring (speedbumped)
  reaches the room; Left/Right focuses PATCH BAY; Down/Enter dives to the
  circle scene; Up/Esc returns to the room. In-scene: Left/Right cycle
  wire selection. The full shell room (slice A of `shell.md`) replaces
  the blockout later without changing the keys.
- Acceptance: with TiMidity up and the render port wired, the scene shows
  `RENDER → <synth>` and `<synth> ↔ system` wires; hand-run `aconnect`/
  `aconnect -d` changes appear on the next poll; the metronome click makes
  the RENDER chord pulse.

### Slice 1 — auto-connect on startup ✓ shipped (independent cheap win)

On startup the app auto-connects its render port to a detected GM synth,
killing the re-`aconnect`-after-restart papercut (`docs/mounts.md` note).
Lives in `crates/kaijutsu-app/src/midi.rs` beside `MidiOut`; no scene
dependency either way. Semantics:

- **Detect by name, never by number** — case-insensitive substring match on
  the ALSA client name against a hardcoded `SYNTH_PATTERNS`
  (`["timidity", "fluidsynth"]`), the one obvious config seam (open question
  #2 stays open — no config surface yet). Never matches our own clients
  (`kaijutsu-app`/`-ear`/`-patchview`).
- **Additive and deferential** — only wires when the render port has *zero*
  outbound subscriptions; if anything already feeds from render (a human
  hand-patch), it does nothing. Never disconnects.
- **One-shot with patient retry** — retries on the 2 s poll cadence until the
  first connect (or an already-wired / ALSA-less stand-down), then stops for
  the life of the process. Startup-once on purpose: the metronome click rides
  the render port with no off-switch yet, so Amy sometimes cuts the wire with
  `aconnect -d`; a continuously-reconciling ensure would make it uncuttable.
  Continuous declared-wire reconciliation is slice 2's job (kernel-owned).
- Pure decision core (`decide_autoconnect`) is unit-tested; the ALSA write is
  a single `Seq::subscribe_port` on the render handle.

### Slice 2 — declared intent (the backend, when it's time)

- Kernel-side declared-wire store, ClientId-keyed. Storage-shape fork
  (recorded, not decided): **typed KernelDb table** —
  `patchbay_wires(client_id, source, dest, enabled, …)`, the `client_views`
  precedent, the current lean — vs a **CRDT surface** you rewire from any
  peer (most kaijutsu-flavored; heavier, and routing is per-client so
  multi-writer buys less). Decide at build time against real use.
- `kj patchbay connect|disconnect|list` — the CLI *is* the patching surface
  for a long time; the scene stays a viewer until well after this lands.
- App-side **reconciler** (the audio edge): resolves symbolic endpoints by
  name against the live graph, creates missing declared wires, additive by
  default. Ghost and amber wire states activate in the scene.

### Slice 3 — PipeWire fabric

The PCM/audio graph joins, as the cyan ring — the layout candidate is a
two-ring matrix: inner MIDI ring, outer audio ring, ember bridge-chords
crossing between them where a synth turns MIDI into sound.
pawlsa overlap noted: it's an existing *imperative* PipeWire surface with no
owner/reconciler; slice 3 is where ownership questions get real.

### Slice 4 — observed graph kernel-ward

Ship edge-observed topology snapshots to the kernel (RPC or telemetry —
open) so remote peers and models see the local fabric. Completes appendage
symmetry for this station.

## Open questions

Carried from the metronome-session handoff notes plus new ones:

1. **Storage shape** (slice 2): typed KernelDb store vs CRDT surface.
2. **Symbolic endpoint naming**: ALSA client-name substring match vs a small
   named-endpoint registry in config ("gm-synth" → pattern).
3. **Headless edge reconciler**: the app is the obvious audio edge; where
   does a synth-only box's reconciler live?
4. **Click endpoint**: does the metronome click deserve its own routable seq
   port, so it can go somewhere other than the music? (The concrete "what
   the patch bay unlocks" example — mockup 15's topology assumed yes.)
5. **ClientId for MCP/headless clients**: durable client identity exists
   only for the Bevy app today — shared prerequisite with per-client config
   ergonomics.
6. **Slice-4 transport**: RPC vs telemetry for observed-graph reporting.

## Execution notes

- Reuse the well's substrate patterns: `Join` keyed on stable ids (wires key
  on `(source, dest)` endpoint-name pairs, not client numbers), poll =
  layout tick / events = data tick, `bevy_math::curve` tweens, instanced
  chords with `MeshTag` if count ever demands it. HDR bloom on live wires
  only; decoration stays LDR (charter budget discipline).
- Bevy 0.18 renames: trust the CLAUDE.md table.
- Chord geometry: bowed arcs around the open center — great-circle-style
  curves at staggered heights avoid crossings reading as junctions.
- The gotcha that started it all: **never persist an ALSA client number.**

## Appendix — keeper prompts (for future iterations)

Mockup 14 (form): *"routing-as-inscription: a large tilted round table …
endpoint nodes — small brass sockets each marked by a floating holographic
glyph … chords of light connect node to node: blazing crimson and cyan
chords for live routes, faint ghost chords for declared-but-down routes, one
warm amber chord for a hand-made patch. Concentric guide rings and radial
ticks etched in faint gold … no pentagrams … legible like a well-designed
instrument."* (Imagen Ultra, 16:9.)

Mockup 18 (UI grammar; generated as VR, adopt minus the hand): *"circular
patching table at waist height … etched gold concentric rings with an open
uncluttered center, brass endpoint sockets around the rim with short glowing
capital-letter labels (RENDER, SYNTH, CLICK, SPEAKERS), chords of cyan and
crimson light bowing AROUND the open center … a small inspection card
blooming beside [the selected chord] … curved smoked-glass panels: a thin
breadcrumb ribbon above, a narrow detail panel to the right — world-space
edge-HUD grammar, no flat overlays."* (Nano Banana Pro, 16:9.)

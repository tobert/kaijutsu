# The Shell — Tardis Room (design)

2026-07-07. The shell is the charter's connective tissue made concrete: the
one continuous room that holds the stations (`docs/scenes/README.md`,
"Topology: shell + dives"). It is **not a station** — it has no data shape of
its own; its job is orientation, ambient awareness, and travel. Canonical
concept: `docs/scenes/concepts/01-shell-atrium.png` (Amy: "the atrium
*rules*"), with `02-shell-overhead.png` as the map reading and
`04-shell-palette.png` holding the hand-free tool-tray framing.

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

A circular vaulted room. Center: the well, on its table (the table edge is
already spoken for — the Stage-2 rank rail, `docs/timewell.md`). Around the
walls, stations at **hand-assigned, stable compass bearings** — few enough
to hand-place, permanent enough to learn (the track-rays lesson: a bearing
you learn is an address). Provisional assignment, to be settled when the
shell is built:

| Bearing | Occupant | Shell rendering (ambient) |
|---|---|---|
| center | Time well (station #1) | itself — the console |
| W | Patch bay | the wall-of-threads (mockup 11): thread-web flickers as MIDI/PCM flows |
| E | Track transport | the vertical beat highway (mockup 33) seen side-on; notes fall while tracks play |
| N | VFS / rc library | an **archway** opening onto the fsn landscape (mockup 43), dimmed to horizon glow |
| S | *(reserved)* | future: MCP broker switchboard / LLM engine room |
| overhead | Drift / mailboxes | not a bearing — **the air**: particle arcs over the well rim (mockup 22) |

The conversation view (station #0) has **no bearing**: it is reached by
diving *through a context card* (the Join transition, mockup 34 of the
time-well set) — conversations live inside contexts, and the geometry says
so.

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
- The shell never loads station detail. Dives cut to dedicated scenes
  (charter dive-through rule); Esc returns to the room, camera continuity
  both ways.

## Navigation

- **Levels**: the shell extends the well's progression one level *upward* —
  shell (room overview) → well levels (rings → detail → conversation) or
  → a station dive. Esc always walks up one level; from the well's overview,
  Esc reaches the room.
- **Entry**: Ctrl+W keeps meaning *the well* (muscle memory is sacred); the
  shell is one Esc above it, plus (open) a direct key of its own.
- **Travel by intent** (charter): arrows/tab cycle bearings, Enter dives;
  the camera dollies continuously so spatial relationships register. No
  free-flight, no walking.
- The overhead map (mockup 02) is the future minimap/orientation reading of
  the same geometry — a rendering, not a second model.

## Build path

The pragmatic claim: **the shell is a camera pull-back from the existing
well scene**, not a new world. Slices, each shippable alone:

- **Slice A — the room exists**: pull-back level above the well; the vault,
  the floor geometry, placeholder bearing markers with real ambient glow
  (driven from the event stream by producing-context type/track — derivable
  today). Esc/Enter level plumbing. Acceptance: enter the room, watch a
  jam make the tracks bearing breathe without visiting it.
- **Slice B — first real bearing**: the patch-bay thread-wall ambient at W,
  diving to the patch-bay slice-0 scene (`docs/scenes/patchbay.md`). This
  pairs the first station dive with the first bearing — the whole grammar
  proven end to end.
- **Slice C+ — bearings accrete**: highway ambient at E when tracks-station
  work starts; the N archway when the fsn scene exists. No fixed order.

## Open questions

1. Bearing assignments — settle when slice A's camera exists to judge them.
2. Entry key for the room (Esc-from-well is decided; a direct binding is
   not), and whether the app's `Screen` formalization (timewell.md appendix,
   "ViewSpec + the kj→app seam") should land with slice A.
3. One scene graph or separate Bevy worlds for shell vs dives? (Budget
   argues separate — only one station's detail exists at a time; camera
   continuity argues shared. Decide at slice B.)
4. The vault itself: what does the dome show? (Starfield is the lazy
   default; a slowly rotating glyph firmament could carry kernel-wide state
   — deferred, taste call.)
5. Where the Tilt-Brush-style tool tray (mockup 04, hand-free) docks — per
   dive scene, or a shell-level fixture that travels with the camera?

# The heroic view

The heroic view is a full-window 3D scene in the app that shows every
performer's anticipation and commitment on the shared pulse. It is the client
that `docs/issues.md`, "Anticipation and commitment — iteration order", step 4
asks for: "no client shows work yet."

This document is our local understanding of a visual brief Fable wrote after a
three.js prototype. Fable worked from deliberately vague descriptions of
kaijutsu. Where the brief and the code disagree, this document follows the
code, and says so. "Heroic" is a working name.

What is settled and what is not:

- **Settled:** the composition, the color meanings, the motion rules, and how
  the scene draws time.
- **Loose:** the gameplay — scoring, credit, levels. The kernel has none of it
  yet; see "Gameplay".
- **Tunable by eye:** every number. Numbers live in the `[heroic]` section of
  `theme.toml` and apply while the view is open. When a number and the look
  disagree, the look wins. Render it and look at it at every step; this scene
  cannot be judged from code.

When a choice comes up that this document does not cover, ask in order:

1. Can a player still see how much slack each performer has?
2. Does it move on the music's clock?
3. Does each color mean only what that color means?

## The picture

A night road made of time runs from the viewer's feet to a striped sun on the
horizon.

Lanes of dark glass lie side by side, ruled with thin bright rails. Each lane
is one track. Every note wears the color of the character that played it. Notes are small glowing
discs that slide toward the viewer at the speed of the music. Near the bottom
of the frame a white line crosses the road. That line is now. A note that
reaches it flashes its landing pad and is gone.

Above each lane, somewhere up the road, a white crystal turns inside a wire
cage in its performer's color. Each context attached to the track has its own
crystal, working at the place in the future it is writing. No two contexts
have the same crystal. Between it and the viewer the lane holds solid notes.
Under it lies a lit slab one phrase long, where wireframe notes turn solid as
work becomes ready. Beyond it the lane is empty.

The floor between the white line and the last ready note glows faintly in the
lane color. Its length is the performer's slack. When the slack falls below
the performer's own lead, the floor turns red and pulses.

Walls of fire stand along both road edges, one flame per beat, rainbow in hue
and white at the root. Outside the fire a magenta grid runs flat to wireframe
hills, which are the sound of the last few bars rolling away. The sky is deep
violet falling to hot pink. Wire gliders cross the sun slowly. A ring stands
across the road at the end of each round, and the viewer passes through it.

Select a performer and the other lanes dim. A performer panel of dark glass
opens on the far side of the road, tied to the crystal by a thin line.

## Distance is time

Every position along the road is a moment. The strike line is now; farther up
the road is later. Three rules follow:

- **Slack is a length.** How far ahead a performer has prepared is a length of
  floor, readable without a number.
- **Anything scheduled is an object on the road.** A note, an instruction, a
  phrase boundary, a round boundary: each sits at its time and travels toward
  now.
- **Things without a time live off the road.** Identity, totals and history go
  in the performer panel, the sky, or the flat overlay.

**Road distance is wall time, not beats.** Each track keeps its own period
(`TrackInfo.periodUs`; `docs/time.md`, "Tracks and Score Contexts"), so one
beat is not one length everywhere. One second is one length everywhere, and
each lane draws its own beat and phrase lines from its track's period. When
every track shares a tempo, this looks the same as a beat-ruled road.

### What a note shows

A note shows its state without a legend: solid is ready, wireframe is
provisional. The state comes from one `WorkStatus` record
(`crates/kaijutsu-hyoushigi/src/work.rs`) and the track's playhead. The
engine's `CellState` is private to the timeline, so the view decides from what
the record carries. Rules are checked in order; the first that matches wins:

| # | Record | Before its start | At its start |
|---|---|---|---|
| 1 | No record for this phrase | Nothing. The lane is empty. | — |
| 2 | `Cancelled` or `Superseded` | Fades out in place | — |
| 3 | `Fallback`, reason `Requested` | Solid disc with a hollow core: a repeat the player asked for | Pad flashes in the performer's color. Not trouble. |
| 4 | `Fallback`, any other reason (`DeadlineMissed`, `ResolveFailed`, `InvalidBasis`) | Red ring in the lock zone | Pad flashes red. A filled disc if fallback content played (`content` is set); a hollow ring if nothing played (`skip`, or `last-good` with nothing to repeat). |
| 5 | `Committed` | Solid, slightly brighter, inside the lock line | Played: the pad flashes white and a ring expands and fades |
| 6 | Open, `Readiness::Failed` | Red wireframe: it failed and waits for its deadline to fall back | — |
| 7 | Open, `Readiness::Ready` | Solid; pops to about 1.7× when `ready_at` first appears and settles over one beat | — |
| 8 | Open, `Readiness::Running`, `attempt > 1` | Flickering wireframe, and the performer's cage tumbles: it is speculating again after its basis changed | — |
| 9 | Open, `Readiness::Running` | Wireframe at full brightness, at the performer | — |
| 10 | Open, `Readiness::Queued` | Wireframe at 80% size, dim, inside the slab | — |

Notes on the rules:

- **A squash is visible only as a second attempt.** `speculate` clears
  `valid` when it starts again (`engine.rs`, `speculate`), so a polled record
  never shows `valid == false` on open work. `attempt > 1` while running is
  the signal. `valid == false` does show on a settled `InvalidBasis` fallback.
- **A failure shows a beat early.** A failed resolve stays open with
  `Readiness::Failed` until its commit deadline (rule 6), so trouble appears
  before the pad.
- **Order settles the overlaps.** A record can be ready and also have fallen
  back for `DeadlineMissed` after a clock jump; the disposition rules come
  first, so it shows as trouble.
- **The record list mixes past and future.** `Timeline::statuses` returns
  settled history (newest 256) and open work together. The view keeps records
  whose start falls inside the road's window.

**The lock line is the commit deadline.** The kernel commits a cell at
`start − commit_margin`, one tick before it plays on the live clock
(`TickClock`, `crates/kaijutsu-hyoushigi/src/engine.rs`). Draw that as a thin
line across the road just ahead of the strike line. Notes inside it cannot
change. Neither `TrackInfo` nor `WorkStatus` carries `commit_margin`, so the
view assumes one tick until the work-status event sends it.

This mapping is presentation logic shared by every client: a pure function
from `(WorkStatus, playhead)` to a note state, with a test for every rule and
for each overlap. It goes in `kaijutsu-present`, which then depends on
`kaijutsu-hyoushigi` for `WorkStatus`; if the work-status event defines its
own wire type in `kaijutsu-types`, the function maps that instead and the
dependency goes away.

### Slack and lead

Two quantities, two words, both counted in beats on the performer's track (a
tick is a beat). The road draws them as lengths using the track's period.

- **Lead** is the kernel's word: how far ahead of a cell's start its
  preparation has to begin, from the performer's measured cost times the
  clock's safety factor. rc scripts see it as `KJ_LEAD_BEATS` (`cost_vars`,
  `crates/kaijutsu-server/src/beat.rs`) and plan their next target by it. It
  is how far ahead the performer *needs* to be. It is 0 when the performer
  has no measured sample yet.
- **Slack** is how far ahead the performer *is*: the beats from now to the end
  of its last covered phrase, counting only an unbroken run of covered phrases
  from the current one. A phrase is covered when its record is `Committed`, a
  `Fallback` with content, or open and `Ready`. The run breaks at the first
  phrase start with no record, or with a record that is not covered.
  Committed cells have left the open future and sit in history until they
  play, so slack reads both.

A performer is in one of three states:

- **Idle:** no open work and no covered phrase ahead. No floor is drawn. An
  idle performer is calm, not in trouble.
- **Working:** slack at or above its threshold. The floor shows slack in the
  performer's color.
- **Alarm:** slack below its threshold. The floor turns red and pulses.

**The threshold is the performer's lead, or `[heroic] alarm_beats` while lead
is 0.** A performer with less slack than its own lead cannot prepare its next
phrase in time at its measured speed. That is trouble at its own pace, with no
fixed number. Lead 0 cannot tell "no sample yet" from "costs nothing"; both
use `alarm_beats`.

Each value has one owner. Lead is the kernel's: the view reads it from the
work-status event once it carries lead, and until then uses the newest
record's `estimate` (ticks, without the safety factor) as a stand-in. Slack is
computed by a pure function in `kaijutsu-present`.

### Slots for later

- **Long tasks.** A coding context does not land on beats. Its work wants a
  sustain bar that spans its duration, not a disc.
- **Hardware lanes.** An audio node (microphone, Eurorack) is another lane.
  Inputs get a different note shape from performers.

## Composition

The road owns the center of the frame, and everything else points at it.

### Framing is solved, not placed

The view is one-point perspective with the vanishing point at the sun's
center. **The camera is solved from framing targets.** `[heroic.framing]`
names where things land on screen; the camera's pitch, height and distance
behind the strike line follow from them and the field of view, and are solved
again when the window resizes:

```toml
[heroic.framing]
fov_degrees = 58.0
horizon = 0.67        # fraction of frame height from the bottom
strike_line = 0.11    # fraction of frame height from the bottom
strike_width = 0.55   # fraction of frame width the strike line spans
orbit_radians = 1.0   # how far the player may orbit left or right
```

Pitch comes from the horizon alone: the horizon is the screen image of the
horizontal direction, so `pitch = atan((2·horizon − 1) · tan(fov/2))`. The
strike line's height then fixes the ratio of camera height to distance, and
its width fixes the scale. The solve is a pure function with tests that place
a world point and check its screen position. Changing how much sky there is
means changing one number.

Starting targets for the rest of the frame:

- **Sun:** centered on the vanishing point, half set, about a quarter of the
  frame wide.
- **Hills:** on both flanks, peaks below the top of the sun so they frame it.
- **Fire:** on both road edges, never covering notes in the outer lanes.
- **Gliders:** in the sky, crossing the sun; the larger spans about a third of
  the frame.
- **Ring:** about one and a half road widths across.
- **Performer panel:** mid height, about 30% of the frame wide, on the side
  away from the selected lane. It may overlap fire and hills, never the
  selected lane.
- **Flat overlay:** performer chips and history along the bottom.

### Depth bands and attention

| Band | Contents | Brightness |
|---|---|---|
| Overlay | Chips, history trace, key hints | UI |
| Now | Strike line, pads, lane names | Brightest |
| Road | Notes, performers, slabs, ribbons, markers, ring | Bright, fading with fog |
| Roadside | Fire, grid, hills | Capped below the road |
| Sky | Gliders, stars, optional context constellation | Thin lines only |
| Backdrop | Sun, sky gradient | The one large bright shape |

The eye lands in this order: the selected performer, any lane in trouble, the
notes, then the world. Spend brightness and saturation in that order. Only
notes, pads, performers and alarms reach full brightness.

Distance fog pulls the road and roadside toward the fog color. The sky,
backdrop and performer panel take no fog.

## Color

Each color means one thing. The heroic view uses the theme's scene lane
(`[scene.*]` in `theme.toml`, linear HDR, read through `ScenePalette`) and adds
its own `[heroic.hues]`.

- **White is now, and the player.** The strike line, the performer's core, a
  decision's pick, the player's instructions on the road. Nothing else is
  white.
- **Red is trouble.** Slack below lead, a missed deadline, a failed resolve. Red is
  never decoration, and no lane may be red.
- **A performer's color is identity.** Everything that belongs to a
  performer wears it: notes, cage, slab, ribbon, panel border, tether, ring
  arc. A lane whose contexts share one character takes that color for its
  floor; a lane with several characters keeps a neutral floor and lets the
  notes carry identity.
- **Magenta and cyan are the world.** Grid, rails and phrase lines. They belong
  to no performer.
- **Rainbow is energy.** Fire and hills only. Its chroma follows the intensity
  dial and, later, the player's mood signal.

```toml
[heroic.hues]
sky_top = "#06010f"     # also the clear color while the view is open
sky_mid = "#1d0638"
sky_low = "#5a0f6b"
horizon = "#ff2e88"     # horizon glow and grid magenta
fog = "#2a0845"
ground = "#0d0221"
hill_fill = "#12041f"
rail = "#00f0ff"
beat_line = "#234a66"
sun = ["#ffe66d", "#ff5e8a", "#c800ff"]   # top to base
trouble = "#ff2020"
now = "#ffffff"
```

The scene's `trouble` stays vivid red. The conversation uses a quiet variant
for sustained reading: plum-charcoal ground, lavender-gray text, muted violet
tool borders, rose errors, and cyan focus. Its borders and text have no glow.
The two lanes share hue families while keeping their own brightness budget.

### Identity hues: one rule, separate keys

The app has two hash-to-hue functions today, with different saturation and
lightness: `time_well::scene::accent_color` (`view/time_well/scene.rs`),
keyed on a context-type bucket, and `room::seats::nick_hue_degrees`
(`view/room/seats.rs`), keyed on a peer's nick, which is a connection. The
heroic view needs a third use, keyed on a performer. We make one hue *rule*
and keep each caller's *key*, because requester, performer and connection
stay distinct (AGENTS.md):

- **The rule is shared.** Hues are spaced by the golden ratio from a stable
  hash, so nearby keys get distant hues. Hues inside excluded bands are pushed
  out: red (trouble), and the world's magenta and cyan.
- **The key stays the caller's.** The heroic view keys on the performer's
  character, so a character keeps its color across tracks. The room keeps
  keying on the nick, and the time well on its context-type bucket.
- **Brightness stays the lane's.** Lightness and chroma come from the theme,
  so the UI lane and the scene lane keep their own brightness.
- **Tests** check that the excluded bands are never returned, and that the
  first eight keys of a fixed set are pairwise distinguishable.

Both old functions move onto the rule and are deleted. Their hues will shift;
that is expected and needs a look at the room and the well afterward.

### A rainbow ramp

The theme has no shared rainbow. `msdf_block.wgsl` has a screen-space HSV
rainbow for `MsdfBlockGlyphs.rainbow`, and `font_rainbow` lives in the UI
lane. HSV rainbows are uneven: yellow and cyan glare, blue sinks. We add one
ramp, `rainbow(t, chroma)`, defined in OKLCH, so equal steps in `t` look like
equal steps of hue and lowering `chroma` fades every hue toward gray together.
It exists twice, in Rust (`ScenePalette`) and in one shared WESL module
(`assets/shaders/kaijutsu/color.wesl`), and a test compares the two at sample
points. Fire, hills and the existing MSDF rainbow use it.

```toml
[heroic.rainbow]
lightness = 0.78
chroma_min = 0.02    # nearly gray at the bottom of the dial
chroma_max = 0.20
```

### Light

Nothing in the scene is lit. Every surface is unlit and emissive.

The app camera is already HDR with bloom and a prefilter threshold of 1.0
(`setup_camera`, `main.rs`). Spend bloom by staying below or going above that
threshold:

- Notes, pads, performers, the strike line, the sun and the fire go above 1.0
  and bloom.
- The grid, rails and hills stay near or below it.
- Text stays below it. Text never blooms.

Fire, the slack floor, slabs and ribbons blend additively. The performer panel
is opaque.

### Line and shape

Lines are thin and the same weight everywhere. Solid means ready or committed;
wireframe means provisional or environmental. The note states depend on this.

- **Note:** a low disc with slightly sloped sides.
- **Performer:** a procedural crystal inside a larger wire cage; see
  "Crystals".
- **Pad:** a flat ring.
- **Instruction marker:** a thin bar across one lane.
- **Ring:** arcs of a torus, one per performer.
- **Glider:** a few straight lines.

No textures except text and the sun. Shading is flat.

### Crystals

Every attached context gets its own crystal, generated from the context. **Hue
comes from the character; shape comes from the context.** Two contexts played
by one character on the same track share a color and differ in shape, so a
player can tell them apart without reading.

- **The shape is a seeded gem.** A bipyramid on an `n`-sided girdle
  (`n` from 3 to 8), with its own upper and lower elongation, girdle twist,
  and an optional flat table cut across the top. The seed is a hash of the
  context id, so the same context draws the same crystal in every session and
  every client.
- **The cage is the same shape, scaled up and wireframe,** so the core and
  cage read as one object. The core stays white: white is the performer's
  core.
- **Lineage shows as resemblance.** A forked context starts from its parent's
  parameters and mutates a little under its own seed. A rotation successor
  keeps its predecessor's shape, because it inherits the predecessor's
  attachment to the track. Siblings look related; strangers do not.
- **Shape stays readable at a distance.** Parameters are kept inside ranges
  where the silhouette stays distinct at the size of a crystal halfway up the
  road; a screenshot of eight seeds side by side is the check.

The generator is a pure function from a seed (and an optional parent's
parameters) to a closed convex mesh, with tests: the same seed gives the same
mesh, the mesh is closed and convex, and parameters stay in range. Meshes are
built once per context and cached.

## Scene elements

Each element shows one thing and reads from one piece of scene state.

### On the road

| Element | Shows | Driven by | Motion |
|---|---|---|---|
| Lanes and rails | One strip per track | Track list | Still. Reflow on a phrase boundary when tracks come or go. |
| Beat and phrase lines | Each track's ruler. Dim per beat, bright per phrase. Phrase lines continue across the grid in magenta. | Track period, `beatsPerPhrase` | Scroll toward the viewer |
| Strike line and pads | Now, one ring per lane | Clock, played and fallback events | Pad swells and whitens on a hit; a second ring expands and fades over 0.3 s; red on trouble |
| Lock line | The commit deadline | `commit_margin` (assumed one tick until the wire carries it) | Rides just ahead of the strike line |
| Notes | Work items | Each track's `WorkStatus` list | Scroll; pop when ready |
| Slack floor | Slack | Slack and lead (see above) | Length tracks slack; red and pulsing while slack is below lead |
| Performer crystal | Where the performer is working | Its running or newest admitted work | Hops between phrases in an arc; cage tumbles while it speculates again after a squash; flares when an instruction reaches it |
| Phrase slab | The phrase being prepared | Same | Jumps with the performer |
| Candidate stack | A decision player weighing options | Choice weights and the pick | Short bars riding the performer; length is probability; the pick is white |
| Text ribbon | A text model talking | The performer context's streaming blocks | Rises above the crystal and travels with it; newest line at the bottom |
| Instruction marker | A message the player sent a performer | Pending instructions | Falls onto its beat in 0.7 s, then rides the road; dims once the performer has heard it |
| Round ring | A round boundary, and who earned the round | Round clock, credit | Approaches for a whole round |

### Around the road

| Element | Shows | Driven by | Motion |
|---|---|---|---|
| Edge fire | Loudness | Band energies, intensity | One bar per beat cell, locked to the beat lines; tips flicker |
| Grid | Ground | Nothing | Still, except phrase lines crossing it |
| Spectrogram hills | The last few bars of sound | Band-energy history | One new row per eighth note, rolling away |
| Sun | The mood | Intensity, low band | Grows a little with energy; barely pulses on the beat |
| Gliders | Overseers | Score events | One wingbeat per bar; cross the sky over minutes |
| Sky and stars | Depth | Nothing | Still |
| Context constellation | The context forest, optional | Context tree | One star per context; a performer's star pulses while it works |

The gliders hold a place for a role: the council, or a critic, visibly
watching. One could dip toward a lane it is about to judge.

### On selection

| Element | Shows | Driven by | Motion |
|---|---|---|---|
| Dimming | Focus | Selection | Unselected lanes drop to about 30% |
| Performer panel | Everything about one performer | Selection and that performer's state | Opens at once on the side away from the lane |
| Tether | Which crystal the panel belongs to | Selection | A thin lane-colored line that follows the crystal |
| Chips | Name and state per performer | Performer state | Flat overlay |

## Motion and timing

Everything moves on the music's clock, and the farther a thing is from the
road, the slower it moves.

| Band | Tempo | Examples |
|---|---|---|
| Road | Continuous and quick | Notes scroll smoothly; hops take about 150 ms; pads pulse on the hit |
| Roadside | On the beat | Each fire bar is one beat cell; phrase lines sweep the grid |
| Far field | Half time or slower | Hills step once per eighth note; gliders beat once per bar; the sun barely moves |

**Nothing in the far field moves faster than something on the road.** The
prototype ran its hills on a free 10 Hz clock, and they looked frantic and
unrelated to the music. Stepping them on the eighth note fixed it.

**Acknowledgment is instant; effect is scheduled.** This is the most important
interaction rule in the scene.

- **Within a frame:** selection, the panel, the appearance of an instruction
  marker.
- **When the performer hears it:** anything that changes what a performer
  plays. The marker shows where that is, and the player watches it arrive.
- **On a phrase boundary:** rearrangements of the scene, such as tracks joining
  or leaving.

**Moving terrain is pinned where it meets something still.** The hills use an
envelope that is zero at the road edge and grows outward, so they never lift
off the ground:

```
// d = 0 at the roadside, 1 at the outer edge
envelope = d < 0.25 ? d * d / 0.25 : d
```

The fire's root stays on the ground; only its top moves.

| Motion | Duration | Feel |
|---|---|---|
| Performer hop | About 150 ms | Ease out, arc higher for longer jumps |
| Note pop when ready | One beat | Overshoot, then settle |
| Pad pulse | 300 ms | Sharp attack, linear fade |
| Instruction falling | 700 ms | Decelerates onto the lane |
| Performer flare | 800 ms | Sharp attack, slow fade |

At rest, with no input and nothing wrong, the scene is watchable for an hour:
slow, periodic and calm. The only quick events are the strike pads.

## Text

Text appears at four levels, and only the top one has to be comfortably
readable. All of it goes through the MSDF renderer. World-space text already
works: `view::time_well::panel::create_msdf_panel` renders glyphs into a
texture shown on a 3D quad, as the room nameplates and the well's reading card
do.

| Level | What | Requirement |
|---|---|---|
| Performer panel | Full state of one performer | Read at a glance. Opaque backing, fixed layout, no fog. |
| Lane names | One word at the strike line, in the lane color | Always legible; faces the camera |
| Marker labels | A few words above an instruction marker, white | Legible when near; cut at about 24 characters |
| Ribbons | A text model's streaming output | Texture first: it reads as "this one is talking" from across the room |

The performer panel's layout is fixed so the eye learns where things are:

```
+--------------------------------------------------+
| bass · musician                                  |   name, rc directory
| slack 14  lead 3 beats   phrase 6   ready        |   slack turns red below lead
|                                                  |
| E1     ################                     0.41 |   decision player: candidates,
| G1     #######                              0.17 |   bar length = probability,
| A1     #####                                0.13 |   the pick in white
| picked E1 G1 E1 D2 A1                            |
|                                                  |
| last 16 phrases: committed ▮▮▮▮▮▯▮▮ fallback ▮   |   history of dispositions
| est 420 ms  queue 12 ms  compute 380 ms          |   from WorkStatus.timing
+--------------------------------------------------+
```

For a text model the middle block is the last six lines of its output. The
panel is where a player checks a performer's history, so the history rows stay
even when space is short.

Rules for all text:

- Text never blooms.
- Text never sits over fire or the sun without a dark backing.
- Streams and numbers are monospace.
- Text has a minimum size measured by how large it appears to the viewer;
  below it, the text hides.
- Show tokens as they arrive. Do not fake a typewriter.
- Ribbons and the panel update a few times a second, not every frame.

## Shaders

Three shaders carry the look: the edge fire, the spectrogram hills, and the
sun. They are WESL materials loaded by asset path, like the existing
`assets/shaders/*.wesl`. Two firsts come with them:

- **A shared project module.** `assets/shaders/kaijutsu/color.wesl` holds the
  rainbow ramp and the fog function. Today every WESL file imports only Bevy
  modules.
- **A vertex shader.** The hills move vertices from a small history texture.
  Every existing material is fragment-only.

`shaders/shader_assets_test.rs` composes every mesh `.wesl` through Bevy's
shader cache and validates it with naga. Each new file is added to its list
by hand.

**Fog.** Bevy's `DistanceFog` is applied by `bevy_pbr`'s own fragment code;
custom materials have to apply fog themselves. Every road and roadside
material calls the shared fog function with `[heroic.fog]`'s start, end and
color. Sky, sun and panel do not.

### Edge fire

A tall quad stands along each road edge for the road's full length. It blends
additively, writes no depth, and is visible from both sides. The shader cuts
it into one bar per beat cell, locked to the lane's beat lines so the bars
scroll with them.

- **Bars, not a sheet.** The gap between beats is what makes it read as a
  spectrum.
- **Neighbors are similar.** The band index is a triangle wave along the road,
  so heights change gradually.
- **Hue travels.** The rainbow slides along the road over time and shifts with
  height.
- **The root is white.** Chroma drops in the lower half of each bar.
- **Intensity scales height and chroma together.** Low energy gives low gray
  embers.

The prototype's math, to port in intent:

```glsl
float cell = floor(vZ / beat_len);                       // one bar per beat
float b = abs(fract(cell / 30.) * 2. - 1.) * 15.;        // band index sweeps up, then back
float e = 0.;
for (int i = 0; i < 16; i++) e += uS[i] * max(0., 1. - abs(float(i) - b));
float h = min(.95, (.06 + e * .7) * (.25 + uV * 1.5));   // bar height
float n = ns(vec2(vZ * 3., vUv.y * 5.  - uT * 3.)) * .6
        + ns(vec2(vZ * 7., vUv.y * 11. - uT * 5.)) * .4; // two octaves of rising noise
float a = 1. - smoothstep(h * .4, h, vUv.y + (n - .5) * .7 * h);  // ragged top
a *= step(.14, fract(vZ / beat_len));                    // gap between bars
float core = 1. - smoothstep(0., h * .5, vUv.y);         // white root
```

A later step: feed each side, or each stretch, one track's energy, so the fire
shows who is contributing.

### Spectrogram hills

Each flank is a grid mesh, 12 cells across and 32 deep, drawn as wireframe
over a near-black fill so it hides what is behind it. A vertex shader reads a
history texture with one row per eighth note. Each row stores the peak of its
interval so short hits survive, and the shader blends two rows by phase so the
roll is smooth:

```
band  = d * 15                         // low bands near the road
value = mix(history[age], history[age - 1], phase)
y     = d * d * (1 + noise * 4)        // a still ridge rising from the road
      + value * envelope * 8.5 * gain
```

### Sun and sky

The sun is a disc with a vertical gradient through `sun`'s three colors and
seven transparent bands cut across its lower half, thickening toward the
base. The sky shows through the bands. The bands may drift slowly downward.
The sky is a vertical gradient through the three sky hues with the pink
confined to the lowest fifth; stars are plain points. Neither takes fog.

### Thin lines: undecided

Wireframe carries meaning here, so line drawing is a real choice. Three
candidates, to try side by side at the notes step:

1. **Line-list meshes** (`PrimitiveTopology::LineList`) with an unlit
   material: cheap and batched, but one physical pixel wide on every display.
2. **Retained gizmos:** a width in logical pixels, but fog and bloom behavior
   need checking.
3. **Quad-strip lines** expanded in a vertex shader: constant width and full
   control, at the most code.

Pick by screenshot.

## Where the data comes from

Put one scene-state layer between the sources and the visuals: a
`HeroicState` resource that sources write and every visual system reads.
Two drivers fill it, and they share its shape, so the visuals cannot tell them
apart.

### The fake driver

The fake driver writes plausible state with no kernel running, so the scene
can be built, tuned and screenshotted, and pieces can be built in parallel.
Its records have the same fields as the live ones (`WorkStatus`, `BeatRef`,
`TrackInfo`), so it doubles as a description of what the live driver must
supply.

```js
// per performer, every frame. now and writeHead are in beats.
if (mode === 'write') {
  const speed = baseSpeed * (Math.sin(t * 0.2 + i * 2.1) > 0.6 ? 0.3 : 1);   // a periodic stall
  writeHead += speed * dt;                        // baseSpeed 2.5 to 4 beats per second
  if (writeHead < now) writeHead = now;           // ran dry: deadline misses follow
  if (writeHead - now > 20) { mode = 'revise'; reviseStart = Math.floor((now + 10) / 8) * 8; }
} else {
  if (writeHead - now < 12) mode = 'write';
  if (reviseStart < now + 2) reviseStart = Math.floor((now + 10) / 8) * 8;
}
// a note exists at (lane, beat) when hash(lane, beat) < density; density 0.35 to 0.75
```

**Keep one struggling performer** (base speed 2.5) in the fake driver. Without
it there is never an alarm or a missed deadline, and those are the states the
scene most needs to get right. The fake driver is deterministic from a seed,
so a screenshot can be reproduced.

### The live driver: what exists today

| The view needs | Where it comes from today | Gap |
|---|---|---|
| The clock | `BlockEvents.onBeatSync` (`BeatRef`: beat, tempo, wall-clock stamp) every 8 beats, folded into a local phasor by `kaijutsu_present::beats::WellBeats`, which the time well and the tui already use | None for tempo and position. **No meter on the wire**: there are phrases (`beatsPerPhrase`), not bars. |
| Tracks | `listTracks` (`TrackInfo`: period, phrase length, playhead, playing, clock kind) | Polled; the time well polls every 5 s |
| Work items and their states | `kj transport work --track <t>` through `executeKj`; its `data` is `Vec<WorkStatus>` | Polled. No push, process-local, history reset on restart. |
| What played | Committed cells as score-context `BlockInserted` (with tick and track) and `onRenderCue` (with `onsetBeat`) | About one beat of warning per cell |
| Fallbacks and failures | `Disposition::Fallback` in the work list; failure Error blocks in the producer's context | — |
| Ribbon text | The performer context's block stream | Subscribing to each performer's context |
| Band energies | Nothing. The kernel's only audio analysis is offline beat tracking (`kj audio beats`); nothing measures level or spectrum. | See below |
| Lead | The measured per-performer lead reaches rc scripts only (`KJ_LEAD_BEATS`). Each record's `estimate` (ticks) is in the work list. | The measured lead is not on the wire |
| Slack | Derivable from the work list | — |
| Scores, levels, credit | Nothing | See "Gameplay" |

Two ways to get work items into the view:

1. **Poll `kj transport work --track` JSON** about once a second per track,
   alongside `listTracks`. It works today with no wire change, and it shows
   which fields the view actually uses. Two constraints:
   - **Poll with `quiet` set.** `executeKj` without `quiet` authors blocks for
     the command into the context (`kaijutsu.capnp`, `executeKj`), so a
     non-quiet poll would write a block per poll into the log. A display poll
     is the client's own bookkeeping, which is what `quiet` is for.
   - **It takes the track's timeline lock,** which the beat path also takes.
     Keep the rate low and watch beat timing while polling; if polling shows
     up in beat jitter, the event moves up the build order.
2. **A work-status event on the musical-time lane**, pushing `WorkStatus`
   transitions (and each performer's lead) as they happen. This is what "the stream is the
   truth" wants, and it is a new wire method, so it needs a design
   conversation.

We start with polling to light the view against a real kernel, then design the
event from what the view turned out to need. A polled verb at interaction rate
is acceptable as a bridge, not as the end state.

**Band energies.** The kernel links no audio drivers or hardware FFI;
audiod owns the PCM streams, and heavy audio inference runs as a hyoushigi
producer (`docs/audio-io.md`). Level and spectrum of what is playing need the
PCM, so they come from audiod or from the app's own audio sink; which one, and
how the numbers reach the view, is open. Until then, derive energy from
onsets: a committed cell's `onsetBeat` bumps its track's band, as the
prototype's fake spectrum did:

```
energy[b] = energy[b] * exp(-4 * dt) + small_noise
// every beat: +0.5 to band 0, half that to band 1
// every played cell: +0.9 to its track's band, half that to its neighbors
// cap at 1.5
```

That is honest — it is driven by what played — and it moves on the music's
clock.

**A real scenario for screenshots.** `crates/kaijutsu-server/tests/timeline_commitment_wire.rs`
drives fast, slow, obsolete and failing producers against manual beats. The
same controlled producers, run against the headless harness's kernel, are the
live driver's first test bench: every note state appears on demand.

## Clock and latency

The strike line sits where the listener's ears are. Three provisions allow it,
and calibration waits:

- **One visual clock.** The scene renders from the local phasor `WellBeats`
  already keeps (`kaijutsu_audio::LocalBeat`: it slews toward each sync inside
  a deadband and never replays missed beats; `docs/time.md`). The heroic view
  adds no second clock.
- **One signed offset.** Network delay, audio output latency and display
  latency fold into `[heroic] display_offset_ms`, applied to the visual clock.
- **Positions on inputs.** A player's judgment is about what they just heard.
  Any scoring input carries the visual clock's position at the keypress, so
  the kernel can attribute it.

Most latency is already in the picture: an instruction's delay is the marker's
distance up the road, plans that arrive late appear far up the road where
nobody looks closely, and late arrivals near now pop in with the ready
animation or show as a red pad.

## Gameplay

This part is loose. The brief's idea is a rhythm game turned around: the
performers play, and the player judges them. The judgment is a reward signal
that, in the long run, steers the performers toward what the people in the
room like — an endless song, with rounds to keep it manageable.

None of this exists in the kernel yet: no scores, no levels, no credit. What
maps onto existing mechanisms:

| Player action | Kaijutsu today | When it lands |
|---|---|---|
| Select a performer | Client only | At once |
| Send a performer an instruction | A message into the performer's context; async writers reach the next turn through the mailbox cursor (`docs/contexts.md`) | The start of the first work admitted after the performer's next turn reads it |
| Ask for a redo from a beat | A requested fallback or re-drive (`kj drive --score-at <tick> --fallback …`) | At that tick, if it is past the lock line |
| Score the performance up or down | Nothing | Needs design |
| Bump a performer's level | Nothing | Needs design |

Two rules hold however the game turns out:

- **Every action is acknowledged at once and shows when it will take effect.**
  So every call answers with the tick at which it takes effect, and the marker
  lands there.
- **Every score shows where the credit went, and enough history stays visible
  for the player to judge their own calls.** A player may be wrong about who
  deserved the blame. The scene makes that observable so they can learn.

Scores, credit and levels are durable feedback about performers. Where they
live, and whether the kernel owns them, is their own design conversation; the
client does not author them. They wait for that design. Until
then the fake driver may carry them so the scoring visuals (sparks, credit
bars, ring arcs, history trace) can be built and judged.

## Restraint

The scene has a lot of moving light, and the signal that matters most, slack,
is quiet. Protect it.

- **One intensity dial.** `[heroic] intensity` scales everything environmental:
  fire height, hill amplitude, environmental bloom, glider brightness. Energy
  moves the scene within the range the dial allows. It ships at a moderate
  setting.
- **Trouble quiets the world.** While any lane is in alarm, the environment
  drops by about a third. The red floor is the loudest thing on screen.
- **Selection dims.** Unselected lanes fall back so the selected lane and its
  panel read as a pair.
- **No strobing.** Nothing large flashes. Nothing changes faster than about
  three times a second except the small strike pads.
- **Reduced motion.** `[heroic] reduced_motion = true` freezes the far field
  and the fire's flicker. The road keeps moving, because the road is the
  information.
- **Never color alone.** Trouble is red and it pulses. Provisional is a color
  and a wireframe. The pick is white and the longest bar.

**Leave room for a headset.** A seated, head-rotation-only VR version may come
later. Keep the scene in world units; only the flat overlay assumes a screen.
Keep the far field still relative to the world. Size text by how large it
appears to the viewer, not by pixels.

**Cost.** A few hundred notes, a handful of performers, two fire quads and two
terrain meshes. Notes share one mesh and a few materials so Bevy batches them.
Text is the expensive part.

## In the app

### Where it lives

- **A new screen.** `Screen::Heroic` in `ui/screen.rs`, entered and left the
  way `Screen::Room` is (`view/room/mod.rs`, `enter_room` and `exit_room`):
  claim the shared `AppCamera` with a marker, set the clear color, insert
  `DistanceFog` if used, spawn one `HeroicRoot` and parent everything to it;
  on exit, despawn the root, remove what was inserted, restore the camera.
- **Code.** `crates/kaijutsu-app/src/view/heroic/`: `mod.rs` (plugin, enter,
  exit), `framing.rs` (the camera solve), `state.rs` (`HeroicState`),
  `fake.rs`, `live.rs`, then one module per element group (`road.rs`,
  `notes.rs`, `performers.rs`, `environment.rs`, `text.rs`, `selection.rs`).
  Materials in `shaders/`, shaders in `assets/shaders/heroic/`.
- **Shared logic.** The `WorkStatus` → note state mapping and the slack rule go
  in `kaijutsu-present`, next to `beats.rs`.
- **Theme.** A `HeroicData` type beside `SceneData` in
  `crates/kaijutsu-types/src/theme.rs`, parsed from `[heroic]`, held by the app
  as its own resource. The tui parses the same file and ignores the section.
  The shipped defaults go in `assets/defaults/theme.toml`. The host's
  `/config/kernel/theme.toml` is never reseeded, so an existing host file has
  no `[heroic]` section: every `HeroicData` field takes the shipped default
  when it is missing, and `kj config show|reset` shows or restores the
  embedded file.

### Structure: `bsn!`, values: TOML

Bevy 0.20.1's scene notation is the `bsn!` macro. The `.bsn` file format is
not released yet (`bevy_scene-0.20.1/src/spawn.rs`). So:

- **Structure** — what is parented to what, which components an element has —
  is written with `bsn!` and `spawn_scene`, as the time well's furniture is.
  That file records what does not fit yet: `create_msdf_panel` returns a bundle
  `bsn!` cannot name, so text panels are inserted after the scene spawns;
  meshes are passed in as handles to avoid one frame of invisible geometry.
- **Values** — framing, colors, durations, thresholds — live in `[heroic]` and
  are read every frame or on change, never baked into a spawned scene.

### Live tuning

The theme reaches the app over RPC on connect and on reconnect
(`connection/actor_plugin.rs`); there is no reload while connected. Today only
`[scene.post]` applies to a live camera, and the rest waits for the next room
entry. The heroic view applies all of `[heroic]` live, because tuning by eye
is how this scene gets made:

- Add an action that fetches `theme.toml` again, bound in the heroic view.
- Framing, fog, intensity and durations are read every frame or on change.
  Colors go to material uniforms on change.
- A test changes `HeroicData` and checks that the camera and a material
  uniform follow without leaving the view.

The flow is: edit `/config/kernel/theme.toml`, press the reload key, look.
Values worth keeping go back into `assets/defaults/theme.toml`.

### Input

A new `InputContext::Heroic` and an arm in `derive_contexts`
(`input/context.rs`); bindings in `input/defaults.rs`; `PopLevel` leaves the
view. Selection by number keys and Left/Right fits the room's navigation
grammar. **`Ctrl+A k` enters the heroic view**: it is free in both prefix
tables (`input/prefix.rs`, `crates/kaijutsu-tui/src/keys.rs`). The tui binds it
too, answering "not built yet" the way its `Ctrl+A d` does, so the chord
means one thing in both clients. `docs/input.md` has no prefix table today;
the chord is recorded there when it is bound.

### Seeing it

Every step ends with a screenshot. The headless harness does this without a
desktop: `contrib/headless-app.sh start`, force the screen with BRP
(`NextState<Screen>` = `{"Pending":"Heroic"}`), then
`contrib/headless-app.sh shot <name>`. Amy's runner (`contrib/kj`) is for
looking together.

## Build order

Build from the backdrop forward, and show Amy a screenshot at the end of every
step.

1. **Color groundwork.** The identity-hue rule, with the time well and room
   moved onto it and both old functions deleted. The OKLCH rainbow in Rust and
   `color.wesl`, with the comparison test. `HeroicData` and its parse.
2. **Stage.** `Screen::Heroic`, enter and exit, the framing solve, sky, sun,
   ground, grid, fog, the reload action. With nothing moving it already reads
   as the place.
3. **Road and clock.** `HeroicState`, the fake driver, lanes, rails, beat and
   phrase lines scrolling on `WellBeats`, the strike line and lock line.
4. **Notes.** Every note state and the pads. Try the three line options here.
5. **Performers.** Crystal, cage, slab, hop, slack floor, alarm.
6. **Live driver.** Polling `listTracks` and `kj transport work`, the beat
   sync, render cues, onset energy. Run it against the controlled-producer
   scenario. Real data this early tells us whether the note states are right.
7. **Environment shaders.** Fire and hills on onset energy. Tune bloom here,
   with the road already bright in frame.
8. **Text.** Lane names, ribbons, candidate stacks.
9. **Selection.** Dimming, the performer panel, the tether.
10. **Gameplay visuals.** Markers and instructions on the live driver; sparks,
    credit bars, ring and history trace on the fake driver until the kernel
    has scores.
11. **Sky life.** Gliders, and the constellation if wanted.

Steps 1 and 2 fix the color rules, the stage and the state layer. After step
3, the shaders, the panel layout and the sky are independent of the road logic
and of each other, so separate subagents can take them. Notes and performers
share too much to split. The composition review belongs to Amy and the
context leading the build, not a subagent: someone has to look at the whole
frame.

### Tests

Behavior that can fail gets a test that can fail:

- The framing solve places world points at the requested screen fractions.
- The identity-hue rule never returns an excluded hue.
- The Rust and WESL rainbows agree at sample points.
- Each note-state rule, and each overlap the order settles, has a case.
- Slack breaks at a missing or uncovered phrase; an idle performer is not in alarm.
- The alarm fires when slack drops below lead, and uses `alarm_beats` when lead is 0.
- The fake driver is deterministic for a seed and includes a deadline miss
  within a fixed run.
- New `.wesl` files compose in `shader_assets_test.rs`.
- `derive_contexts` returns `Heroic` on the new screen.
- Live tuning: a changed `HeroicData` reaches the camera and a uniform.

### Visual checks

Run these on a screenshot or a short capture. Each has a plain pass condition.

| Check | How | Pass |
|---|---|---|
| Squint | Blur the frame | A bright road, a sun, and dark flanks |
| Slack at a glance | Hide the overlay and look for one second | You can name the performer closest to running dry |
| Freeze frame | Any single frame | Provisional and ready notes are distinguishable in every lane |
| Sound off | Mute and watch | The beat is visible in the beat lines, pads and fire |
| White audit | Find every white pixel | Each is now, the player, or a pick |
| Red audit | Find every red pixel | Each is trouble |
| Roadside seam | Watch the hills at their loudest | They never lift off the ground at the road edge |
| Far-field tempo | Watch the hills and gliders | Nothing out there moves faster than the notes |
| Panel | Select each performer in turn | The panel never covers the selected lane, and its text is comfortable to read |
| An hour | Leave it idle | It is calm. Nothing nags. |

If a check fails and the fix is not obvious, show Amy the frame.

## Starting values

From the prototype, in units where one lane is 1 wide. They go into
`[heroic]` as defaults and are tuned by eye from there.

```toml
[heroic]
intensity = 0.6
reduced_motion = false
display_offset_ms = 0
alarm_beats = 8.0           # alarm threshold until a performer has a measured lead
lane_width = 1.0
road_seconds = 20.0         # how much future the road shows
units_per_second = 1.8      # 0.9 per beat at 120 bpm
unselected_opacity = 0.3

[heroic.fog]
start = 18.0
end = 55.0

[heroic.sizes]
note_radius = 0.34
note_height = 0.14
pad_radius = 0.38
performer_radius = 0.3
cage_scale = 1.5
hover_height = 1.1
hop_arc = 1.6
fire_height = 3.4
fire_offset = 0.35          # outside the road edge
hill_width = 26.0
hill_length = 64.0
hill_inner_edge = 6.0       # from the road center
sun_diameter = 20.0
sun_distance = 60.0
ring_radius = 4.5
panel_size = [6.5, 4.9]
panel_yaw_degrees = 26.0

[heroic.durations_ms]
hop = 150
pad_pulse = 300
instruction_fall = 700
performer_flare = 800
```

The prototype's camera numbers are not carried over: the framing solve
replaces them.

## Open questions

- **Structure: direction, not yet decided.** Bars, meter, phrase grouping,
  sections and tempo come from **structure tracks**: tracks whose cells are
  plans, published at the tick they are declared, about later ticks. Tracks
  stay unaware of each other; the models planning other tracks read the
  structure tracks as an input, as they read `KJ_HEARD`. A plan about a tick
  at or before the playhead is refused; for a future tick the latest committed
  plan wins. The structure is hashed into dependent work's basis directly.
  Open: how two tracks' tick grids line up, since each counts from its own
  start. `docs/time.md` takes this once it settles. In the heroic view the
  structure is the road's ruler: bar lines from meter, sections as gantries,
  and the round ring as a section boundary.
- **The work-status event.** Its fields, its lane, and whether lead rides on
  it. Lead is per performer and per track; it reaches the view only through
  rc today.
- **Instructions.** Which tick a message to a performer "takes effect" at,
  and how the kernel reports that back.
- **Scores, credit and levels.** Their own design, in the kernel.
- **Band energies from audiod.** The analysis and how it reaches the app.

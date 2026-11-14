# Chameleon — models playing on a shared pulse

Chameleon is the music application of kaijutsu: contexts play music on a beat
the kernel owns. This document describes the band as it runs today, checked
against the code and against a live run on 2026-11-14. When it disagrees with
the kernel, fix the document.

Read with it: `docs/hyoushigi.md` (the timeline, resolvers, and commitment),
`docs/tracks.md` (the track), `docs/midi.md` (clocks and the wire),
`docs/pcm.md` (samples and clips), `docs/audio-daemon.md` (the hardware
node), `docs/midi-next.md` (device profiles), and `docs/audio-inference.md`
(what model work costs). History is in `docs/devlog.md`.

## What the kernel does for a band

The kernel coordinates anticipation and commitment on a shared pulse. A track's
beat advances whether or not a player is ready. Each player works at its own
pace; its output counts when it is ready before the moment it was made for.

```sh
kj context create musician --name groove   # a player, attached to track `groove`, stopped
kj transport play --track groove           # start the beat; the player's turns begin
kj transport work --track groove           # each attempt: target, timing, outcome
```

Four words carry the model:

- **Admission.** A player's turn is admitted for one absolute tick in the
  future. The target is fixed at admission; late work cannot move it.
- **Readiness.** The work must be ready before its target tick. A result first
  seen after its target is discarded.
- **Commitment.** One tick before the target, the kernel commits ready, valid
  work. At the target the phrase is written to the track's score as a durable,
  immutable block.
- **Fallback.** Work that failed or missed its tick uses the fallback declared
  at admission. So does a model turn whose basis changed, because a model turn
  is not run again: `last-good` repeats the same performer's latest
  accepted phrase on the track, `skip` leaves silence. With no earlier phrase, `last-good`
  is silence too.

The fallback is a musical choice. On a vamp, repeating the last phrase sounds
correct, so a slow or absent player does not break the performance. Amy
describes the workload as "near-term latent decisions": models "dancing at
their own pace, integrated just in time for the performance."

`docs/hyoushigi.md`, "The core contract" and "Model turns with an intended
score tick", owns the mechanism.

## Tracks and transport

The beat belongs to the **track**: a named clock domain with a clock source, a
score, and attached contexts (`docs/tracks.md`). Players attach, play, and
rotate; the track stays. The score lives in a durable **score context**
labeled `score-<track>`, which has no performer and takes no turns.

`kj transport list` shows every track: state, clock kind, BPM, phrase length,
attachments, playhead, and score context. A track that is in the database with
nothing attached in this kernel process shows `dormant`.

| Verb | Effect |
|---|---|
| `attach` / `detach` | Put a context on a track, or take it off. With no flags, `attach` takes the current context and derives the track from its label. A new attachment is stopped, with its turn loop armed. |
| `play` / `pause` / `stop` | Start, freeze, or stop the track's clock. `pause` and `stop` also tell every sink to drop scheduled events and silence sounding notes. Attachments and the turn loop setting stay. |
| `tempo <bpm>` | Set the beat period. |
| `ooda on\|off` | Arm or disarm one attached context's turn loop without touching the clock. |
| `clock system\|modeled` | Choose the clock source (see "Tempo"). |
| `rotate` | Set or clear the phrase count at which a player is replaced by a successor (at least 2). |
| `repeat --at <tick>` | Place the calling context's performer's last good phrase at a future tick. No model runs. |
| `work` | Show pending work and the newest 256 outcomes, as a table or `--json`. |
| `delete` | Rename the track aside. The score context stays; a later attach with the same name starts a new track. |
| `list` | Show all tracks. |

A context becomes a beat participant when its rc runs `kj transport attach`.
Nothing in the kernel is specific to musicians: any rc directory whose
`create/` scripts attach makes beat participants.

A kernel restart does not re-attach contexts: every track shows `dormant`
until something attaches again, and then it is stopped. On that attach the
committed score reloads from the score context, and the playhead starts at the
later of the score's last tick and the track row's saved playhead.

## Tempo

A track runs a local clock. An external master is observed and modeled; only
low-rate references cross the wire, never pulses. Every cue's time derives
from the scheduled beat grid, not from the moment a task woke up
(`docs/midi.md`, "The one timebase").

- **`system`**: a fixed-tempo local timer. `kj transport tempo --track <t>
  <bpm>` sets it. Every track starts here.
- **`modeled`**: phase-locked to an external MIDI master. The audio node that
  owns the master's USB port learns tempo, phase, and drift
  (`kaijutsu_audio::clockin::ClockEstimator`) and sends `ClockEstimate`
  references; the track fires on the master's integer beats with slew-limited
  corrections (`ModeledClock::apply_estimate`, `kaijutsu-server/src/clock.rs`).
  Until the first reference arrives the track keeps its last tempo.
  References older than 5 s are dropped.

Three ways to get a rack's tempo into a track:

1. Read it off the gear and run `kj transport tempo`.
2. Record a few bars and run `kj audio beats <file>`, then set the measured
   BPM.
3. Send MIDI clock from the rack to a node running `kaijutsu-audiod`, attach
   that node's capture context to the track, and switch the track to
   `modeled`. `kj audio devices --node audio/<host>` shows what the node can
   see.

Start with the first. The modeled lock is not required to play.

## Players

A player is a context whose performer's rc directory attaches it to a track.
The shipped rc directory is `musician` (`assets/defaults/rc/musician/`), and
the shipped character `musician` uses it.

| Script | What it does |
|---|---|
| `create/S00-stance.md` | The player's stance: the beat does not wait; fill the phrase window. |
| `create/S05-chair.kai` | The chair, from `chairs/<name>.md`, named by the context's `KJ_CHAIR` (default `bass`; `drums` ships too). An unknown chair fails the step and lists the chairs. A chair states the role, the register, and the exact ABC header. |
| `create/S10-binding.kai` | The loadout: `drive` only. The model sees no tools. |
| `create/S15-abc-primer.md` | The output contract: raw ABC, header order, note lengths. |
| `create/S20-arm.kai` | `kj transport attach`, on the track named by `KJ_BAND` when the context has one, else on a track named for the label. |
| `create/S30-hydrate.kai` | `kj context hydrate --window 16`: pin everything so far as the prefix and slide a 16-block window over later turns. |
| `tick/S10-drive.kai` | One admitted turn per phrase (below). |
| `rotate/S10-rotate.kai` | The page turn: `kj fork --preset spawn --switch && kj transport attach && kj transport play`. |
| `fork/S40-hydrate.kai` | Set the hydration window again on a filtered or compact fork. A full fork is left alone. |

**The turn.** On the player's cadence (default: every phrase, 32 beats) the
scheduler snapshots `KJ_TRACK`, `KJ_TICK`, `KJ_PHRASE`, `KJ_PHRASE_BEATS`,
`KJ_TEMPO`, `KJ_PULSE`, `KJ_EPOCH_NS`, and `KJ_HEARD`, then runs the `tick` rc.
`KJ_COST_MS`, `KJ_LEAD_BEATS`, and `KJ_OPEN_WORK` come with them: the
performer's measured cost, that cost times the clock's safety factor in beats,
and the performer's unsettled work on the track. The shipped script admits one
turn:

```sh
kj drive --track "$KJ_TRACK" --score-at "$target" --fallback last-good --prompt "…"
```

The target is one phrase ahead until the performer has a measured cost. After
that it is `KJ_LEAD_BEATS / KJ_PHRASE_BEATS + 1` phrases ahead. While the
performer's earlier phrase is still open the tick admits nothing. A model
slower than one phrase therefore plays every other phrase and is not
interrupted at each deadline.

The phrases before a far target have no model work. `KJ_UNCOVERED` in the
context's environment says what fills them: `repeat` (the default) places the
performer's last good phrase at each with `kj transport repeat --track <t>
--at <tick>`; `silence` places nothing. Any other value fails the tick.

The prompt is written as a user block, so it hydrates as the new turn. The
model's whole text reply is the phrase. The runtime validates it as ABC and
hands it to the timeline; invalid notation becomes an Error block in the
player's context and the fallback plays.

**Hearing.** Score blocks are not hydrated into a player's conversation.
`KJ_HEARD` is the player's only view of what was played: the last 8 phrases of
committed notation on its own track, from every producer on that track, as a
JSON string in the prompt. A player does not hear other tracks, and it does
not hear clips.

**No tools.** A player gets no model-visible tools. Its reply is the score, so
it needs none, and a small model given a tool palette has stalled without an
error.

**The page turn.** `kj transport rotate --every N` sets it; it is off by
default. At the horizon the scheduler detaches the parent in the same beat, in
Rust, and then runs the `rotate` rc, which forks a `spawn` child and attaches
it. The child inherits the attachment row. The intent is that each fork is a
section of the song.

The `rotate` rc is one verb, `kj context rotate`: the successor runs the
`create` rc again, so it has a fresh stance, chair, primer, and loadout, the
same label, and the same track; the predecessor is archived
(`docs/prompts.md`, "Rotating a context"). The same verb run by hand does the
same thing, and is how a changed model reaches a running player. A cadence
under 2 is refused: the horizon replaces that phrase's tick, so a cadence of 1
would never play.

**The model.** A character carries its model; a context copies it when made.
`kj character create bass2 --model <backend>/<model> --max-tokens 1024 --rc
musician` makes another player on another model.

### A band

```sh
kj character create bassist --model <backend>/<model> --rc musician
kj character create drummer --model <backend>/<model> --rc musician
kj context create bassist --name bass  --env KJ_BAND=band
kj context create drummer --name drums --env KJ_BAND=band --env KJ_CHAIR=drums
kj transport play --track band
```

Players made with the same `KJ_BAND` share one track, so they share one clock
and hear each other in `KJ_HEARD`. Each has its own chair, and each chair is
its own character: work, measured cost, and `last-good` are keyed by performer,
so a second live context of one character on a track is refused. The
performer stays the same across a page turn while the context changes. The drums chair's
header carries `%%MIDI channel 10`, so its notes play as General MIDI
percussion while the bass plays on channel 1. A page turn keeps the chair and
the band, because rotation copies the context's environment.

Limits today:

- `KJ_HEARD` holds 8 phrases for the whole track, about 4 per player with two.
- The kernel checks that a phrase parses as ABC. It does not check its
  length; the prompt alone asks for the right number of beats.
- The tick prompt still fixes 4/4 and one beat per note or chord. Nothing
  seeds a chart, so key and form come from the chair. The Herbie Hancock
  *Chameleon* vamp (B♭ Dorian, B♭m7 to E♭7) named the project; no shipped
  chair plays it.

## The score and the sound

- **Notation is the score.** A committed phrase is `text/vnd.abc`. A deriver
  inserts the MIDI rendering as a sibling asset block in the score context.
- **The sink renders.** Committed cells publish as render cues.
  `kaijutsu-audiod` schedules ABC as MIDI into its local ALSA queue
  (`kaijutsu-audio-runtime/src/dj/midi.rs`). The app draws from the same cues
  and does no hardware I/O.
- **Phrases, not bars.** The track policy holds `beats_per_phrase` (16 or 32
  in practice). Bar lines belong to the notation.
- **Samples use the same path.** A clip cell
  (`application/vnd.kaijutsu.clip+json`, placed with `kj play --track`) names
  media in CAS and renders through the same cue (`docs/pcm.md`). The MIME type
  selects the sink.
- **Capture.** `kaijutsu-audiod` captures incoming MIDI with ALSA receipt
  times and sends it to the kernel in batches as score blocks. Device
  knowledge is in profiles (`/config/midi/devices/`, `kj midi`); a device
  context changes the gear beside the band and is not on the beat
  (`docs/midi-next.md`).

To hear a track you need a `kaijutsu-audiod` node with a synth or device on
its MIDI ports. There is no web player. `python3 contrib/install-audiod.py
--host <kernel-host>` installs a node (`docs/audio-daemon.md`, "Linux
service"). Without a sink the score still commits; `kj cas get <hash> --out
phrase.mid` takes a rendered phrase out by hand.

## What a live run looks like (zorak, 2026-11-14)

One player per track, 120 BPM, 32-beat phrases, so 16 s per phrase.

| Model | Result | Measured |
|---|---|---|
| `deepseek/deepseek-flash` (shipped `musician`) | Every phrase committed | 1.7 to 4.1 s per turn; ready 23 to 28 beats early |
| `nagaya/Qwen3.8-Flash-Next` (local, thinking) | Every phrase missed its tick | Still thinking at 16 s; each miss is interrupted |
| `nagaya/lfm3` | No phrase | The server answered 503: the model was unloaded and its host was away |

What the run showed:

- The chain from tick rc to admitted turn to ABC validation to committed score
  works on the current kernel.
- A thinking model needs its thinking bounded, or a longer lead than one
  phrase, to play at this tempo.
- On that day the model resolver estimated zero cost and the tick always
  targeted one phrase ahead, which is why the slow model never played. The
  tick now plans from measured cost ("Players", "The turn"); that change has
  tests and has not yet run on a live kernel.
- The faults found are in `docs/issues.md`, "Music refresh: live run findings".

## Omni models

An omni model reads and writes media as well as text. `lfm3-omni` on the
nagaya reports, in its identity on 2026-11-14, PNG, JPEG, WAV, MP4, and WebM
as inputs, WAV as output, and a 32768-token window. We start with it because we know it; the design below is not specific
to it.

This section is direction, not built behavior. It will change as experiments
report.

### What already fits

- **The timeline does not know content types.** A cell is a position, a way to
  produce content, and a state. Content is a CAS hash plus an open MIME
  string. An audio phrase is a cell like any other.
- **The MIME type already selects the deriver and the sink.** A clip cell is
  a small JSON record that names media in CAS; audiod already plays it. The
  commit step is narrower: it validates ABC and clip records and passes every
  other type through unchecked.
- **Admission, readiness, and fallback do not depend on what the work
  produces.** A model that takes 10 s to write 4 s of audio is a producer with
  a long lead time, which is the case the resolver contract was written for.

### What assumes text

- **A turn's reply is one text block.** The timed-drive handoff takes one
  completed output block and validates its text as ABC; the admission carries
  no MIME type. Nothing accepts a media part as the phrase.
- **The conversation carries text and images only.** A model message is text,
  tool use, tool result, image, or reasoning. The Claude and OpenAI builders
  send images; the council builder replaces one with a marker. No builder has
  an audio part, and a media part needs all three.
- **A block has one content string and a closed content type.** `audio/wav`
  and a clip record both map to `Plain`. Hydration turns only image asset
  blocks into parts and skips every other asset, so an audio block in a
  context is invisible to its model.
- **Token estimates know text and a flat cost per image.**
- **Hearing is notation in a prompt.** `KJ_HEARD` cannot carry sound, and a
  captured MIDI window has no path to a model.
- **Models have no media capability record.** The nagaya's identity lists each
  model's input and output types; the kernel's model rows do not hold them.
- **Model output has no path into CAS.** The kernel can store bytes; no
  provider stream delivers media to it. Separately, a client cannot put bytes:
  `/v/cas` is read-only.
- **The wire carries one content string per block.** One media reference per
  block works, as images do; a reply with several parts has no shape yet.
- **The hydration window counts blocks.** Audio spends tokens by the second,
  so a window in blocks does not bound cost.

### The shape if omni came first

1. **A turn reads and writes typed parts.** Each part is a MIME type and a CAS
   reference; text is one type among several. Storage is close to this, one
   block per part. The conversation, the provider builders, and the turn
   handoff are not, and this is the largest change in the list.
2. **Capability belongs to the model row.** Accepted and produced MIME types
   are recorded per model, read from the server where it reports them. Backend
   kind does not imply them.
3. **A part the model cannot read gets a declared projection.** A text-only
   player hearing audio gets a derived transcription or notation, or a marker
   that says what was left out. Derivers already exist at the commit barrier;
   this is the same mechanism facing the other way.
4. **The reply's MIME type selects the commit path.** `text/vnd.abc` is
   validated as notation. `audio/wav` is stored in CAS and checked for length
   against its window at the admitted tempo; the kernel then writes a clip
   record that names it and commits the record.
5. **Hearing is a query, answered in the listener's types.** A player asks for
   a window of one or more tracks. A text model receives notation. An omni
   model receives a captured window or a rendered mix, budgeted in seconds.
   A rendered mix needs a renderer, which the kernel does not have; audiod
   owns playback. A captured window is the nearer source.
6. **Lead time comes from measurement.** Audio generation time grows with the
   audio's length, so a fixed one-phrase lead is wrong for it. The cost
   samples the timeline already keeps become the input to `estimate_cost`.
   This is the smallest change in the list and helps text players today.
7. **Short audio first.** A whole-phrase artifact must be complete before its
   tick. Fills, one-shots, and one or two bars fit that rule and a 32768-token
   window. Continuous generation needs cells smaller than a phrase and is
   later work.

Two earlier statements change. "A symbolic score, never audio" becomes:
notation is preferred because it is small, editable, and renders at any tempo;
audio is accepted where notation cannot say the thing, and an audio phrase is
bound to the tempo it was made at. "Bytes never ride the track" stays true:
the cell holds a reference and the bytes stay in CAS.

### First experiments

These need no kernel change and run against the nagaya directly:

- Give `lfm3-omni` a rendered bass phrase as WAV and ask for ABC. This
  measures hearing as transcription.
- Ask `lfm3-omni` for a short audio fill at a stated tempo. Measure generation
  time against audio length, and length error against the window.
- Run `lfm3` as a text player in two forms: a free ABC turn, and a council
  decision that picks one cell per bar from a menu. An October prototype made
  the choice in about 250 ms; the free turn is unmeasured.

## Open items

`docs/issues.md` owns open work: "Anticipation and commitment — iteration
order", "Hyoushigi / Musician — open remainder", "Musician create cannot name
its track", "Tracks do not re-arm after a kernel restart", "System 1
musicians share the council server with the gate", and "Music refresh: live
run findings". Larger designed-and-unbuilt pieces:

- **Two tracks sounding at once.** A tune names its channel with `%%MIDI
  channel N` (1 to 16; 10 is GM percussion, and `K:perc` defaults to it), and
  the sink schedules and flushes MIDI per score context. Still open: a flush
  for one track returns the sink's clock to wallclock and drops every track's
  buffered phrases, the sample scheduler drops every context's pending clips,
  and a track does not yet bind to a device role (`docs/midi.md`,
  `docs/midi-next.md`).
- **A chart.** Nothing gives a player key, form, or changes; a producer
  context that sets and revises one is designed, not built.
- **Mailbox flush on the grid.** Async events for a player arrive at its next
  turn, not at a beat crossing.
- **Automation.** Knob movements as cells on the timeline.
- **Traps in musical time.** The heartbeat variables ship; `trap`-style
  schedules do not.

## Starting a jam

1. `kj context create musician --name <track>` makes the player and its track,
   stopped.
2. `kj transport tempo --track <track> <bpm>` sets the tempo.
3. `kj transport play --track <track>` starts the beat.
4. `kj transport work --track <track>` shows whether phrases commit.
5. `kj transport stop --track <track>` stops the clock. A turn already running
   keeps running; `kj interrupt <context>` ends it.

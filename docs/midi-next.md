# MIDI Next — device profiles, bindings, device contexts, `kj midi`

> **Status:** living document, seeded 2026-07-15 (Amy + Claude design
> conversation); updated 2026-08-02 (moltar bench session, rack live): two
> constraints became doctrine — **presence is sink-fed** (kernel never
> shells) and **native MIDI backends per platform** (macOS is coming) —
> and the roster now carries observed hardware facts. This is where the
> *device knowledge + device I/O* half of the MIDI story accumulates as we
> build; expect it to change shape.
> Companions: `docs/midi.md` (transport/clock/realtime doctrine — settled
> direction), `docs/tracks.md` (the substrate), `docs/chameleon.md` (the
> music application; its "per-track MIDI channel" open item is what slice 2
> pays for), `docs/config-ownership.md` (the storage + rc precedent).

## The problem

Real gear is arriving: each device has MIDI settings (channels, CC
assignments, note maps, clock behavior) and capabilities (mono vs poly, MPE,
what's controllable at all) that today live nowhere — or worse, live as folk
knowledge in memory files ("TiMidity has GM drums on ch 10"). Every place we
touch a device — render-cue routing, the ear's `played_by` mapping, a
musician deciding what to send — needs the same facts. This doc answers two
questions: what is the shared, durable shape for that knowledge, and what is
the one surface every player uses to act on it?

## Prior art (surveyed 2026-07-15, so we don't re-derive it)

- **MIDNAM** (Logic, DP, Ardour): XML device-name documents — patch/CC/note
  names per bank/channel. The right *idea* (device facts as portable
  documents), wrong economics: volunteer-maintained XML rots, and it's
  names-only — nothing says "this synth is mono."
- **Ableton / Bitwig**: invest in *controller* scripts (full programs per
  device), punt on instruments — CC mappings are stored per-project, so the
  mapping doesn't follow the device. The anti-pattern: setup state welded to
  the song.
- **Cubase Device Panels**: the cautionary tale for "generic MIDI map table."
  Maximal genericity up front (describe any device's panel as CC/SysEx
  widgets) produced a schema so arcane users abandoned it. **Lesson: grow the
  schema from real consumers, never from generality.**
- **ReaLearn** (Reaper): best-in-class generic *mapping engine*
  (source→transform→target), but device knowledge is still re-taught by hand.
- **MIDI-CI Property Exchange** (MIDI 2.0): devices self-describe as JSON
  resources — `DeviceInfo`, `ChannelList`, `ChCtrlList`, `ProgramList`. None
  of our gear speaks it, but we **borrow its vocabulary** so our schema
  converges with where the industry is going; PE-capable gear will someday
  fill in its own profile.

## The core split: profile vs binding

Two things every DAW conflates, kept separate here:

1. **Device profile** — durable facts about the gear, independent of any
   session. Portable, shareable, slowly changing. A kernel document.
2. **Binding** — this setup, this session: track *bass* → Poly 2, drum lane →
   ch 10 on TiMidity, KSP is clock master. Routing state; lives where routing
   lives — on the track / attachment model (`tracks.md`), *referencing*
   profiles. A binding says "track → *device.role*" (e.g. `minibrute.notes`,
   `drumbrute.kick`) and the profile resolves the role to port + channel +
   note. Never a raw channel int on the track.

### Inside the profile: settings vs capabilities (the ground-truth split)

- **`settings`** — the mutable half: receive channel, velocity curve,
  per-track channels, CC assignments. **The device is ground truth**; a SysEx
  pull refreshes this section and overwrites it.
- **`capabilities`** — the authored half: mono/poly, MPE mode semantics, note
  ranges, "panel is not CC-controllable," drum-note maps, prose gotchas.
  **The document is ground truth**; a pull that contradicts it raises a flag,
  never a silent overwrite.

Format: **prose + data hybrid** — a model-facing `.md` body carrying the
machine-facing versioned-JSON sections (jq-able, the clip-record precedent).
The prose is not documentation *about* the profile; it **is the skill body**
a specialized context boots with (next section). The rc `.kai`/`.md` split,
applied to devices. Conventions (set by the first drafts): channels are
**1–16** in profiles (wire byte = channel−1); best-knowledge facts ride in
an `unverified` list per JSON section until confirmed against the real unit;
role channels may reference settings (`"@settings.receive_channel"`) so a
pull re-routes bindings automatically.

**Profiles are rc-style buckets, not single files (Amy, 2026-07-15).** The
first TiMidity draft exposed it: a static `settings` section bakes in
host-current facts (which box, which client) that don't carry to the next
machine TiMidity runs on. So `/etc/midi/devices/<name>/` is a bucket of
`SXX-*.{md,kai}` exactly like rc: **static knowledge is `.md`** (capabilities,
skill prose — document ground truth), **the current picture is `.kai`
output** (locate the device now, synthesize settings from live state). The
ground-truth split maps onto file types. This is what makes a profile
*portable*: it stops naming zorak — wherever TiMidity runs, its locate step
finds it. TiMidity is the reference case for kai-synthesized profiles
("we'll always have it around somewhere in the rig").

**Amended 2026-08-02 — kai reads kernel state, never host commands.** The
original sketch had `.kai` shelling `aconnect -l` or snooping a local
`timidity.cfg`. That's doubly wrong now: (1) the kernel routinely runs on a
different box than the gear (zorak kernel, moltar rack) — kernel-side host
probing reads the *wrong machine's* ALSA graph; (2) "mostly just works
without shelling out" is a design constraint (Amy, 2026-08-02). Doctrine:
kai synthesizes profile sections **only from kernel-visible state** — the
sink-fed `/run/midi` presence store (next section) and the VFS — and the
sinks are the only processes that touch platform MIDI APIs. No subprocess
anywhere in the path.

## Device contexts: profile + model + toolbox (the skills angle)

The consumption model that makes profiles pay rent daily (Amy, 2026-07-15):
a **specialized context per device** — e.g. *haiku + subharmonicon + the
MIDI toolbox*. Flip to it, say "turn the reverb down 25%" (or whatever the
device actually has), and the model works it out from injected knowledge.
Everything needed already exists:

- **rc symlinks are the injection mechanism.** A `subharmonicon`
  context_type's rc bucket symlinks the profile
  (`ln -s /etc/midi/devices/subharmonicon /etc/rc/subharmonicon/create/S20-device.md`);
  the `.md` routes into the system-prompt slot at hydrate. No new machinery —
  init.d-style composition doing its job.
- **The narrow loadout is what makes a cheap model workable.** The chameleon
  lesson: small models hang on big tool surfaces. A device context's toolbox
  is roughly *send CC / send notes / read profile / read state* — footguns
  absent by construction. The model needn't know MIDI in general, only this
  device's documented controls. This is the loadout-as-focus doctrine
  (`docs/instrument-design.md`) with a perfect concrete case.
- **Relative commands need state.** "Down 25%" requires knowing where the
  knob *is* — readable from **`/run/midi/<device>`**, the provenance-tagged
  state store (defined in the `kj midi` section below). Absolute
  commands work day one; relative ones only for controls the profile marks
  relative-safe on this device.
- **Snapshot semantics ride along:** rc scripts snapshot at instantiation, so
  a profile edit reaches a live device context at next fork — consistent with
  every other rc consumer; stated here so nobody's surprised.

### The side channel (decided 2026-07-15)

A device context is a **side channel**, not a track attachment — it tweaks
the device while the band plays; the track doesn't know it exists:

```
KERNEL ─────────────────────────────────────────────────────────────

  score lane (the band)           side channel (the tech)
  ────────────────────            ─────────────────────────────
  musician context                device context "subh"
    │ producer turns,               boot (rc @create):
    │ ooda-armed                      profile .md → system prompt
    ▼                                 (snapshot; fork to refresh)
  track "subh"                      turn time (fresh reads):
    clock → playhead                  /run/midi/subh, KJ_TEMPO…
    committed score cells           │
    │ materialize                   │ kj midi cc subh reverb -25%
    ▼ (speculation lead)            │   ├─► writes /run/midi/subh
  RenderCue: score                  ▼   ▼
  (lead = seconds ahead)          RenderCue: control (lead ≈ 0)
    │                               │
────┼───────────────────────────────┼───────────────────────────────
    └───────────────┬───────────────┘
                    ▼
  APP SINK (owns the USB): routes both by profile → port/channel,
  schedules into its local ALSA queue → device
  (human hands on the panel are a third, unmodeled writer — a sent-
   provenance /run value is a hope, not a fact; the ear or a pull corrects)
```

Properties: (1) no playhead, no beat arming, never commits score cells — its
output is control cues converging with score cues only at the sink; (2)
knowledge has a freshness split — profile at boot via rc, live facts at turn
time via `/run` + heartbeat vars; (3) crosstalk-as-feature with one
guardrail: flush-on-stop must be **per-context** so stopping the track never
kills an in-flight control cue (the known per-track flush issue, now
load-bearing here too); (4) it's a *default posture*, not a wall — a
musician's loadout may include `kj midi`, and side-channel sends can later be
**promoted to data cells on an automation lane** if we want replayable
tweaks. Side channel now, promotable later.

## Storage and identity

- **`/etc/midi/devices/<name>`** — kernel-owned per
  `docs/config-ownership.md`: kernel sole owner, edited via `kj` (a
  `kj midi` verb family), no host files. Optional embedded seeds for gear we
  ship knowledge of.
- **Identity**: the universal **MIDI Identity Request** (`F0 7E 7F 06 01 F7`)
  fingerprints nearly any device (manufacturer/model/firmware) — build first;
  tiny and vendor-neutral. Profiles carry the fingerprint plus
  **backend-neutral match strings** (amended 2026-08-02): port display-name
  substrings and USB `vendor:product` IDs — never ALSA client numbers or any
  backend-specific handle, so the same profile matches under CoreMIDI (see
  "Platform backends"). A hotplugged port then resolves to a profile and the
  ear's `played_by` gets a real name ("KeyStep Pro track 3", not "USB MIDI
  2x2 port 1").
- **DIN-attached gear is invisible to enumeration** (observed 2026-08-02:
  the foot pedal on the moltar rack). A DIN device rides behind its host
  interface's port — no USB ID, no client name, no announce event. Only
  profile knowledge can name it: the host interface's profile declares what
  hangs off its DIN jacks (a `din_children` fact, authored), and identity
  exchanges over that port are the one way to *verify* it. Presence for DIN
  children is therefore inferred (host port present + authored claim), never
  observed — and the profile should say so.

## Presence is sink-fed (decided 2026-08-02)

What makes "plug it in and kaijutsu just works" true, and the concrete form
of Amendment A above:

- **The app already watches hotplug.** The ear subscribes to the ALSA System
  Announce port (shipped, `midi_in.rs`); verified live 2026-08-02 — rack
  power-on auto-subscribed KeyLab MIDI/DAW and KeyStep Pro ports with zero
  code changes. Announce is the trigger; nothing polls.
- **Matching happens in the app**, against profile match strings (the
  backend-neutral set above). The app holds the platform truth (port names,
  USB IDs); the kernel holds the profiles. Profiles reach the app the same
  way any config does; match results flow back.
- **Presence flows app→kernel as a wire event** — the reverse direction
  `commitCapture` already practices. On match (and on unplug), the app
  reports `{device, port_facts, present, at}`; the kernel writes it into
  **`/run/midi/<device>`** as provenance-tagged facts (`source: sink`).
  `kj midi list` on any node then shows what's live *where* — across the
  whole rig, because every sink reports into the same store.
- **The kernel never shells, ever.** No `aconnect`, no exec. The sink-fed
  store is the only source of "current picture" facts; kai synthesis reads
  it (bucket section above).
- **Presence is connection-bound** (amended 2026-08-02, after review). A sink
  that crashes, loses its network, or is `kill -9`'d never sends its unplug —
  so a record must not be able to outlive the connection that made it. The
  kernel stamps each record with **its own** per-connection id (never anything
  the sink says about itself) and, when the connection dies, *removes* every
  record attributed to it: back to **unknown**, the honest state. Not
  `present=false` — that would claim an observation nobody made. Accepted
  trade-off: removal also drops the latest-timestamp guard for that device, so
  a stale report queued by a reconnecting sink can briefly land; the sink's
  next fresh report (it re-states its whole picture on reconnect) corrects it.
  A self-healing wrong answer beats a permanent one. Implementation joins the
  existing per-connection teardown: `ConnectionState::drop` in
  `kaijutsu-server`, the same Drop that kills subscriptions and the session's
  context entry.
- **The report carries the sink's `sinkHost`** — display and provenance only,
  so `kj midi list` can answer "live, but *where*" across a multi-machine rig
  (a `host` column, and a tagged `host` fact in `/run/midi/<device>`). It is
  never a key: hostnames collide, go stale, and can be wrong, and reaping by
  one would let a sink erase another's records.

Plug → Announce → match → report → addressable. No polling, no exec, no
host-naming in profiles.

## Platform backends (decided 2026-08-02)

macOS support is planned (the MacBook is a first-class client machine), and
it constrains the sink now:

- **The `MidiDispatch` trait is the seam.** The app's sink already lives
  behind it with the `alsa` dep cfg-gated to Linux. Cross-platform = **two
  native backends behind one trait**: `alsa` (Linux, scheduled seq queues)
  and `coremidi` (macOS, native `MIDITimeStamp` scheduled sends).
- **midir is rejected**, despite being the obvious cross-platform crate: it
  is send-now only. The timing doctrine (`docs/midi.md`: schedule into the
  sink's local queue, back-date on arrival) requires timestamped scheduled
  output, which both native APIs provide and the lowest common denominator
  does not. We pay for two backends to keep the one thing doctrine can't
  give up.
- Corollaries: match strings stay backend-neutral (identity section), and
  the ear/exchange clients need the same per-platform split when the mac
  sink lands — same trait-seam pattern, stated now so nobody reaches for a
  shortcut later.

## `kj midi` — one emit surface for scripts, contexts, humans

(Amy, 2026-07-15.) kaish scripts, the device context's toolbox, and a human
one-liner all need the same thing: emit MIDI through the kernel without
touching a device. One verb family serves all three — **the `kj midi` verbs
ARE the device context's tools**, so building the verb builds the loadout.
`kj` is getting big, but this is a noun with subverbs (`kj audio` /
`kj transport` precedent) and a separate surface would fragment discovery for
exactly the contexts we keep narrow. It also retires the whip-up-python
pattern: future instances lean less on system exec, and `kj midi` works from
any context on any node with zero ALSA deps in the script.

Architecture is already decided by `midi.md` law — **the kernel never touches
hardware**. `kj midi cc` composes a small **control `RenderCue`** (lead ≈ 0)
down the existing wire to the sink owning the port; same path as `kj play`,
different payload. Emit stays sink-side, so a kaish script anywhere can play
gear on the laptop.

- **Verbs**: raw first — `kj midi send <device> cc|note|pc …`,
  `kj midi panic [device]`; profile-resolved names second
  (`kj midi cc subh vco1-level -25%`) once routing (slice 2) lands;
  `kj midi identify` / `kj midi pull` join the same noun (next section).
- **`/run/midi/<device>` is provenance-tagged state** (decided 2026-07-15):
  every entry is `{value, source, at}` with three provenances — **sent** (we
  emitted it; `kj midi` writes this for free — a hope, not a fact),
  **observed** (the ear saw the device say it; controllers that echo),
  **pulled** (the device answered an exchange — next section; Arturia-class
  gear, meaningless for a Subharmonicon). Latest timestamp wins mechanically,
  but the source rides along so the consumer judges — no resolution engine;
  record provenance, let the model reading it reason. The symmetry: exchange
  feeds `pulled`, ear feeds `observed`, `kj midi` feeds `sent` — three
  producers, one store; the settings-vs-capabilities ground-truth discipline
  extended to runtime state. Consequence for profiles: a control is
  **relative-safe** only if some provenance can actually refresh it on this
  device — the profile should say which.

## SysEx: the exchange pattern (decided 2026-07-15)

Identity Request, Arturia param get/set, settings dump, firmware ops later —
all one shape: *send bytes at a port, collect a matching reply, bounded by a
timeout*. That's a **call**, not a cue:

- **Wire = an `exchange()` method on the sink capability** the app already
  registers for cue push — Cap'n Proto is bidirectional and promise-returning
  methods are free. Kernel calls `exchange {port, payload, reply_match,
  timeout}`; app runs it on a **dedicated ALSA exchange client** (the same
  client-separation render and capture already practice, so the ear never
  sees request/reply traffic); returns bytes or a loud error (unplugged =
  error result, never a hang). Serialized per-port so replies can't
  interleave. First *round-trip* member of the mirror-pair set
  (RenderCue↔CaptureBatch and BeatSync↔ClockEstimate are fire-and-forget).
  The rejected alternative — correlated cue + capture with a sysex mime —
  reuses more but threads a transaction through two fire-and-forget mailboxes
  and pollutes the score path with non-musical bytes.
- **`kj midi pull <device>`** = exchanges that refresh the profile's
  `settings` section. **`kj midi identify`** = the universal Identity Request
  over the same method.
- **Day-one escape hatch, no exchange needed:** `kj midi send <device> sysex
  <hex|file>` — fire-and-forget raw bytes down the ordinary cue path. Most
  whipped-up python is exactly this; the python-retirement pattern doesn't
  wait for exchanges.
- **Ear obligation is only *unsolicited* SysEx** (someone presses "dump" on a
  device): don't crash, drop-and-count initially, capturable-with-a-mime
  later (recorded residual). ALSA SysEx fragment reassembly is an
  implementation detail inside the app's exchange client.

**Arturia**: proprietary but decoded family protocol (`F0 00 20 6B …` param
get/set). Working reference code:
[soyersoyer/sysex-controls](https://github.com/soyersoyer/sysex-controls), a
Linux MCC-replacement that reads/writes settings for a range of Arturia
(+ Akai/Korg) devices — our decoder ring. Caveat: KeyStep Pro / BeatStep Pro
*sequencer banks* unsupported there so far — day one we pull device settings,
not pattern contents (pattern pull → score is a tantalizing later item).
Deeper spelunking:
[dsgruss's KeyStep firmware RE notes](https://dsgruss.com/notes/2020/10/02/keystep1.html).

### Payload sizes, and the third shape (2026-07-15)

Real-world SysEx: param get/set ≈ 10–15 B; identity reply ≈ 15 B; single
patch dumps hundreds of bytes (DX7 voice = 163 B); bulk memory dumps 4–100 KB
(KSP sequencer banks live here); firmware 100 KB–MBs, always chunked with
inter-chunk delays or ack handshakes. **The bottleneck is the MIDI wire, not
our network** (DIN = 3,125 B/s — 1 MB of firmware ≈ 5.5 min *at the device*;
even USB gear enforces chunk-and-wait because device buffers are small). So
the question is never how bytes cross the LAN; it's who runs the pacing state
machine — and that's the sink, always: pacing is local-to-the-hardware
timing. Big blobs move via **CAS, not SFTP** — the `Inline | Cas` seam cue
payloads already define (`docs/pcm.md`); a captured bulk dump
is a candidate for the *first heavy payload* that forces the deferred
client→kernel CAS write surface (`midi.md` M2).

Three shapes by duration, the repeatable taxonomy:

| Shape | Duration | Payload | Exists? |
|---|---|---|---|
| **Cue** (fire-and-forget) | scheduled instant | Inline \| Cas | shipped |
| **Exchange** (bounded call) | sub-second, timeout | inline both ways | this design |
| **Transfer job** (sink-paced) | secs–mins, progress + cancel | CAS both ways | **deferred** — nothing on the roster needs it to jam |

The exchange/job line isn't size (100 KB inline is nothing to capnp) — it's
*duration and interactivity*: a firmware flash wants progress + cancel, not
one promise held open five minutes. Settings pulls and identity are
exchanges; firmware and Sample-Dump-Standard territory becomes a job when
something actually needs it.

## Keeping it current — grooming tracks (seeded 2026-07-15)

Kai-computed profile sections are fresh at hydrate and stale thereafter; the
refresh cadence wants to be a **track**: a slow clock + a probe attachment
(`ooda_armed: false`) firing kai on beats — `kj midi identify` sweeps, KSP
settings re-pulls, `/run/midi` staleness checks, pulled-vs-document drift
flags. Chameleon's cue traps are "cron in musical time"
(`docs/chameleon.md`); grooming tracks are the same machinery at ops tempo —
kaijutsu-style cron, and much bigger than MIDI (archiving, index grooming,
compaction). Seeded as its own backlog item in `docs/issues.md`; profile
refresh is likely its first consumer.

## Plugin instruments (VST/LV2/CLAP) — brooding (2026-07-15)

Software instruments beyond TiMidity — Surge XT is the motivating case
(open-source, CLAP-native, deep parameter surface). Three paths, not yet
decided:

1. **Standalone plugin host as a device (near-zero new code).** Run Surge XT
   standalone (or Carla/jalv for arbitrary plugins) as its own process with
   ALSA MIDI in + PipeWire audio out, and profile it exactly like TiMidity —
   another software device with an ALSA client name. Crash isolation for
   free (plugins are notoriously crashy; the wire-sink doctrine already says
   keep hardware-adjacent risk out of the kernel — same logic keeps plugin
   code out of the app).
2. **CLAP in-app via clack** — already anticipated by `docs/chameleon.md`
   ("CLAP hosting (clack) when plugins land"). The prize: **CLAP parameters
   are introspectable and settable**, so plugin instruments get *actual*
   read-back — the best-case provenance (`observed` becomes "read the real
   value"), full `relative_safe`, and profiles that could be **auto-generated
   from param enumeration** (the MIDI-CI Property Exchange dream, realized
   locally). The costs: crashy third-party code and RT audio threads inside
   the Bevy app.
3. **The likely synthesis: a headless CLAP-host *sink*** — a separate
   kaijutsu process speaking the same cue/exchange protocol as the app,
   hosting plugins, exposing params via `exchange()`. Isolation *and*
   introspection, and it converges with the `midi.md` M4 headless edge sink
   rather than inventing a new species.

Direction to sit with: **standalone-first** (a Surge XT profile costs one
bucket, proves plugins-as-devices), host-sink when the M4 shape firms up.

## Where models beat the DAWs

DAW device support fails on *maintenance economics* — volunteer XML rots.
Three moves change that:

- **Manual → profile drafting.** A model reads the device manual once and
  drafts the profile; human curates; kernel stores. Marginal cost of a new
  device drops from "someone writes a control script" to "one conversation."
- **MIDI-learn as conversation, verified by the ear.** With M2 capture, a
  model interrogates reality: "wiggle the filter knob" → CC 74 arrives →
  the mapping lands in the profile *with provenance*. DAW MIDI-learn is this
  loop with the human doing the bookkeeping.
- **Profiles as boot knowledge, not lookups.** The device-context angle
  above: the profile is *in the system prompt* of the context that plays the
  device. DAWs can't do this at all.

## The roster (Amy's gear, priority order)

| Device | Role | Profile notes |
|---|---|---|
| **Minibrute** (original) | first consumer — hanging off the laptop running kaijutsu-app | Tiny profile: one receive channel, notes+bend+mod, **mono**, analog panel not CC-controllable. A handful of Brute Connection globals are SysEx-settable (sparsely documented; [Hackabrute](http://hackabrute.yusynth.net/MINIBRUTE/standard2SE_en.html) for architecture). Hand/model-draft it — needs no pull machinery. |
| **KeyStep Pro** | usual clock master (`midi.md` topology); first *pull* target | 4 sequencer tracks on configurable channels, drum mode, CC config — big enough to make SysEx pull worth building. Doesn't *pass through* large SysEx (thru-routing limitation) but answers its own config protocol (MCC does exactly this). **Live on moltar 2026-08-02**: USB `1c75:0218`, client "KeyStep Pro", one MIDI port. |
| **1010music Bitbox** | **center of the system — the deep-dive device** | `midi.md` topology said "deliberately not on MIDI for now (recording path)"; direction updated — Bitbox MIDI IO is coming and we go deep: per-pad note/channel trigger maps, CC parameter control, clock. Likely bonus: 1010music stores presets as **XML on the microSD card** — a config-ingestion path that skips SysEx entirely (verify against Amy's unit's model/firmware). |
| **Polyend Poly 2** | Eurorack MIDI→CV bridge (loft) | Profile *must* carry its mode (first-fit poly / channel-per-voice / MPE) because the mode changes what a "channel" means. Config is on-device; profile is authored, not pulled. |
| **Moog Subharmonicon** | MIDI-out target; likely first *device context* | Semi-modular, 2 VCOs + 4 subs, polyrhythmic sequencers. Note→VCO routing and globals per its MIDI implementation chart (verify from manual when it lands on the bench). |
| **KeyLab 88 mkII** | promoted — live on the moltar rack | Controller: DAW maps, pads, faders/knobs, CV outs. Same Arturia SysEx family — KSP pull machinery should mostly transfer. **Live on moltar 2026-08-02**: USB `1c75:02cb`, client "KeyLab mkII 88", two ports (MIDI + DAW) — the profile must name both roles. |
| **1010music Bluebox** | rack mixer — on the bench now | **Observed 2026-08-02**: USB `368e:0007` enumerates as *audio interface only* — no MIDI endpoint, no seq client. Its MIDI is TRS DIN (or disabled in settings; verify on the unit). First concrete case of gear that's *USB-present but MIDI-invisible* — presence via USB ID, MIDI via a DIN path or not at all. |
| **Foot pedal** (unidentified) | on the rack, DIN-only | Invisible to USB enumeration (2026-08-02) — the motivating case for the `din_children` doctrine (identity section). Identify on the bench: what is it, whose DIN jack does it hang off, what does it send. |
| **DrumBrute** (original) | lower priority | Analog drum machine; pads send fixed-ish notes (MCC-configurable) — profile is mostly a **drum-note map**, the same shape TiMidity's GM drum profile needs. |
| **TiMidity (zorak)** | software device, already in use | GM soundfont synth, **drums on ch 10** — currently folk knowledge in a memory file; belongs in a profile. Cheap proof profiles aren't hardware-only. |

## Slices

1. **Devices become available** (reordered 2026-08-02; sub-steps in build
   order):
   1. ~~Verify the shipped hotplug/ear path against live gear~~ — **done
      2026-08-02** on moltar: rack power-on auto-subscribed KeyLab + KSP,
      zero changes.
   2. Embed the `assets/defaults/midi/devices/` seeds; `/etc/midi/devices/`
      namespace; `kj midi list|show`. Draft **KeyStep Pro** and **KeyLab**
      profiles alongside the existing minibrute/timidity seeds — live bench
      gear beats the roster order.
   3. ~~**In-app match + presence wire event**~~ — **done 2026-08-02**. The
      matcher is a pure, backend-neutral function
      (`kaijutsu-app/src/midi_match.rs`: names + USB IDs in, device + role
      out; ambiguity refuses rather than guesses) fed by the ear's existing
      announce watcher; `reportMidiPresence` carries `{device, present,
      backend, ports, at}` to the kernel, which records it in an **ephemeral**
      in-memory store rendered read-only at `/run/midi/<device>`
      (`kaijutsu-kernel/src/midi_presence.rs`). `kj midi list` gained a
      live/absent/**unknown** column — unknown is load-bearing: a restarted
      kernel with no sinks connected knows nothing and says so. Deferred with
      a seam in place: USB `vendor:product` enrichment (`PortFacts::usb_id` is
      never filled on Linux yet — matching runs on name substrings).
   4. ~~**`kj midi send`/`panic`**~~ — **done 2026-08-02**. Raw control cues
      ride the EXISTING `RenderCue` wire under a new mime,
      `application/vnd.kaijutsu.midi-control+json`
      (`kaijutsu-audio/src/midi_control.rs`): a small JSON envelope carrying
      the **device name** plus hex-encoded complete MIDI messages, each with
      an `offset_ms` (how a gated note's Note Off rides the same cue). Zero
      capnp change — the envelope lives inside the existing inline payload,
      and the port-anonymous score path is untouched. Kernel side
      (`kj/midi.rs`): `send <device> note|cc|pc|sysex`, `panic [device]`;
      channels 1-16 at the surface; the **profile** is the gate (unknown
      device = loud error), **presence is not** (absent/unknown warns and
      sends anyway — the sink drops what it can't route). Sink side
      (`dj/midi.rs`): the app's matcher ships a device→address table to the DJ
      thread (`DjCtl::MidiRoutes`), and a control cue rides a **per-device
      `ctl:<name>` port wired to its device by subscription** — visible in
      `aconnect -l`, exact (its one subscription IS the device, so the score
      never leaks in), and load-bearing: the first live bench session
      (2026-08-02) proved the original design's DIRECT `set_dest` emit is
      silently discarded by hardware — the ALSA seq→rawmidi bridge only opens
      a kernel port's output substream while a subscription targets it
      (user-client ports don't share the trap, which is why loopback tests
      passed while no instrument ever heard a note; diagnosed against
      `/proc/asound/*/midi0` Tx counters). The wire is re-verified per cue
      (one query at control-cue pace) so replug self-heals; routes updates
      prune stale ports. Unroutable = loud warn + drop, **never** a fallback
      to the auto-connected render port. Deferred with the seam in place:
      role-aware port choice (slice 1 takes the device's *first* matched
      port), `/run/midi` sent-provenance (slice 3), CoreMIDI address forms
      (`parse_alsa_addr` refuses them rather than guessing).
   5. ~~The **`exchange()` sink method** + `kj midi identify`~~ — **done
      2026-08-02**, and it closes slice 1. The wire is one appended method on
      the existing subscriber callback (`BlockEvents.exchange @15
      {portOrDevice, payload, replyMatch, timeoutMs} -> reply`) — the second
      and last capnp change of the slice, as planned. The shape that made it
      work: an exchange is **addressed, not fanned out**, so the kernel needs
      a way to call ONE connection. Presence already knew which
      (`SinkAttribution::connection`), so the kernel gained a matching
      registry (`midi_exchange::MidiExchangeRegistry`, connection → channel,
      registered by the server at `subscribe_blocks*` and reaped on
      disconnect exactly like presence) and the server owns the task that
      turns a channel request into the capnp call — the kernel still holds no
      capnp capability and no hardware. App side: a **third** ALSA client
      (`kaijutsu-exchange`, alongside render and the ear) on its own thread,
      one dialogue at a time, the request and reply each riding a temporary
      subscription taken and dropped inside the exchange (send-side pin
      required by the same seq→rawmidi substream rule as control cues), so
      the ear never sees request/reply traffic. Timeouts are a
      ladder (app 2 s, then +0.5/+0.75/+1 s outward) so the layer that
      actually wedged is the layer whose error a player reads; every failure
      — unknown device, absent device, sink not serving exchanges, silent
      device, unparseable reply — is a named error, never a hang and never an
      empty reply. `kj midi identify` files the parsed reply at
      `/run/midi/<device>` as the doc's **third provenance, `pulled`** — the
      first fact in that store the *device itself* asserted. It survives
      re-reports that keep the device live, and dies on any unplug or reap
      (what returns to a port may be a different unit; re-plug ⇒
      re-identify). Deferred with seams in place: per-port (rather than
      per-sink) serialization, role-aware port choice, a CoreMIDI worker, and
      the four-crate timeout ladder living in four files (`docs/issues.md`).
2. **Routing consumes profiles.** The render sink resolves "track →
   *device.role*" through the profile to port + channel — paying for the
   per-track channel-routing open item (`midi.md` open questions,
   `chameleon.md` open items) with profile vocabulary instead of raw ints;
   per-context flush rides along (the side channel needs it too). `kj midi`
   gains profile-resolved control names. The ear maps `played_by` through
   fingerprints. Minibrute-on-laptop plays from a track end-to-end.
3. **First device context.** An rc bucket symlinking a profile + a narrow
   loadout (the `kj midi` verbs) + a cheap model; "set X to Y" absolute
   commands. The `/run/midi` provenance store follows for relative commands
   (sent-provenance only at first).
4. **SysEx settings pull.** `kj midi pull` against the KSP, sysex-controls as
   reference; the ground-truth split enforced (settings overwrite,
   capabilities flag).
5. **Later:** rc-style profile buckets with kai-synthesized sections
   (TiMidity as reference case) + a grooming track refreshing them; Bitbox
   deep dive (XML preset ingestion?); KSP/DrumBrute pattern pull → score;
   plugin instruments (Surge XT standalone first); manual→profile drafting
   as a repeatable flow; ear-verified MIDI-learn; MIDI-CI PE for future gear.

## Open questions

- **Bitbox specifics** — model/firmware on Amy's unit, XML preset format,
  whether MIDI config is per-preset or global. Blocks the deep dive, not
  slices 1–4. **2026-08-02**: the rack unit on USB is a *Bluebox* (mixer);
  is a Bitbox also in the rig, and is the Bluebox now the deep-dive target?
  Also verify whether Bluebox USB-MIDI exists behind a setting.
- **The foot pedal** — identify the unit and its DIN host; first real
  `din_children` entry.
- **Role vocabulary** — how rich is a profile "role"? (`notes` / `kick` /
  `voice[3]` / KSP `track2`?) Grow from the routing consumer; resist
  generality (the Cubase lesson). **Convention seeded 2026-08-02** (gemini
  review concurred): `direction` describes *wire physics* (source /
  destination / bidirectional), `musical` describes *intent* (may a track
  ever render to it?) — the KeyLab `daw` role is the motivating case
  (bidirectional wire, `musical: false`). Keep them separate; never infer
  one from the other.
- **Device instance vs device model** (gemini review, 2026-08-02) — the
  presence store keys on profile name, so two identical units (or two sinks
  claiming one device) collide at `/run/midi/<device>`. Slice-1 stance:
  first-available-instance, acceptable for this rig today. When a second
  identical unit arrives, presence needs instance identity
  (`<sink>/<device>`-shaped or similar) and bindings need to name an
  instance, not a model. Deliberately deferred — grow it from the real
  collision, not before (the Cubase lesson again).
- **Pull conflicts** — a settings pull disagrees with an in-flight binding
  (device receive channel changed under a live track): who wins, how loud?
  Leaning: the pull commits; the binding's resolution goes stale loudly at
  next use.
- ~~**Device-context ↔ track relationship**~~ — **RESOLVED 2026-07-15: side
  channel by default** (diagram + properties in "Device contexts" above); a
  musician's loadout may still include `kj midi`, and automation-lane
  promotion is the upgrade path. The binding model assumes neither.
- **SysEx patch editing** (Ctrlr/Edisyn territory) — deliberately out of
  scope; nothing on the roster needs it to jam. Revisit only with a concrete
  need.

# MIDI Next — device profiles, bindings, device contexts, `kj midi`

> **Status:** living document, seeded 2026-07-15 (Amy + Claude design
> conversation). This is where the *device knowledge + device I/O* half of
> the MIDI story accumulates as we build; expect it to change shape.
> Companions: `docs/midi.md` (transport/clock/realtime doctrine — settled
> direction), `docs/tracks.md` (the substrate), `docs/chameleon.md` (the
> music application; its "per-track MIDI channel" open item is what slice 2
> pays for), `docs/config-crdt-ownership.md` (the storage + rc precedent).

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
   session. Portable, shareable, slowly changing. A CRDT-owned document in
   the kernel.
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
applied to devices.

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

- **`/etc/midi/devices/<name>`** — CRDT-owned per
  `docs/config-crdt-ownership.md`: kernel sole owner, edited via `kj` (a
  `kj midi` verb family), no host files. Optional embedded seeds for gear we
  ship knowledge of.
- **Identity**: the universal **MIDI Identity Request** (`F0 7E 7F 06 01 F7`)
  fingerprints nearly any device (manufacturer/model/firmware) — build first;
  tiny and vendor-neutral. Profiles carry the fingerprint plus ALSA
  client-name match strings, so a hotplugged port resolves to a profile and
  the ear's `played_by` gets a real name ("KeyStep Pro track 3", not "USB
  MIDI 2x2 port 1").

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
payloads already define (`docs/pcm.md`/`docs/clips.md`); a captured bulk dump
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
| **KeyStep Pro** | usual clock master (`midi.md` topology); first *pull* target | 4 sequencer tracks on configurable channels, drum mode, CC config — big enough to make SysEx pull worth building. Doesn't *pass through* large SysEx (thru-routing limitation) but answers its own config protocol (MCC does exactly this). |
| **1010music Bitbox** | **center of the system — the deep-dive device** | `midi.md` topology said "deliberately not on MIDI for now (recording path)"; direction updated — Bitbox MIDI IO is coming and we go deep: per-pad note/channel trigger maps, CC parameter control, clock. Likely bonus: 1010music stores presets as **XML on the microSD card** — a config-ingestion path that skips SysEx entirely (verify against Amy's unit's model/firmware). |
| **Polyend Poly 2** | Eurorack MIDI→CV bridge (loft) | Profile *must* carry its mode (first-fit poly / channel-per-voice / MPE) because the mode changes what a "channel" means. Config is on-device; profile is authored, not pulled. |
| **Moog Subharmonicon** | MIDI-out target; likely first *device context* | Semi-modular, 2 VCOs + 4 subs, polyrhythmic sequencers. Note→VCO routing and globals per its MIDI implementation chart (verify from manual when it lands on the bench). |
| **KeyLab 88 mkII** | lower priority | Controller: DAW maps, pads, faders/knobs, CV outs. Same Arturia SysEx family — KSP pull machinery should mostly transfer. |
| **DrumBrute** (original) | lower priority | Analog drum machine; pads send fixed-ish notes (MCC-configurable) — profile is mostly a **drum-note map**, the same shape TiMidity's GM drum profile needs. |
| **TiMidity (zorak)** | software device, already in use | GM soundfont synth, **drums on ch 10** — currently folk knowledge in a memory file; belongs in a profile. Cheap proof profiles aren't hardware-only. |

## Slices

1. **Identity + namespace + raw emit.** `/etc/midi/devices/` profile
   documents; `kj midi` list/show/edit; **`kj midi send`/`panic`** (raw
   control cues incl. fire-and-forget sysex bytes, kaish-scriptable day one);
   the **`exchange()` sink method** + `kj midi identify` (Identity-Request
   fingerprint); ALSA-name matching. Hand-author **Minibrute** and
   **TiMidity** profiles.
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
5. **Later:** Bitbox deep dive (XML preset ingestion?); KSP/DrumBrute pattern
   pull → score; manual→profile drafting as a repeatable flow; ear-verified
   MIDI-learn; MIDI-CI PE for future gear.

## Open questions

- **Bitbox specifics** — model/firmware on Amy's unit, XML preset format,
  whether MIDI config is per-preset or global. Blocks the deep dive, not
  slices 1–4.
- **Role vocabulary** — how rich is a profile "role"? (`notes` / `kick` /
  `voice[3]` / KSP `track2`?) Grow from the routing consumer; resist
  generality (the Cubase lesson).
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

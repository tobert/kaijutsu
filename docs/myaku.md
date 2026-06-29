# 脈 myaku — the pulse facility

> **Status:** high-level sketch. A dedicated design session will fill this in.
> Captured 2026-06-27, evolved 2026-06-28 (the facility shrank — see below).
> Decisions are *directions*, not commitments. Code is truth; this is where we're
> aiming. Companion: `docs/shared-state.md`, `docs/slash-v.md`.

## What it is

`myaku` (脈〔みゃく〕, "pulse") is a general **cron-like sampling facility** in the
kernel: it runs *probes* at a cadence so each one refreshes a small set of files
under `/run`. The first consumer is the HUD (CPU / memory / GPU sparklines when
running fullscreen), but the point is the *general facility* — any stat a user
wants to watch becomes a probe.

## The facility is tiny: cron + a death certificate

The big move (2026-06-28): **the facility does almost nothing.** It does *not*
capture probe output, own a ring buffer, or render a synthesized filesystem. A
probe is a kaish script that **manages its own world in `/run`** (a MemoryFS
mount) — it reads `/proc`/`/sys`, computes, and writes its own files. Consumers
read those files directly. The facility is just:

1. **Cadence** — run each due probe via kaish on its own clock.
2. **A death certificate** — record each run's `exit_code` + a stderr tail,
   *because only the parent sees a crash*. A probe that hits a kaish error, times
   out, or is OOM-killed writes nothing; a consumer staring at a stale `history`
   can't tell fresh-and-flat from three-minutes-dead. The probe can't report its
   own death — the scheduler can. That is the entire residual machinery.

Everything else is kaish writing files. See "Why so little" for what we deleted.

## Why it's its own thing (and not hyoushigi or KV)

**Beside hyoushigi, not on it.** Hyoushigi's `Timeline` is a *production-ahead*
engine — speculate → commit → squash, lead-time so content is ready *before* a
deadline, pumped pull-style, gated by the musician's arm/pause transport. Polling
is the negative of that: it **observes the present**, has no lead time, and a
*metrics* probe **must keep sampling while the transport is paused** (your CPU
graph can't freeze when the music stops). So myaku is **not built on the
`Timeline` engine**. It borrows two things: hyoushigi's `Tick` coordinate
(`kaijutsu-types/src/tick.rs`) and the **scheduler-loop pattern** from
`kaijutsu-server/src/beat.rs` (tokio `select!` over a timer + command channel +
min-heap). The shape is **one executor, two trigger front-ends** (see *Triggers*
below): the beat can drive a probe that *wants* to be on the beat, while a
wall-clock interval drives the probes that must ignore the transport.

**Why so little (what we deleted).** Earlier drafts gave the facility a
stdout-capture contract, a facility-owned in-memory ring, and a read-only
synthesized `/v` backend (à la `/v/ctx`). All ceremony: a probe *born as files*
has nothing to synthesize (unlike `/v/ctx`, which reflects the non-file block
store), and read-only is a nudge not a boundary in our shared-trust model — and
the data self-heals (clobber it, the probe rewrites it in ≤1s). So metrics are
plain MemoryFS, the facility owns no ring, and the synthesized backend is gone.
(`KvDocument`/`Kv` is also being deleted — see `docs/shared-state.md`.)

## Triggers and the three time coordinates

**The trigger and the timestamp are independent axes.** *When* a probe fires and
*what coordinate it stamps* are separate choices, so two trigger sources share the
one executor:

- **wall interval** (`#pulse every=1s`) → metrics, polling — ignores the transport;
- **hyoushigi beat** (`#pulse on=beat`) → music-synced actions — pauses with it.

The beat trigger is the existing `beat.rs` "playhead advanced" event; the wall
trigger is a plain interval timer. Both call the same run-kaish-and-write-`/run`
executor. (We lean *two front-ends* over *one scheduler taught two clocks* because
the beat machinery already exists and is arm/play-gated.)

The scheduler is the only party that knows the *intended* fire time and the only
one that can read the hyoushigi playhead (kaish can't reach it). So it **injects
the fire coordinates as `KJ_` env vars** (matching the existing
`KJ_PARENT_BLOCK_COUNT` kernel-injected convention) — the probe stamps from these,
so a row carries the *intended* fire time, not a `date` call a few ms later, and
every probe fired on the same tick shares one `KJ_EPOCH_NS` so their rows align
exactly.

| var | source | job | ordering key? |
|-----|--------|-----|---------------|
| `KJ_TICK` | hyoushigi playhead | musical position | **only within a beat** — frozen off-beat, by design |
| `KJ_PULSE` | per-probe monotonic counter | the reliable sequence | **yes, always** — never freezes, never jumps |
| `KJ_EPOCH_NS` | wall clock, shared per fire | human "when" + cross-probe alignment | **no** — NTP can step it back |

So a sparkline orders by `KJ_PULSE`, displays "when" via `KJ_EPOCH_NS`, and ignores
`KJ_TICK`; a beat-correlated row orders by `KJ_TICK`. `KJ_PULSE` is **per-probe**
(intra-probe order); `KJ_EPOCH_NS` is **shared** across probes fired the same tick
(cross-probe join). The frozen tick is a *feature* — off-beat there is no musical
time, so it must not be an ordering key there; that is exactly why `KJ_PULSE`
exists. A probe can still self-serve wall time via the `date` builtin
(`date +%s%N` → epoch ns; builtin-only, no external command), but the injected
coords are the idiom.

## How a probe works — own your world in `/run`

The scheduler runs each probe with a systemd/XDG-ish environment, then the probe
does its own bookkeeping and calls `pulse_emit` to lay out the sample:

```
# scheduler injects, per fire (KJ_ prefix = kernel-set, cf. KJ_PARENT_BLOCK_COUNT):
#   KJ_RUNTIME_DIR=/run/pulse/cpu   (systemd RUNTIME_DIRECTORY vibe; volatile)
#   KJ_NAME=cpu
#   KJ_CAP=120                      (window length, from the #pulse header)
#   KJ_TICK / KJ_PULSE / KJ_EPOCH_NS  (the fire coordinates — see Triggers)

# probe body (kaish): delta math is the probe's own job —
#   read /proc/stat, diff against $KJ_RUNTIME_DIR/.prev, rewrite .prev, then:
pulse_emit busy_pct=12.4 user_pct=8.1 system_pct=3.2 idle_pct=76.3
```

**`pulse_emit` is a kaish helper** (in the pulse rc library — editable, no Rust),
the uniformity contract so every probe stays dumb and the layout stays consistent.
From the `key=value` pairs plus the injected `KJ_` coords it writes **two files**:

1. **`now` — the snapshot.** The current sample in one read
   (`tick=… pulse=… epoch_ns=… busy_pct=… …`), no header to parse — the headline
   "what is it right now."
2. **`history` — the bounded TSV ring.** Append a row, trim to the last `$KJ_CAP`,
   write back. Centralizing this bounded read-trim-write is the point: a probe
   can't accidentally write an unbounded log.

…with a **fail-loud column check** (header written once; a probe that changes its
fields mid-flight crashes rather than silently corrupting the column a sparkline
reads by position). The time columns come from the injected coords, so `pulse_emit`
needs no clock of its own.

A pulse's directory, then:

```
/run/pulse/cpu/
├── now        # snapshot — current sample, one read     ← probe (pulse_emit)
├── history    # bounded TSV ring, for sparklines        ← probe (pulse_emit)
├── status     # death certificate: cadence, last_run, exit, stderr tail  ← scheduler
└── .prev      # probe's own delta scratch (hidden)
```

(No per-field scalar files — dropped 2026-06-28 as N redundant files duplicating
`now`; trivially re-added if a single-value `cat` need appears.) The roster
`/run/pulse/index` is a scheduler-maintained TSV — one read tells you every probe's
cadence and whether any are failing (`systemctl list-timers`-style).

Non-jobs (crisp boundary): `pulse_emit` does **not** do delta math (probe-specific,
kept in `$KJ_RUNTIME_DIR/.prev`), does **not** record health (the scheduler's
death certificate in `status`), and is **not** compiled.

First probes are **all kaish** reading `/proc`·`/sys` — no Rust until we genuinely
need the hot path. A future Rust probe uses the *same* `/run` file contract, so
the facility never changes.

## Consumers

- **HUD** (`kaijutsu-app`): the current `DockSparkline` sparklines are a
  **prototype** — expect them rewritten to read `/run/pulse/<x>/history`. Do not
  preserve their current form. `MemoryBackend` already bumps `generation` on each
  write, so the app's existing ~250ms poll gets a free hot/cold signal (re-`stat`
  to see a new sample; a dead probe stops bumping).
- **OODA contexts**: a context that watches system state ("how's the GPU doing?")
  reads `/run/pulse/...` at turn cadence — its Observe stage is an `rc` verb whose
  kaish `cat`s these files and assembles blocks (`docs/shared-state.md`).

## Open questions (deferred to the dedicated session)

- Probe **registration surface**: rc-style (CRDT-owned, restart-surviving probe
  definitions; `#pulse every=1s` / `#pulse on=beat` header) vs a `kj pulse` verb
  (ad-hoc) — rc as primary, kj for throwaway?
- **Beat-trigger scope**: does `#pulse on=beat` land in v1, or wall-only first and
  beat once the wall path is proven?
- `pulse_emit` **exact kaish surface** + whether a `pulse_delta`/`pulse_prev`
  companion is worth shipping for the common counter-diff case.
- **`now`/`status` format** — `key=value` line (greppable, symmetric with
  `pulse_emit`'s input) vs `json` (matches slash-v's per-object `json`).
- **Reductions** for OODA (e.g. "GPU 95% for 5 min, trending up"): a probe-written
  summary file, or the agent derives it from `history`? (thin-client tension.)

Settled this session (2026-06-28): `/run` is its **own** `MemoryBackend` mount
(`/scratch` likely retired); the death certificate is a `status` file **in the
probe dir**; scalars dropped; fire coords are flat `KJ_TICK`/`KJ_PULSE`/
`KJ_EPOCH_NS`.

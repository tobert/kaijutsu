# Playback: scheduled audio via peers

Status: design + architecture prep. Decisions dated 2026-06-10. This doc is
the line-up work — what must be true of the architecture before the feature
starts. The feature itself comes after other queued work.

## Model

The kernel never plays audio. Peers (the Bevy app, MCP servers like a
pawlsa-shaped sink) **advertise sound output capability** when they attach;
the kernel **schedules** playable objects onto the hyoushigi timeline; sinks
**pull objects from CAS ahead of time and fire them locally** when their
clock says the tick has arrived.

This is the same inversion as ABC engraving: the kernel owns content and
time, peers own presentation. It is multi-user-native — every attached
listener hears playback on their own output, and the kernel (a headless
systemd service) needs no audio stack.

Hyoushigi's speculative design is what makes distributed timing work: you
never say "play now" over a jittery connection. Committed cells materialize
ahead of the playhead (lead time, commit margin, safety factor are already
in `TickClock`); sinks get the object early and fire on local clock. Prior
art for the shared-tempo session clock: Ableton Link, MIDI clock.

## Decisions (2026-06-10)

- **Pull from CAS, not push.** A materialized beat block already carries
  `(tick, CAS hash, mime)` — scheduling *is* the block. Sinks subscribe to
  the context's block flow, prefetch by hash, hold until due. Because
  objects are hash-addressed and immutable, the fetch path can move out of
  band later (off the SSH connection) without changing semantics.
- **Transport state rides FlowBus.** New flow type (working name
  `TransportFlow`), not a capnp side-channel.
- **`kj transport pause` ≠ `stop` — distinct verbs, new semantics:**
  - **pause** = mute. No sound comes out; hyoushigi keeps going. The
    playhead advances, cells keep resolving and materializing, OODA stays
    armed. Forward-only philosophy holds: muting is a radio, not a tape —
    music that elapses while paused is *missed*, not deferred. Pause never
    touches the beat scheduler; it is sink-facing state on TransportFlow
    only.
  - **stop** = the clock stops. This is today's clock-freeze (currently the
    `Pause` verb → `BeatCommand::Pause`, `kj/transport.rs:43-48,137`) plus
    OODA disarm (today's `Stop`, `:49-54,138`). Event-counted tick semantics
    are unchanged: resume at +1, no rewind.

  This is a **verb remap**: `BeatCommand::Pause` (clock hold) becomes the
  backing for `stop`; the `pause` verb stops sending BeatCommands entirely.
  Sweep doc-comments (`kj/transport.rs:1-14`) and the composer rc loadout
  when remapping.

## Architecture prep (ordered)

1. **FlowBus health first.** The `ContextSwitched` published-into-the-void
   bug (see issues.md, Event Plumbing) is exactly the failure mode a new
   flow type must not repeat. Fix it and land the exhaustive
   `subject()`↔`TOPICS` test *before* adding `TransportFlow`.

2. **`TransportFlow`.** Carries per-context transport state: clock running
   /stopped, mute on/off, tempo (period), tick epoch (tick N ↔ walltime T
   pair for clock mapping). Emitted by the beat scheduler on every state
   change; capnp subscription surface so peers receive it (the planned
   "transport surface beyond kj" — app buttons/spacebar — subscribes to the
   same flow). Topics registered + covered by the exhaustive test.

3. **CAS read RPC.** Narrow, read-only fetch-by-hash for peers. The old
   generic blob API stubs are being deleted (issues.md, Cleanup); this is
   its scoped replacement — read-only, hash-addressed, cache-friendly.
   Design so the same contract can later be served out of band.

4. **Peer capability advertisement.** `PeerConfig` is nick-only today
   (`kernel/src/peers.rs:20-23`). Extend attach to carry declared
   capabilities — for sinks: accepted formats (`audio/midi`, PCM/WAV),
   optionally a latency estimate. Thread through the capnp `attachPeer`
   params and `PeerInfo`/listing. Keep it general (a "capabilities" bag,
   not an audio-specific field) — rendering surfaces may want the same
   slot.

5. **Fix peer re-attach on reconnect** (auto-memory
   `tech_debt_peer_reattach_on_reconnect`): the app doesn't re-attach as a
   peer after a kernel restart, so the registry stays empty until app
   restart. Tolerable for `invoke_peer` navigation; load-bearing for audio
   — a sink that silently vanishes after a kernel bounce is a silent
   fallback.

6. **Clock distribution model.** Define the sink-side clock: from
   TransportFlow it learns `(epoch_tick, epoch_walltime, period, running,
   muted)` and computes tick→local-walltime. Must handle the event-counted
   freeze/resume (+1, no rewind) and tempo changes (re-anchor epoch). Spec
   how a sink behaves on each transition, especially stop-while-objects-
   are-held (drop them — the playhead won't reach their ticks until
   resume, and resume re-anchors).

7. **Scheduling unit = materialized beat block.** Sinks use the existing
   filtered block subscription to watch for blocks with audio content
   types in armed contexts, then prefetch from CAS. May need a content-
   type filter dimension on `subscribe_blocks_filtered` if labels/kinds
   aren't sufficient. **Interim sink key (Chameleon batch 1, F2):** until
   `ContentType::Midi` lands (item 9 / playback slice 2), the derived MIDI is
   a `Role::Asset` block whose `parent_id` points at the ABC source
   (`ContentType::Abc`); the authoritative mime lives in the CAS sidecar.
   Sinks **must tolerate a source ABC block with no MIDI sibling** — a SIGKILL
   between the two inserts (the source is journaled, the sibling not) leaves
   one un-renderable phrase. Skip it and note it; never silently double or
   guess. Loud at crash time, never silently doubled (§11.8 of the F2 design).

8. **Resolver chain for dumb sinks.** Smart sinks (app with an embedded
   soundfont synth — oxisynth/rustysynth) take `audio/midi` directly. Dumb
   sinks (pawlsa-shaped: PCM/WAV only) need a kernel-side `midi→pcm` step —
   pure, idempotent, CAS-to-CAS. **Re-anchor (Chameleon batch 1, F2):** the
   `abc_to_midi` *resolver* no longer exists — notation-first commits made
   ABC→MIDI a barrier-side **`Deriver`**, not a timeline resolver, so there is
   no resolver to copy its shape from. The two candidate shapes for the
   midi→pcm step — a deferred PCM **cell keyed on the derived MIDI hash**, or a
   measured budget-excepted **deriver** — are recorded in issues.md (playback
   slice 3); pick one there before building. Whether the step runs at all is
   format negotiation against the attached sinks' advertised formats. Per the
   two-voices rule, design the sink interface against both concrete sinks (app
   + MCP) simultaneously.

9. **`ContentType::Midi` + renderer** (already in issues.md, Hyoushigi) —
   `audio/midi` projects to `Plain` today; the scrubbable timeline render
   becomes the visual twin of playback.

## Slices

1. **Clock + metronome.** Sink advertisement, TransportFlow, sink-side
   clock, and a local 拍子木 click on the beat in the app. No CAS, no MIDI,
   no synth. Clock-sync quality is audible — this slice is its own test
   harness.
2. **MIDI objects to a smart sink.** Beat blocks → CAS prefetch → app-side
   synth fires at tick. First end-to-end "the model wrote music and the
   room heard it."
3. **PCM resolver chain + MCP sink.** `midi→pcm` resolver, format
   negotiation, pawlsa-shaped peer plays WAV/PCM.
4. **Routing.** Default: all attached sinks of a context play (shared
   listening = shared context). Add `kj transport route <sink>` for
   targeted output (room speakers vs headphones).

## Out of scope (for now)

- **Streams.** Continuous audio (live synth, streamed TTS) has no natural
  tick coordinate; revisit with a dedicated channel after objects work.
  Note `generate_speech`-style clips are still *objects* and fit slice 2-3.
- **Latency compensation per sink.** Advertised latency estimates are
  collected (prep 4) but not consumed until jitter is measured in slice 1.
- **Seek/rewind.** The playhead is forward-only by design; revisiting the
  past is an export, not a seek.

## Open questions

- Does mute-during-OODA change what the composing turn sees? (It shouldn't
  — mute is presentation-only — but confirm the rc scripts don't key off
  playback state.)
- Sink auth: shared-trust kernel says capabilities are ergonomic nudges,
  so attach-time advertisement is enough; revisit if cross-kernel drift
  ever carries audio.
- Where does the soundfont live for the kernel-side `midi→pcm` resolver
  (CAS? config dir?) and the app synth — same asset or per-peer?

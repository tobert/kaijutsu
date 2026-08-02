# Arturia KeyStep Pro — device profile

> Draft seed, authored 2026-08-02 (`docs/midi-next.md` slice 1.2); seeded to
> `/etc/midi/devices/keystep-pro`. Convention: MIDI channels
> are **1–16** in every profile (the wire byte is channel−1). Match strings are
> **backend-neutral** — port display-name substrings and USB `vendor:product`
> IDs only, never ALSA client numbers (2026-08-02 amendment).
> Fields listed in `unverified` are best-knowledge drafts — confirm against
> Amy's unit on the bench and delete from the list as they're verified.
> **Observed live on moltar 2026-08-02**: USB `1c75:0218`, one MIDI port.

You are driving a **4-track hardware sequencer and keyboard controller that
makes no sound of its own**. Everything you send it goes somewhere else — to a
synth on one of its DIN/USB channels, or out its CV/gate jacks into the
eurorack. It is also **the rig's usual MIDI clock master** (`docs/midi.md`
topology), which makes it the one device on the roster whose transport state
other people's music depends on: think twice before sending it start/stop.

Its four sequencer tracks each live on their own MIDI channel (factory
default: tracks 1–4 on channels 1–4, but they are user-assignable and the
settings section below is a *draft*, not a reading). **Track 1 has a drum
mode** — 24 drum lanes on one channel, each lane a fixed note, the first eight
also firing the rear drum-gate jacks. Address it through the `drums` role and
its note map, never by guessing notes.

It does **not** reliably pass large SysEx through its MIDI thru path, so do
not treat it as a conduit for another device's patch dump. It *does* answer
the Arturia config protocol (`F0 00 20 6B …`) about itself — that is what
makes it the first `kj midi pull` target (slice 4; reference decoder:
[soyersoyer/sysex-controls](https://github.com/soyersoyer/sysex-controls),
whose KSP support covers device settings but **not** sequencer banks).

## Identity

```json
{
  "v": 1,
  "device": "keystep-pro",
  "display_name": "Arturia KeyStep Pro",
  "kind": "hardware-sequencer-controller",
  "match": {
    "usb_ids": ["1c75:0218"],
    "port_name_substrings": ["KeyStep Pro"],
    "ports": {
      "midi": {
        "port_name_substrings": ["KeyStep Pro MIDI 1", "KeyStep Pro"],
        "_note": "observed ALSA port display name is 'KeyStep Pro MIDI 1'; the ' MIDI 1' suffix is an ALSA-ism, so the bare 'KeyStep Pro' substring is the portable match and the long form is only a disambiguation hint"
      }
    },
    "identity_reply": null,
    "_note": "identity_reply: capture via `kj midi identify` once slice 1.5 lands (F0 7E 7F 06 01 F7)"
  },
  "observed": {
    "at": "2026-08-02",
    "host": "moltar",
    "backend": "alsa",
    "client_name": "KeyStep Pro",
    "port_count": 1
  },
  "unverified": ["match.ports.midi.port_name_substrings[0] under CoreMIDI"]
}
```

## Settings (device is ground truth — a pull overwrites this section)

Every value here is an **authored draft of factory defaults**, not a reading
from Amy's unit — the KSP is the first device where that distinction bites,
because its track channels are exactly the thing a previous session may have
moved in MIDI Control Center. Treat these as a hypothesis until
`kj midi pull keystep-pro` lands (slice 4) and rewrites the section.

```json
{
  "v": 1,
  "source": "authored-draft",
  "pulled_at": null,
  "track_channels": {
    "track1": 1,
    "track2": 2,
    "track3": 3,
    "track4": 4
  },
  "drum_track_channel": "@settings.track_channels.track1",
  "_note_drum_track_channel": "the drum track may be independently assignable in MCC; if it is, this becomes a plain int and ch 10 is the useful value when the target is a GM drum map",
  "clock_source": "internal",
  "clock_source_options": ["internal", "usb", "midi", "sync"],
  "clock_out_ppqn": 24,
  "sync_out_format": "1pulse-per-step",
  "transport_sends": { "start": true, "stop": true, "continue": true, "song_position": null },
  "midi_thru": false,
  "sustain_pedal_polarity": "auto",
  "velocity_curve": "linear",
  "aftertouch_curve": "linear",
  "touch_strip_mode": "pitch-bend",
  "knob_catch_mode": "hook",
  "tempo_bpm": 120,
  "unverified": [
    "track_channels", "drum_track_channel", "clock_source",
    "clock_out_ppqn", "sync_out_format", "transport_sends.song_position",
    "midi_thru", "sustain_pedal_polarity", "velocity_curve",
    "aftertouch_curve", "touch_strip_mode", "knob_catch_mode", "tempo_bpm"
  ]
}
```

## Capabilities (this document is ground truth — a contradicting pull flags, never overwrites)

```json
{
  "v": 1,
  "polyphony": "controller",
  "_note_polyphony": "this device makes no sound; polyphony is a property of whatever each track drives",
  "sound_source": false,
  "patch_memory": true,
  "program_change": true,
  "keys": { "count": 37, "type": "slim", "note_range": [36, 72], "octave_shift": true },
  "controls": {
    "sequencer_tracks": 4,
    "drum_lanes": 24,
    "touch_strip": true,
    "encoders": "per-track sequence parameters (swing, randomness, probability, gate length, time division)",
    "_note_encoders": "these are sequencer parameters on the panel, not a generic CC-knob controller surface; whether any of them transmit CC (and on what number) is an MCC/pull question, not a guess"
  },
  "receives": {
    "notes": true,
    "velocity": true,
    "clock": true,
    "transport": true,
    "program_change": true,
    "cc": {},
    "_note": "MIDI-in feeds track recording and transpose; whether incoming notes on a track channel drive that track's CV/gate jacks (i.e. KSP as a 4-channel MIDI-to-CV bridge for the eurorack) is UNVERIFIED and worth a bench test"
  },
  "sends": {
    "notes": true,
    "velocity": true,
    "channel_pressure": true,
    "pitch_bend": true,
    "clock": true,
    "transport": true,
    "program_change": true,
    "cc": { "1": "modulation (touch strip, when strip mode = mod)" }
  },
  "clock": {
    "master_capable": true,
    "slave_capable": true,
    "ppqn": 24,
    "usual_role": "master",
    "_note": "the rig's usual clock master (docs/midi.md topology). Doctrine: we MODEL this clock, never chase it — observe, fit tempo/phase/drift, regenerate locally"
  },
  "sysex": {
    "own_protocol": "arturia",
    "arturia_prefix": "F0 00 20 6B",
    "passthru": "small-messages-only",
    "_note_passthru": "does not reliably pass large SysEx through its thru path — never route another device's patch/bulk dump through this unit",
    "pull_supported_by_reference": "device settings yes, sequencer banks no (soyersoyer/sysex-controls)"
  },
  "ports": {
    "din_in": true,
    "din_out": true,
    "usb": true,
    "sync_in": true,
    "sync_out": true,
    "cv_gate_out": 4,
    "drum_gate_out": 8,
    "sustain_pedal_in": true,
    "_note_din_count": "DIN jack count (one IN / one OUT assumed) is unverified"
  },
  "din_children": {
    "_doc": "authored claims about gear hanging off this device's DIN jacks. DIN gear is invisible to enumeration: presence for a child is INFERRED (this host port present + this claim), never observed. Verify a child with an identity exchange over the host port before trusting it.",
    "_example": {
      "jack": "din_in",
      "device": "foot-pedal",
      "presence": "inferred",
      "verified_by": null
    },
    "din_in": [],
    "din_out": []
  },
  "roles": {
    "track1": { "port": "midi", "channel": "@settings.track_channels.track1", "direction": "source" },
    "track2": { "port": "midi", "channel": "@settings.track_channels.track2", "direction": "source" },
    "track3": { "port": "midi", "channel": "@settings.track_channels.track3", "direction": "source" },
    "track4": { "port": "midi", "channel": "@settings.track_channels.track4", "direction": "source" },
    "drums":  { "port": "midi", "channel": "@settings.drum_track_channel", "note_map": "ksp_drum_default", "direction": "source" },
    "clock":  { "port": "midi", "realtime": true, "direction": "source" },
    "_note_direction": "'direction' is a first stab at the open role-vocabulary question (docs/midi-next.md): a KSP track is a SOURCE the ear maps played_by through, not a destination a kaijutsu track renders into — unless the CV-bridge question above answers yes, in which case these become bidirectional"
  },
  "note_maps": {
    "ksp_drum_default": {
      "_note": "24 drum lanes on the drum track, believed to be 24 consecutive notes from 36 (C1); lanes 1-8 also fire the rear drum-gate jacks. Per-lane notes are MCC-editable, so this whole map is a draft — verify lane 1's note and the contiguity before relying on it",
      "36": "lane 1 (gate out 1)", "37": "lane 2 (gate out 2)",
      "38": "lane 3 (gate out 3)", "39": "lane 4 (gate out 4)",
      "40": "lane 5 (gate out 5)", "41": "lane 6 (gate out 6)",
      "42": "lane 7 (gate out 7)", "43": "lane 8 (gate out 8)",
      "44": "lane 9", "45": "lane 10", "46": "lane 11", "47": "lane 12",
      "48": "lane 13", "49": "lane 14", "50": "lane 15", "51": "lane 16",
      "52": "lane 17", "53": "lane 18", "54": "lane 19", "55": "lane 20",
      "56": "lane 21", "57": "lane 22", "58": "lane 23", "59": "lane 24"
    }
  },
  "relative_safe": [],
  "unverified": [
    "keys.count", "keys.note_range", "controls.drum_lanes",
    "receives.notes", "receives.program_change", "sends.channel_pressure",
    "sends.pitch_bend", "sends.cc.1", "clock.ppqn", "sysex.passthru",
    "ports.din_in", "ports.din_out", "ports.sync_in", "ports.sync_out",
    "ports.cv_gate_out", "ports.drum_gate_out", "ports.sustain_pedal_in",
    "note_maps.ksp_drum_default"
  ]
}
```

## Playing notes (the skill body)

- **Do not touch transport casually.** This is the clock master; a stray start
  or stop is felt by every device downstream and by the kernel's tempo model.
  If a human asks you to stop *your* part, stop your part — do not stop the
  KSP. Clock is modelled, never chased (`docs/midi.md`, "The one timebase"):
  we observe its pulses and regenerate a local grid, so a missed beat stays
  missed and is never replayed.
- **Channels are a hypothesis until pulled.** Every track channel above is a
  factory-default guess. Always route through a role (`keystep-pro.track2`),
  never a raw channel int, so that a `kj midi pull` re-pointing track 2 to ch 7
  re-routes bindings for free. If something plays on the wrong synth, suspect
  the channel map before suspecting the note data.
- **Drums are notes, not gates, from our side.** The 24 lanes are 24 fixed
  notes on one channel; the rear gate jacks are a side effect of lanes 1–8.
  Use the `drums` role and `ksp_drum_default`, and remember the map is
  per-lane editable in MCC — a lane that fires the wrong drum means the map is
  stale, not that you sent the wrong lane.
- **`relative_safe` is empty on purpose.** Nothing on this panel echoes its
  position back, and the knobs are sequencer parameters rather than a CC
  surface, so `/run/midi/keystep-pro` only ever holds what we *sent* — a hope,
  not a fact. Human hands on the tempo knob are an unmodeled writer. Once
  slice 4's settings pull works, `pulled` provenance can make *settings*
  (channels, curves, clock source) relative-safe; panel positions never will
  be.
- **Never route bulk SysEx through it.** Large messages do not survive its
  thru path. Its own Arturia protocol is a different matter and is the
  intended `kj midi pull` route — and note that the reference decoder does not
  do sequencer banks, so "pull the patterns into a score" is a later item, not
  a thing you can do.
- **The sustain pedal input is the device's, not ours.** A pedal in that jack
  becomes CC 64 in the KSP's outgoing stream on the active track's channel; we
  see it as capture, we cannot set it.
- **It enumerates as an ALSA card** (USB-MIDI is a USB-audio-class subclass,
  so `/proc/asound/cards` lists it) but it is a MIDI device — whether any PCM
  endpoint exists at all is unverified and irrelevant to routing. Do not let
  an audio-side card index leak into a match string; matching is by name
  substring and USB ID only.

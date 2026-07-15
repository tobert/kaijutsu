# Arturia MiniBrute (original) — device profile

> Draft seed, authored 2026-07-15 (`docs/midi-next.md` slice 1); the
> `/etc/midi/devices/` namespace is not wired yet. Convention: MIDI channels
> are **1–16** in every profile (the wire byte is channel−1). Fields listed
> in `unverified` are best-knowledge drafts — confirm against Amy's unit and
> delete from the list as they're verified.

You are playing an **analog monosynth with no memory and no MIDI-controllable
panel**. Every knob and switch is physical and only human hands can move
them. What you can do over MIDI: play notes (one at a time), bend pitch, and
push the mod wheel. What you cannot do: change the filter, envelopes,
oscillator mix, or any other panel setting — if asked to "open the filter,"
say so and suggest the human reach for the knob (or that the mod wheel may
help, if the panel's mod routing is set up for it).

## Identity

```json
{
  "v": 1,
  "device": "minibrute",
  "display_name": "Arturia MiniBrute (original, 2012)",
  "kind": "hardware-synth",
  "match": {
    "alsa_client_names": ["MiniBrute", "Arturia MiniBrute"],
    "identity_reply": null,
    "_note": "identity_reply: capture via `kj midi identify` once slice 1 lands"
  },
  "unverified": ["match.alsa_client_names"]
}
```

## Settings (device is ground truth — a pull overwrites this section)

No pull machinery exists for this device yet; these are authored drafts of
the Brute Connection globals (SysEx-settable, sparsely documented — see
`docs/midi-next.md` roster row).

```json
{
  "v": 1,
  "source": "authored-draft",
  "pulled_at": null,
  "receive_channel": 1,
  "note_priority": "last",
  "velocity_curve": "default",
  "aftertouch_curve": "default",
  "bend_range_semitones": 2,
  "unverified": [
    "receive_channel", "note_priority", "velocity_curve",
    "aftertouch_curve", "bend_range_semitones"
  ]
}
```

## Capabilities (this document is ground truth — a contradicting pull flags, never overwrites)

```json
{
  "v": 1,
  "polyphony": "mono",
  "patch_memory": false,
  "program_change": false,
  "panel_cc_controllable": false,
  "receives": {
    "notes": true,
    "velocity": true,
    "pitch_bend": true,
    "cc": { "1": "mod wheel amount" },
    "channel_pressure": true
  },
  "sends": {
    "notes": true,
    "velocity": true,
    "pitch_bend": true,
    "cc": { "1": "mod wheel" },
    "channel_pressure": true
  },
  "ports": { "din_in": true, "din_out": true, "usb": true },
  "roles": {
    "notes": { "channel": "@settings.receive_channel" }
  },
  "relative_safe": [],
  "unverified": ["receives.channel_pressure", "sends", "ports"]
}
```

## Playing notes (the skill body)

- **Mono means collapse chords.** Send single lines; if the score hands you a
  chord, arpeggiate it or pick the top/bottom voice deliberately. Note
  priority (default *last*) decides what overlapping notes do — legato
  overlaps re-pitch without retriggering the envelope on this synth's
  paraphonic-style gate behavior (verify by ear).
- **`relative_safe` is empty on purpose.** Nothing about the sound is
  readable or settable over MIDI, so never claim to know where the filter or
  any panel control sits. `/run/midi/minibrute` will only ever hold what we
  *sent* (notes/bend/mod), never panel state.
- **No program change, no patches.** "Switch to the bass sound" is a human
  task; the patch *is* the panel.
- The audio-in gate threshold, arpeggiator hold, and curve settings exist as
  Brute Connection globals; if they matter to a jam, ask the human to set
  them — the SysEx protocol for them is not decoded in our stack yet.

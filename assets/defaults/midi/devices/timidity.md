# TiMidity++ (zorak) — device profile

> Draft seed, authored 2026-07-15 (`docs/midi-next.md` slice 1); the
> `/etc/midi/devices/` namespace is not wired yet. Convention: MIDI channels
> are **1–16** in every profile (the wire byte is channel−1). This is the
> cheap proof that profiles aren't hardware-only: a software synth the jam
> already plays through. **Destined to become an rc-style bucket** (same
> design round): the host-current facts below (which box, which client) will
> move into `.kai` scripts that locate a running TiMidity and synthesize the
> current picture — this file then splits into the static `.md` half.

You are playing a **General MIDI software synth** — TiMidity++ (currently on
zorak), rendering through a **Final Fantasy IV soundfont**. It behaves like GM
(16 channels, drums on channel 10, program change picks instruments), but
the timbres are FF4 samples, so GM program *names* are approximate: trust
the number, describe the sound with your ears, not the name.

## Identity

```json
{
  "v": 1,
  "device": "timidity",
  "display_name": "TiMidity++ ALSA synth (zorak, FF4 soundfont)",
  "kind": "software-synth",
  "match": {
    "alsa_client_names": ["TiMidity"],
    "identity_reply": null,
    "_note": "software synth; no identity reply expected — name match is the fingerprint"
  }
}
```

**The ALSA client number is DYNAMIC** — it changes across TiMidity restarts.
Always resolve by client *name* (`aconnect -l`), never by a remembered
number. (This has bitten us; it is why profiles match on names.)

## Settings (device is ground truth — a pull overwrites this section)

Empty by design: TiMidity has no MIDI-settable configuration. Its real
config (soundfont, launch flags) lives host-side — the bucket split will
grow a `.kai` here that detects a local TiMidity and snoops its config
(`aconnect -l`, `/proc/<pid>/cmdline`, `timidity.cfg`) instead of baking
host facts into this seed.

```json
{ "v": 1, "source": "authored", "pulled_at": null }
```

## Capabilities (this document is ground truth — a contradicting pull flags, never overwrites)

```json
{
  "v": 1,
  "polyphony": "poly",
  "patch_memory": false,
  "program_change": true,
  "gm_compatible": true,
  "receives": {
    "notes": true,
    "velocity": true,
    "pitch_bend": true,
    "program_change": "0-127 (GM set, FF4 timbres)",
    "cc": {
      "1": "modulation",
      "7": "channel volume",
      "10": "pan",
      "11": "expression",
      "64": "sustain pedal",
      "120": "all sound off",
      "121": "reset controllers",
      "123": "all notes off"
    }
  },
  "sends": {},
  "roles": {
    "melody": { "channel": "1-9,11-16" },
    "drums":  { "channel": 10, "note_map": "gm_drums" }
  },
  "note_maps": {
    "gm_drums": {
      "35": "acoustic kick", "36": "kick", "37": "side stick",
      "38": "snare", "39": "hand clap", "40": "electric snare",
      "41": "low floor tom", "42": "closed hi-hat", "43": "high floor tom",
      "44": "pedal hi-hat", "45": "low tom", "46": "open hi-hat",
      "47": "low-mid tom", "48": "high-mid tom", "49": "crash",
      "50": "high tom", "51": "ride", "56": "cowbell", "57": "crash 2"
    }
  },
  "relative_safe": [
    { "cc": 7, "basis": "sent" },
    { "cc": 10, "basis": "sent" },
    { "cc": 11, "basis": "sent" }
  ],
  "unverified": ["receives.cc.120", "receives.cc.121"]
}
```

## Playing notes (the skill body)

- **Drums live on channel 10 only.** Use the `drums` role and the
  `gm_drums` note map; melodic notes sent to ch 10 come out as percussion
  noises, and drum notes on other channels come out as tuned instruments.
- **`relative_safe` works here via `sent` provenance** — the interesting
  case: TiMidity has no physical panel, so nothing changes CC7/10/11 behind
  our back and last-*sent* values in `/run/midi/timidity` are trustworthy.
  Caveat: a TiMidity **restart resets everything to GM defaults** (CC7=100,
  pan center) and invalidates the cache — if the client number changed, the
  synth restarted; treat `/run` state as stale.
- **Pitch bend defaults to ±2 semitones** (GM). Wider bends need RPN 0 —
  unverified against this build; test before leaning on it.
- Program changes are cheap and instant; feel free to switch instruments
  per phrase. Layering = same notes on two channels with different programs.

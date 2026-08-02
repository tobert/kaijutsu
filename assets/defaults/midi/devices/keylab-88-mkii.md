# Arturia KeyLab mkII 88 — device profile

> Draft seed, authored 2026-08-02 (`docs/midi-next.md` slice 1.2); seeded to
> `/etc/midi/devices/keylab-88-mkii`. Convention: MIDI channels
> are **1–16** in every profile (the wire byte is channel−1). Match strings are
> **backend-neutral** — port display-name substrings and USB `vendor:product`
> IDs only, never ALSA client numbers (2026-08-02 amendment).
> Fields listed in `unverified` are best-knowledge drafts — confirm against
> Amy's unit on the bench and delete from the list as they're verified.
> **Observed live on moltar 2026-08-02**: USB `1c75:02cb`, **two** ports.

You are working an **88-key weighted master controller that makes no sound**.
It is a source, not a destination: it sends what human hands do — keys, 9
faders, 9 encoders, 16 pads, a transport section — and it drives four CV jacks
into the eurorack. Sending it notes accomplishes nothing musical; its faders
are not motorized, so you cannot move a control from here either.

**It presents two ports and they are not interchangeable.** This is the first
device on the roster where picking the wrong port is silently wrong rather
than loudly broken:

- **`KeyLab mkII 88 MIDI`** — the musical port. Keys, pads, faders, encoders,
  wheels and pedals on the user channel. This is the port the ear listens to
  and the one every `played_by` mapping should resolve through.
- **`KeyLab mkII 88 DAW`** — a control-surface protocol port (Mackie
  Control / HUI family, selectable in MCC). Its notes are *buttons*, its
  pitch-bend messages are *fader positions*, and its SysEx is display text.
  Nothing on this port is music. Never send score notes here, and never let
  capture from this port reach a score track — it will look like a
  performance and is not one.

Same Arturia SysEx config family as the KeyStep Pro (`F0 00 20 6B …`), so the
slice-4 pull machinery should mostly transfer; the reference decoder is
[soyersoyer/sysex-controls](https://github.com/soyersoyer/sysex-controls).

## Identity

```json
{
  "v": 1,
  "device": "keylab-88-mkii",
  "display_name": "Arturia KeyLab mkII 88",
  "kind": "hardware-controller",
  "match": {
    "usb_ids": ["1c75:02cb"],
    "port_name_substrings": ["KeyLab mkII 88"],
    "ports": {
      "midi": {
        "port_name_substrings": ["KeyLab mkII 88 MIDI"],
        "role": "musical"
      },
      "daw": {
        "port_name_substrings": ["KeyLab mkII 88 DAW"],
        "role": "control-surface"
      }
    },
    "identity_reply": {
      "raw": "f07e7f060200206b0200056854010301f7",
      "manufacturer": "00206b",
      "family": 2,
      "model": 13317,
      "version": "54010301",
      "pulled_at": "2026-08-02",
      "_note": "captured live by `kj midi identify keylab-88-mkii` on moltar (first-matched-port = the MIDI port answered). Version bytes are the unit's firmware at capture time, not a match criterion"
    },
    "_note": "both ports live behind one USB ID, so the USB match identifies the DEVICE and the name substrings disambiguate the PORTS — a two-port device is why match strings need both levels"
  },
  "observed": {
    "at": "2026-08-02",
    "host": "moltar",
    "backend": "alsa",
    "client_name": "KeyLab mkII 88",
    "port_count": 2,
    "port_names": ["KeyLab mkII 88 MIDI", "KeyLab mkII 88 DAW"]
  },
  "unverified": ["match.ports.*.port_name_substrings under CoreMIDI"]
}
```

## Settings (device is ground truth — a pull overwrites this section)

The CC assignments on this device are **per-preset and MCC-editable**, which
means guessing them is worse than admitting we do not know: a wrong CC number
mislabels every incoming control the ear captures. They are left `null`
deliberately. Learn them with `kj midi pull keylab-88-mkii` (slice 4) or by
ear-verified MIDI-learn ("wiggle fader 3" → watch what arrives), and let the
pull overwrite this section.

```json
{
  "v": 1,
  "source": "authored-draft",
  "pulled_at": null,
  "active_mode": "user",
  "mode_options": ["analog-lab", "daw", "user"],
  "user_channel": 1,
  "faders": { "count": 9, "cc": null, "channel": "@settings.user_channel" },
  "encoders": { "count": 9, "cc": null, "channel": "@settings.user_channel", "mode": "absolute" },
  "pads": { "count": 16, "channel": 10, "notes": [36, 51], "mode": "notes" },
  "wheels": { "pitch_bend": true, "mod_cc": 1 },
  "pedals": { "sustain_cc": 64, "expression_cc": 11, "aux1_cc": null, "aux2_cc": null, "aux3_cc": null },
  "daw_protocol": "mackie-control",
  "daw_protocol_options": ["mackie-control", "hui", "logic", "other-mcc-selectable"],
  "velocity_curve": "linear",
  "pad_velocity_curve": "linear",
  "aftertouch_curve": "linear",
  "_hint_cc_blocks": "the factory User preset assigns contiguous CC blocks to the 9 faders and the 9 encoders; the block start is exactly the thing to pull rather than assume",
  "unverified": [
    "active_mode", "user_channel", "faders.cc", "encoders.cc",
    "encoders.mode", "pads.channel", "pads.notes", "pads.mode",
    "wheels.mod_cc", "pedals.expression_cc", "pedals.aux1_cc",
    "pedals.aux2_cc", "pedals.aux3_cc", "daw_protocol",
    "velocity_curve", "pad_velocity_curve", "aftertouch_curve"
  ]
}
```

## Capabilities (this document is ground truth — a contradicting pull flags, never overwrites)

```json
{
  "v": 1,
  "polyphony": "controller",
  "_note_polyphony": "this device makes no sound; polyphony is a property of whatever it drives",
  "sound_source": false,
  "patch_memory": true,
  "program_change": true,
  "keys": {
    "count": 88,
    "type": "hammer-action weighted",
    "note_range": [21, 108],
    "velocity": true,
    "aftertouch": "channel",
    "octave_shift": true
  },
  "controls": {
    "faders": 9,
    "encoders": 9,
    "pads": 16,
    "pads_rgb": true,
    "pads_velocity": true,
    "pads_aftertouch": true,
    "transport_section": true,
    "transport_buttons": ["rewind", "fast-forward", "stop", "play/pause", "record", "loop"],
    "pitch_wheel": true,
    "mod_wheel": true,
    "motorized": false,
    "_note_motorized": "faders and encoders are NOT motorized: we can never set a control position from our side, only observe where a human put it"
  },
  "receives": {
    "notes": false,
    "clock": true,
    "transport": true,
    "cc": {},
    "sysex": "arturia config + DAW-port display/LED feedback",
    "_note": "on the MIDI port this device is effectively source-only. The DAW port receives a great deal (LED states, display text, fader echo) but only in its control-surface protocol, which we do not speak"
  },
  "sends": {
    "notes": true,
    "velocity": true,
    "channel_pressure": true,
    "pitch_bend": true,
    "program_change": true,
    "clock": false,
    "transport": "as DAW-map messages on the daw port, or as CC/MMC from the midi port depending on preset",
    "cc": { "1": "mod wheel", "64": "sustain pedal", "11": "expression pedal" }
  },
  "clock": {
    "master_capable": false,
    "slave_capable": true,
    "usual_role": "none",
    "_note": "the KeyStep Pro is the rig's usual clock master (docs/midi.md topology); this device has no reason to be one"
  },
  "sysex": {
    "own_protocol": "arturia",
    "arturia_prefix": "F0 00 20 6B",
    "passthru": "unverified",
    "_note_passthru": "the KSP's thru path chokes on large SysEx; assume the same here until tested"
  },
  "ports": {
    "usb": true,
    "usb_endpoints": ["midi", "daw"],
    "din_in": true,
    "din_out": true,
    "cv_out": ["pitch", "gate", "mod1", "mod2"],
    "sustain_pedal_in": true,
    "expression_pedal_in": true,
    "aux_pedal_in": 3
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
    "keys":      { "port": "midi", "channel": "@settings.user_channel", "direction": "source" },
    "pads":      { "port": "midi", "channel": "@settings.pads.channel", "note_map": "pads_default", "direction": "source" },
    "faders":    { "port": "midi", "channel": "@settings.user_channel", "direction": "source" },
    "encoders":  { "port": "midi", "channel": "@settings.user_channel", "direction": "source" },
    "daw":       { "port": "daw", "direction": "bidirectional", "protocol": "@settings.daw_protocol", "musical": false },
    "_note_direction": "'direction' is a first stab at the open role-vocabulary question (docs/midi-next.md). Every musical role here is a SOURCE — the ear resolves played_by through them; none is a destination a kaijutsu track renders into. The 'daw' role is bidirectional and explicitly NOT musical: it exists so a binding can name the port in order to AVOID it"
  },
  "note_maps": {
    "pads_default": {
      "_note": "16 pads believed to default to notes 36-51 on ch 10; per-pad note and channel are MCC-editable, so treat the whole map as a draft until pulled or ear-verified",
      "36": "pad 1", "37": "pad 2", "38": "pad 3", "39": "pad 4",
      "40": "pad 5", "41": "pad 6", "42": "pad 7", "43": "pad 8",
      "44": "pad 9", "45": "pad 10", "46": "pad 11", "47": "pad 12",
      "48": "pad 13", "49": "pad 14", "50": "pad 15", "51": "pad 16"
    }
  },
  "relative_safe": [],
  "unverified": [
    "keys.aftertouch", "controls.pads_aftertouch", "controls.transport_buttons",
    "receives.clock", "receives.transport", "sends.channel_pressure",
    "sends.program_change", "sends.transport", "sends.cc.11",
    "sysex.passthru", "ports.din_in", "ports.din_out", "ports.cv_out",
    "ports.aux_pedal_in", "note_maps.pads_default"
  ]
}
```

## Playing notes (the skill body)

- **Pick the port before you pick the channel.** `keylab-88-mkii.keys` and
  friends resolve to the `midi` port; the `daw` role resolves to the `daw`
  port and exists mostly so a binding can name it in order to exclude it. If
  capture from this device looks like someone is playing atonal single notes
  in tight clusters, you are listening to the DAW port's buttons.
- **This device is a source; relative commands are meaningless here.**
  `relative_safe` is empty and will stay empty. Nothing is motorized, so "turn
  fader 3 down 25%" cannot be executed — the honest answer is to ask the human
  to move it, or to apply the 25% to whatever the fader is *bound to*
  downstream. The ear can give us `observed` provenance for where a control
  currently sits (a controller that echoes its own moves is the good case in
  `/run/midi`), but observation is not control.
- **CC numbers are unknown on purpose.** They are `null` in settings because
  they are per-preset. Do not fill them in from memory of "typical Arturia
  defaults" — verify by MIDI-learn (ask for a wiggle, watch capture) or by
  pull, then let the pull own the section. A confidently wrong CC map is the
  failure mode that makes DAW device support rot.
- **The four CV jacks follow the keyboard, not a track.** Pitch/Gate/Mod1/Mod2
  are driven by the panel and preset, not by anything we send over MIDI —
  another reason bindings treat this device as a source. Whether the CV jacks
  can be driven from incoming MIDI is unverified and worth the same bench test
  as the KSP's.
- **Assume the SysEx thru limitation.** The KSP does not pass large SysEx;
  nothing suggests this one is better. Its own Arturia config protocol is the
  supported route, and it is the second `kj midi pull` target after the KSP.
- **It enumerates as an ALSA card** (USB-MIDI is a USB-audio-class subclass,
  so `/proc/asound/cards` lists it) but MIDI is the subject here. A card index
  is a backend-specific handle and must never appear in a match string —
  matching is by name substring and USB ID only.

# Roland JD-Xi — device profile

## Identity

```json
{
  "v": 1,
  "device": "jd-xi",
  "display_name": "Roland JD-Xi",
  "kind": "hardware-synth",
  "match": {
    "usb_ids": [],
    "port_name_substrings": ["JD-Xi"]
  }
}
```

Match the observed client name `JD-Xi` and port name `JD-Xi MIDI 1`.
Names identify a model, not a unique physical unit. USB identity and firmware
are not yet verified. Host and endpoint address belong in live inventory,
not this profile. Channel assignments, musical roles and programming are
separate configuration; none are asserted here.

## Pulled data

Keep raw replies and dumps as separate artifacts with their source node,
endpoint generation, request bytes and observation time. A saved reply is an
observation, not a command to restore those settings when the device moves.
`kj midi identify jd-xi --json` requests identity without programming the synth.
Patch and program dumps require verified device-specific requests.


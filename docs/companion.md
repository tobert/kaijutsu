# The companion

A `companion` is a character, with its own rc directory, for a long-lived personal agent. One person
talks to it. While the person is away, a clock wakes a short-lived copy of it
once an hour. That copy looks at the world, decides whether anything matters,
and stops. The rc lives in `assets/defaults/rc/companion/`. The directory is
standalone: each script is a copy of what it uses, so copying the directory to
`rc/<other-name>/` makes an experiment you can change freely.

## The pieces

- **The main context** is the one the person talks to. It carries the
  companion stance (`create/S00-stance.md`) and the handoff log of its
  character. Its tools are the writable shell and the input surfaces
  (`create/S10-binding.kai`). The person is present when it runs, so the gate
  lets the `shell_write` statement through; `kj handoff note` is allowed for
  every character, and any other command no `gate.toml` key covers asks the person.
- **A check-in** is a fork of the main context, made with `--preset spawn`, so
  it starts empty. Its own `fork/` rc gives it the check-in stance
  (`fork/S20-stance.md`) and the recent handoff notes. Its loadout is cleared
  and set to the read-only shell alone (`fork/S25-binding.kai`): a check-in
  observes and cannot change anything. It leaves the beat track first
  (`fork/S10-detach.kai`), because a fork copies its parent's attachment and a
  child that kept it would wake and fork a check-in of its own.
- **A check-in's report** is its last message, which begins with `QUIET:` or
  `FINDING:`. The next tick reads it (see "One tick").
- **The record** is the check-in context itself. The tick archives it, which
  keeps every block, so the main context and the person can read what it
  looked at: `kj context list`, then `kj block list -c ID`.
- **The handoff log** (`kj handoff note`, `kj handoff tail`) is the memory the
  main context keeps across rotations. Only the character writes it, so only
  the main seat does. The `submit/` rc shows the last eight notes behind the
  person's message, for that turn only.
- **A finding** is not a handoff note. A note is authored by a character, and
  the tick runs as the clock, so the tick puts each finding in the main
  context's log as a notification headed `=== Check-in NAME reported ===`. The
  seat reads it on its next turn and folds what matters into a handoff note,
  at the latest during the wrap-up turn. A finding it never wrote down is
  forgotten at the next rotation.

## The clock

The companion rides the beat track described in `docs/hyoushigi.md` and
`docs/tracks.md`. A track arms stopped, so nothing runs until the person
plays it. Pausing it is how the person says "I am away for a while": a paused
track costs nothing and keeps its place.

One hour at 60 BPM is 3600 beats, so:

```
kj transport attach --track kaiten --wakeup 3600
kj transport tempo 60 --track kaiten
kj transport play --track kaiten
```

The beat counts only while the track is playing, so a pause does not use up
the hour. The first tick comes 3600 beats after `play`. After a kernel
restart the track is stopped and unattached: attach and play again.

## One tick

`tick/S10-checkin.kai` first closes out the previous check-in, on every tick.
It reads the check-in's last model message whole (`kj block list --tail 1`,
then `kj block read`; `kj wait` shows only a block's first line). A message with
`FINDING:` anywhere goes into the main context's log as a notification, one with
only `QUIET:` leaves nothing, and a check-in that reported neither word, or
nothing, is reported the same way as a finding about itself. The stance asks the
check-in to end with its verdict. Then it archives the check-ins, which keeps every block.
Archiving first means two check-ins are never live at once, and a stuck one is
closed at the next tick.

Then it does one of three things, in this order:

1. If `WRAPUP_AT` is set on the main context, a wrap-up was asked for on an
   earlier tick. It clears the marker and rotates the context.
2. If the last model call filled `ROTATE_AT_TOKENS` or more (160000 by
   default, about 60 percent of a 262144-token window), it drives a wrap-up
   turn (`tick/S10-wrapup.md`), sets `WRAPUP_AT`, and runs no check-in.
3. Otherwise it forks a new check-in with the seed text in
   `tick/S10-checkin.md` and records its name in `LAST_CHECKIN`.

A tick returns as soon as the work it starts is admitted. If the kernel's
worker pool is full, the tick is skipped and logged.

## Rotation

The wrap-up turn asks the seat to write its handoff notes, with as many steps
as it needs. The next tick, about an hour later, rotates the context
(`kj context rotate`). The successor has the same character with slots read
fresh, the label `main`, and `ROTATED_FROM` set. It reads the handoff notes
and a short excerpt of the predecessor.

The beat does not rotate the context; the tick does, as an rc lifecycle
rotating its own context. A rotation copies the predecessor's beat
attachment to the successor. The successor's `create/S60-attach.kai`
announces it to the clock, and the predecessor's `archive/S10-detach.kai`
removes it. The person can rotate the context at any time with `Ctrl+A r`,
and the same scripts keep the clock.

## Changing it

- **What a check-in looks at:** the list in `fork/S20-stance.md`. Add a tool to
  its loadout in `fork/S25-binding.kai`, then reseed or edit the live copy. A
  check-in stays read-only unless you grant it more.
- **How often:** the `--wakeup` of the attachment, and the tempo.
- **When to rotate:** `ROTATE_AT_TOKENS` at the top of the tick script.
- **A different persona:** the stance files. They stay plain, ASCII English.
- **The tool roster:** `create/S10-binding.kai`. Every tool schema rides on
  every request, so the roster is the seat's largest cost.

Editing an rc file changes future lifecycle runs, not stored instructions
(`docs/prompts.md`). A running context keeps what it was made with until it
rotates.

## What is open

See `docs/issues.md`, "Companion".

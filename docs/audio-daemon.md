# Audio daemon

The audio daemon owns local MIDI and PCM I/O. The kernel owns contexts,
scores, transport and accepted mutations. App and TUI retain local beat
phasors for display; they do not need to own hardware to follow musical time.

Amy: "a realtime audio daemon we can put on various machines that have audio
hardware"; "don't overrotate on realtime. mostly saying it'll be privileged
and get some rtprio on linux". Keep the existing scheduling algorithms and
give the timing threads an optional Linux scheduling priority.

## Implementation

1. The DJ, PCM scheduler, MIDI workers and matcher live in
   `kaijutsu-audio-runtime`, with their tests. Keep
   `kaijutsu-audio` as the portable data/timebase crate.
2. `kaijutsu-audiod` owns the SSH connection, explicit optional capture context,
   device selection, presence reporting, reconnect and clean shutdown.
3. App hardware I/O is opt-in through `--audio` and the same library. One process owns
   local I/O; an ownership lock prevents accidental duplicate playback.
4. App and TUI can use `LocalBeat` without loading the hardware runtime.

## Scope

Includes MIDI capture/output, clock estimation, device presence and SysEx,
PCM playback with trim/gain, CAS prefetch, metronome and transport flush.
The first daemon uses the current broadcast render contract: all attached
render clients receive playback cues. Named destination routing is separate
work. No PCM recording, plugin host, or network PCM stream is introduced.
Offline beat analysis stays outside the timing threads. MIDI currently uses
ALSA on Linux; CoreMIDI remains unimplemented.

## Run

```bash
cargo build --release -p kaijutsu-audio-runtime --bin kaijutsu-audiod -j 2
./target/release/kaijutsu-audiod --list-outputs
./target/release/kaijutsu-audiod --host zorak --user amy --key '/path/to/audio-key' --rt-priority 20
```

The key must already be enrolled with the kernel. SSH host verification is on;
prepare the account's known_hosts as for any other Kaijutsu SSH client.
Port defaults to 2222. All log output goes to stderr.

PCM uses the default output unless `--output '<exact device name>'` is given.
An unknown or ambiguous name fails; the runtime never chooses another device
silently. `--no-audio` runs MIDI only. `--no-midi` runs PCM only. MIDI defaults
on for Linux and off elsewhere. At least one must be enabled.

Without `--context`, the daemon reports device presence and plays cues, but
does not submit MIDI capture or external-clock estimates. For capture, pass an
existing context id or label with `--context`. It must be attached to a track
through `kj transport attach` for capture commits to succeed. The daemon does
not create contexts, attach tracks or start model turns on its own.

The daemon registers as an `audio/<hostname>` peer. Its `status` action reports
enabled I/O, the selected output, capture context and requested RT priority.
The priority field is the request, not proof that the OS granted it.

## Lifetime and ownership

An advisory file lock permits one runtime per OS user on a machine, whether
hosted by the app or daemon. Keep both under the same user to share that lock;
different OS users must coordinate device ownership. The lock is released
after worker shutdown, including the MIDI exchange worker. The lock file is
left in place so another process cannot bypass a held lock by replacing it.

Missing enabled hardware fails startup. PCM stream failure or a stopped
capture worker stops the runtime so a service manager can restart it.
MIDI hotplug updates the profile match and local routes. Profile/config reads
and presence are refreshed after reconnect; reconnect is owned by the existing
SSH actor. Transport stop, connection loss and shutdown flush scheduled output.
Late musical events follow `docs/midi.md`, "The one timebase".

The app defaults to no hardware I/O. `kaijutsu-app --audio` loads the runtime
in-process for a single-process setup. The patch bay still reads local ALSA
topology; remote topology and daemon traffic animations are follow-up work.

## Linux service

On moltar, build locally and run the installer as your ordinary user:

```bash
cargo build --release -p kaijutsu-audio-runtime --bin kaijutsu-audiod
python3 contrib/install-audiod.py --host zorak
```

The installer creates an unencrypted Ed25519 key at
`~/.ssh/kaijutsu-audio-<hostname>`, or verifies and reuses the existing key.
It prints the public key and enrollment commands, then pauses. Create
`audio/moltar` through `kj character create` on the kernel, copy only the
public key to zorak, and run the printed `kaijutsu-server add-key` command
there with that file's actual path. Prepare known_hosts with the verified
kernel host key; the installer never disables host verification.

Enter `start` to install the binary under `~/.local/bin/`, write the user
unit, enable it and restart it. It checks the daemon's ready log and stops
the service if startup fails. Existing units are backed up beside the unit;
existing keys are never replaced. Reruns use `--enrolled` to skip the pause.
`--binary` selects another build; `--no-audio`, `--no-midi`, `--output`,
`--context` and `--rt-priority` configure the service (both backends enabled,
no capture context and requested priority 20 by default).

No sudo, device ACL, group membership, RT limit or lingering changes are made.
Run in the desktop user's session for its PipeWire access. Stop app hardware
I/O (`--audio`) before starting the daemon. The same-user ownership lock
rejects a second runtime.

`contrib/kaijutsu-audiod.service` is a user-unit example. Install the binary
under `~/.local/bin/`, copy the unit to `~/.config/systemd/user/`, and set its
`ExecStart` to the connection and device options for that machine. A service
usually needs `--key`; it does not inherit an interactive shell's SSH agent.

```ini
[Service]
ExecStart=
ExecStart=%h/.local/bin/kaijutsu-audiod --host zorak --key %h/.ssh/kaijutsu-audio --rt-priority 20
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now kaijutsu-audiod.service
journalctl --user -u kaijutsu-audiod.service -f
```

Priority zero preserves the inherited scheduling policy. A nonzero request
tries `SCHED_RR` on the DJ, PCM scheduler and MIDI capture threads. Permission
failure warns and continues. Network, prefetch and exchange workers keep their
ordinary policy when the process starts under ordinary scheduling.

A user manager cannot raise RT limits beyond those it inherited.
`contrib/kaijutsu-audiod-system.service` is the system-service alternative:
it grants `LimitRTPRIO=40` to a dedicated account and requests priority 20
only on timing threads. Provision that account, device access, SSH key and
known_hosts before enabling it. Update the executable path and kernel host.
For audio served by a desktop session, use its user service; a separate system
account does not automatically have access to that session's audio server.

Neither example is installed or enabled by the build.

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
3. One process owns local I/O per OS user; the ownership lock guards against
   two daemons.
4. App and TUI can use `LocalBeat` without loading the hardware runtime.

## Scope

Includes MIDI capture/output, clock estimation, device presence and SysEx,
PCM playback with trim/gain, CAS prefetch, metronome and transport flush.
MIDI inputs also retain bounded RAM history for retrospective keeps, described
below. This does not require a recording context.
The first daemon uses the current broadcast render contract: all attached
render clients receive playback cues. Named destination routing is separate
work. No PCM recording, plugin host, or network PCM stream is introduced.
Offline beat analysis stays outside the timing threads. MIDI currently uses
ALSA on Linux; CoreMIDI remains unimplemented.

## Keep recent MIDI

```bash
kj audio devices --node audio/moltar
kj audio keep --node audio/moltar --source '24:0' --generation '<generation-uuid>' --seconds 10
kj audio keep-status '<job-uuid>'
```

Use an actual address and generation from `devices`; `24:0` is illustrative.
`ports` is the raw inventory, including unmatched and output-only endpoints;
`sources` lists retained input histories and their generations. Capabilities,
listening success, errors, available coverage and input-loss counts are separate
facts. A generation changes when its endpoint is replaced or restarted. The
inventory above is read through the kernel's existing named-peer RPC; the
consolidated `/run/audio/<node-dir>/inventory.json` projection ("One inventory
owner" below) is a second, VFS-readable path to a related but distinct picture
— the whole local seq graph plus per-endpoint event counts, not `devices`'
retention-and-capabilities detail. `kj midi list` still shows the older
profile-matched presence view.

Each listening source retains up to 60 seconds, 1 MiB and 64 KiB per message,
within 16 MiB of node history and 64 source slots. These are implementation
defaults, not CLI options yet. Raw MIDI clock/active sensing is included in
history; the explicit recording consumer keeps its existing filtering. Inventory
is reconciled every two seconds and after hotplug. A separate observer drains
bounded ingress while the runtime waits on kernel RPCs.

The default keep is ten seconds. Relative windows end at a confirmed local
read watermark, not the time an RPC happens to arrive. A watermark older than
one second fails. Missing coverage, input loss, expired history and changed
generations fail explicitly. No notes are invented and windows are not silently
shortened. Quiet input can have complete coverage with zero MIDI events.

The keep reply names a kernel-owned job after the daemon confirms a protected
RAM snapshot. It does not mean CAS publication has completed. `keep-status`
reports progression through upload, acceptance and release, with `complete`
and a hash after kernel acceptance. A lost acknowledgement returns an error
with an inspectable job id and unconfirmed protection. Jobs are pinned to the
selected daemon process instance; two connected instances with the same name
are refused rather than choosing one.

```bash
kj audio keep-retry '<job-uuid>'
kj audio keep-cancel '<job-uuid>'
```

Failed uploads keep the same RAM snapshot. Retry uses fresh private staging;
it never selects a newer musical window. Cancellation waits for the active
writer to stop before deleting that job's staging. Up to eight active takes
share a 32 MiB encoded RAM budget; each artifact is capped at 16 MiB. Snapshot
preparation reserves its maximum encoded budget before copying. Status records
are bounded to 32; only released/cancelled records may be evicted in the daemon.
Transient snapshot/encoding memory is additional and bounded by preparation
admission. Hashing and SFTP run on ordinary workers, not the ALSA read thread.

SFTP uploads use acknowledged chunks of at most 64 KiB to a unique private
`/tmp/kaijutsu-audio-<uuid>/capture.json`. The kernel verifies that VFS `/tmp`
maps to host `/tmp`, then uses the same streaming CAS writer as `kj cas put`.
It checks length and hash before acknowledging release of daemon RAM. No new
transfer protocol, shell execution site or automatic track placement is added.

The CAS artifact is JSON with MIME
`application/vnd.kaijutsu.midi-history+json`, not SMF or ABC. It carries node,
process instance, source generation, event positions, monotonic offsets and
wallclock anchors alongside raw MIDI bytes. It is inspectable source material;
conversion to a playable score is a separate operation.

Current failure boundaries: RAM takes do not survive daemon restart. Keep jobs
are kernel-ephemeral; kernel restart loses their ownership metadata and may
leave exact staging directories behind. Same-process SSH reconnect can recover;
a permanently gone process leaves its pinned job pending. No force-abandon or
broad staging-cleanup sweep is implemented. Successful publication and explicit
cancellation clean their own staging paths only.

## Observation, inventory and retained history

Amy: "anything in the system can ask for a hunk off the buffer and the reads
are already done"; "it doesn't have to be realtime except for keeping up
with the read off audio buffers." The daemon owns local observation and
retention. The kernel owns accepted configuration, inventory and publication.
Readers request retained material; they do not each open a hardware stream.

### Separate observations from musical use

Host, physical device, backend endpoint, musical role, channel/port mapping
and device programming are separate things. Amy moves devices frequently;
their roles can change with their connections. Presence must not restore an
old role or program merely because a familiar device returns.

- Inventory reports every exposed endpoint, including unmatched devices.
  Profiles annotate observations; a profile match is not a physical identity.
- Device identity evidence may include serials or inquiry replies. Names and
  backend addresses alone do not prove a unique physical device.
- Endpoint addresses are valid within one node stream generation. Replug,
  backend restart and format changes invalidate that generation.
- MIDI channels, USB/port maps, role assignments and desired programming
  belong in editable configuration, not observed presence. Their exact file
  schema is a later routing slice; see `docs/midi-next.md`.
- A future binding composes those inputs immediately before scheduling a
  batch. Programming precedes notes; invalidation cancels pending output.
  This plan does not make today's broadcast playback destination-aware.

### One inventory owner

Shipped, alongside `MidiPresenceStore`/`/run/midi` rather than competing with
it: `AudioInventoryStore` (`crates/kaijutsu-kernel/src/audio_inventory.rs`)
and the read-only view it mounts at `/run/audio`. Node path encoding:
`/run/audio/<node-dir>/inventory.json`, where `<node-dir>` is the daemon's
peer nick with every `/` replaced by `-` (`audio/moltar` → `audio-moltar`,
`kaijutsu_types::paths::audio_node_dir`) — a directory per node, one
`inventory.json` leaf each. Display names inside the body are not filesystem
components; only the node segment of the path is derived from one, and
that derivation is the one place doing it.

The daemon sends a full report on every (re)connection and whenever its
observed topology, its own client set, or any endpoint's event count changes,
rate-limited to at most one report per second and at least one every ten
seconds while connected (`kaijutsu-audio-runtime/src/runtime.rs`'s `serve`
loop, cadence decided by `inventory_report::report_due`). The kernel
associates each report with its connection id and a daemon-assigned
`revision`: a report from an older connection than the one currently holding
a node cannot replace or reap the newer one, and a same-connection report
whose revision does not exceed the one on file is dropped — see
`AudioInventoryStore::record`. Two nodes remain separate entries regardless of
profile. The daemon sends `received_epoch_ns: 0` and `stale: false`; the
kernel overwrites both with its own values before serving the projection —
only the kernel knows either. Disconnect does not remove a node's last report:
`ConnectionState::drop` calls `AudioInventoryStore::reap_connection`, which
marks every node that connection held `stale: true` and keeps the body (the
audio-daemon rule differs from `MidiPresenceStore::reap_connection`'s removal
on purpose — a node's last known wiring is still useful once the daemon
disappears). Kernel restart clears the ephemeral store entirely.

`state` distinguishes `pending`, `ready` and `error`, mirrored from the
runtime's existing MIDI `Observation`; a ready empty inventory means no
endpoints were found. `own_clients` names the daemon's own plumbing ALSA
clients only — the ear (`kaijutsu-ear`), the exchange client
(`kaijutsu-exchange`) and the patch-graph reader (`kaijutsu-patchview`) —
**never** the render client (`kaijutsu-audio`/`render`, `dj/midi.rs`): that is
a real musical endpoint, and the patch bay pulses traffic from its `events`
count. `events` is per-endpoint and monotonic: an input's count is how many
MIDI events the ear has observed from that address
(`Observation::event_counts`); the render port's count is how many events the
DJ has sent out it (`dj::midi::RENDER_EVENTS_SENT`, a process-wide counter
read from a different thread than the one that increments it, since there is
exactly one render port per process). The kernel never enumerates host
hardware itself; `crate::patch_graph::PatchGraphReader` — a second, dedicated
ALSA client separate from the ear and the DJ's render port — is what lets one
report reflect the whole local seq graph rather than only what the ear or the
DJ happen to see.

Unplug/replug address reuse and missed-notification reconciliation are
already covered by the observer's own reconciliation (see the "Inventory
audit complete" note below) and are not re-tested here; this slice's own test
coverage is the store's connection/revision ordering and stale-marking
(`audio_inventory.rs` unit tests), the wire round-trip and disconnect-to-stale
timing (`kaijutsu-server/tests/audio_inventory_wire.rs`), and the report
builder's endpoint/`own_clients`/`events` assembly
(`kaijutsu-audio-runtime/src/inventory_report.rs` unit tests).

### Local history and window contract

```text
local input -> bounded retained history
                  |-> summary windows -> kernel ephemeral state
                  |-> requested window -> encode -> upload -> kernel CAS
                  `-> explicit recording -> existing track/clip paths
```

One owner drains each watched input. MIDI history is event-oriented; PCM
history is frame-oriented. Both expose a stream generation, head, oldest
retained position, format, retention budget and loss counters. Independent
reads do not consume material or advance another reader's cursor.

A window is a half-open interval within a named stream generation. MIDI
uses monotonically increasing local event positions; PCM uses frame positions.
These positions are not kernel mutation sequences. Relative requests such as
"last ten seconds" are resolved once against a sampled local head. Local
monotonic elapsed time determines retention and relative windows; wallclock
anchors support cross-host interpretation but never reorder captured material.
Capture/playback timing follows `docs/midi.md`, "The one timebase".

The reply reports requested and actual coverage, generation, format, timing
anchors and gaps. Expired history, an unknown generation, a stopped stream,
oversized requests and hardware overruns are explicit outcomes. No zero-padding
or invented events. Partial results require an explicit caller choice; the
default rejects incomplete coverage. Stopping a watch closes input but may
retain its history until the configured retention expires; restarting starts
a new generation and cannot silently join the two.

Snapshot admission freezes or copies the selected material under a strict
budget. Readers never hold the ingestion lock while encoding or uploading.
Bound bytes per source, total node bytes, individual MIDI/SysEx messages,
ingress queues, snapshot bytes and concurrent exports. An event-count bound
alone is insufficient for variable-size SysEx. Slow readers cannot pin
unbounded history. Failure to admit a snapshot is explicit and leaves
ingestion running. Record loss at each queue boundary, not only ring overwrite.

No PCM input is opened by default in the first implementation. MIDI observation
keeps its current broad subscription policy, excluding our own clients and
known loopback sources. Per-source watch start/stop allows explicit changes.
Retention defaults and budget eviction/admission policy settled into initial
budgets (`HistoryLimits::default`, `crates/kaijutsu-audio-runtime/src/history.rs`:
60 s retention, 1 MiB per source, 16 MiB per node, 64 sources, 64 KiB per
message, 8 MiB keep budget); there is no config surface to change them yet.
48 kHz stereo float32 consumes 384,000 bytes/second before metadata.

SSH reconnect alone does not start a new hardware stream generation: local
capture may have continued without loss while offline. A daemon process restart,
watch restart, device replacement or format discontinuity does. Separate kernel
connection ownership from local stream continuity. Across reconnect, a request
may name retained history only after the current node connection advertises
that generation and coverage again. Missing history is never fabricated.

### Deferred: ambient recording, summaries, publication (unshipped)

Slices 3–6 below are still open. The direction, in brief:

- **Ambient recording.** Watching an input keeps a rolling recording without
  a track or context — a RAM backend first, a local-NVMe rolling-chunk
  backend for longer retention later. "Keep the last N seconds" reserves its
  window at one sampled local head so rolling eviction cannot remove an
  acknowledged take; a keep acknowledgement must say whether protection is
  memory-only or locally durable. No disk capture exists yet.
- **Summaries and queries.** Deterministic window summaries (event counts,
  note/velocity ranges, changed CCs, clock estimate) reuse the existing MIDI
  cursor/window mechanism and need no context or model turn. SysEx probing
  is explicit request/reply work built on `kj midi identify` and the
  exchange worker; the daemon enforces message/reply limits and generation
  validity across concurrent scripts.
- **Kernel publication.** A requested window becomes an immutable artifact
  only when exported: encode/hash/upload on ordinary workers, verified and
  accepted by the kernel before it is a usable content reference. Exporting
  is not automatically a block or track placement — that is a separate,
  explicit kernel operation.

### Slices

Each slice adds failing tests first. No compatibility layer is required for a
replaced Kaijutsu mechanism.

- [x] **1 — inventory.** The coherent `/run` read: `reportAudioInventory`
  (`kaijutsu.capnp`), `AudioInventoryStore` + the `/run/audio` projection
  (`crates/kaijutsu-kernel/src/audio_inventory.rs`), and the daemon-side
  report builder + cadence (`kaijutsu-audio-runtime/src/inventory_report.rs`).
- [x] **2 — retained MIDI windows.** Independent retrospective reads and byte
  bounds on the existing capture substrate (`kaijutsu-audio/src/capture.rs`);
  per-source lifecycle and bounded ingress.
- [ ] **3 — ambient summaries.** Publish a bounded deterministic observation
  feed with no context.
- [ ] **4 — snapshot to CAS.** Freeze bounded windows, encode and upload
  through kernel acceptance; expose `kj` operations. Current artifact limit
  is 16 MiB; longer artifacts and restart recovery remain open.
- [ ] **5 — PCM input.** Opt-in input watches behind the same coverage and
  lifecycle contract, then the local-NVMe rolling-chunk backend for longer
  ambient recordings.
- [ ] **6 — scripted probing.** Bounded transactions and sample scripts;
  device-specific queries require documented protocol evidence.

Do not treat synthetic tests as physical playback or capture verification.
Do not restart another machine's daemon as a side effect of building these
changes.

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
The priority field is the request, not proof that the OS granted it. `status`
also reports the node's model of the kernel's clock — applied offset,
uncertainty, sample count and whether it is dialed in (`docs/midi.md`, "The one
timebase").

## Lifetime and ownership

An advisory file lock permits one runtime per OS user on a machine. The app
no longer hosts a runtime of its own — it depends only on `kaijutsu-audio`,
not `kaijutsu-audio-runtime` — so `kaijutsu-audiod` is always the one holding
the lock; different OS users must coordinate device ownership. The lock is released
after worker shutdown, including the MIDI exchange worker. The lock file is
left in place so another process cannot bypass a held lock by replacing it.

Missing enabled hardware fails startup. PCM stream failure or a stopped
capture worker stops the runtime so a service manager can restart it.
MIDI hotplug updates the profile match and local routes. Profile/config reads
and presence are refreshed after reconnect; reconnect is owned by the existing
SSH actor. Transport stop, connection loss and shutdown flush scheduled output.
Late musical events follow `docs/midi.md`, "The one timebase".

The app has no hardware I/O; `kaijutsu-audiod` is the sole hardware owner.
The patch bay reads `/run/audio/<node-dir>/inventory.json`, one table per node,
and pulses render traffic from that node's own event counters — a remote
node's fabric (moltar's rack, say) is visible from any connected app the
same way a local one is.

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
Run in the desktop user's session for its PipeWire access. The same-user
ownership lock rejects a second daemon.

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

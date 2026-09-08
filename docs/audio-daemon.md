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
inventory is read through the kernel's existing named-peer RPC; the consolidated
`/run/audio` projection remains planned, not implemented. `kj midi list` still
shows the older profile-matched presence view.

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

## Evolution: observe once, retain windows, request material

This section is the implementation plan, not a description of shipped APIs.
The runtime and installation sections describe what is available today.

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

Evolve the existing kernel `MidiPresenceStore` and `/run/midi` projection;
do not add a competing presence authority. Target projection:
`/run/audio/<node>/inventory.json`, with stream status and summaries alongside
it. The exact node path encoding and migration of existing readers must be
settled in slice 1. Display names are not unvalidated filesystem components.

The daemon sends a full snapshot on connection and versioned replacements
on topology change. The kernel associates reports with its connection id,
assigns an acceptance sequence, and exposes one coherent snapshot. An old
connection cannot replace or reap a newer connection's observations. Two
nodes matching the same profile remain separate entries. Reports carry source
observation time and kernel receipt time, not a cross-host last-wallclock-wins
ordering. Disconnect invalidates current presence; a retained last observation
is labeled stale. Kernel restart clears ephemeral state.

Distinguish backend disabled, scan pending, ready and error. A ready empty
inventory means no endpoints were found. Disconnected means unknown, not
absent. Observed, opened and usable are different facts. Hotplug is advisory:
backend errors and periodic reconciliation repair missed notifications.
The kernel never enumerates host hardware itself.

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
Retention defaults and budget eviction/admission policy remain review decisions;
48 kHz stereo float32 consumes 384,000 bytes/second before metadata.

SSH reconnect alone does not start a new hardware stream generation: local
capture may have continued without loss while offline. A daemon process restart,
watch restart, device replacement or format discontinuity does. Separate kernel
connection ownership from local stream continuity. Across reconnect, a request
may name retained history only after the current node connection advertises
that generation and coverage again. Missing history is never fabricated.

### Ambient recording and keeping a take

Ambient recording is a primary use of retained history, not just diagnostic
telemetry. Amy: "if we turn 'em up and put 'em on nvme or use plenty of ram,
we can keep enough to occasionally grab a happy accident real quick."
Watching an input can retain a substantial rolling recording without a track
or context. Retention remains finite and visible; export is deliberate.

Support a RAM history backend first and a local-NVMe rolling-chunk backend
when longer retention needs it. They share generation, position, coverage and
loss semantics. Storage is local to the daemon; network availability is not
required for ambient recording. Disk-backed history is not automatically
kernel CAS content and is not a promise of permanent archival storage.
At 48 kHz stereo float32, one hour is about 1.38 GB before metadata; the
configuration should express time and byte budgets so the tradeoff is visible.

"Keep the last N seconds" resolves its end at one sampled local head and
reserves the actual window before asynchronous encoding/upload begins. After
the reservation is acknowledged, rolling eviction cannot remove that take.
Keep requests must support meaningful musical windows, not a fixed small RPC
payload limit. Chunked export keeps a long take from requiring one large
contiguous allocation. Repeated keeps and slow uploads remain bounded by a
separate retained-take budget; refuse new requests explicitly when exhausted,
without interrupting ambient ingestion or discarding an acknowledged take.
Release kept material only on successful publication or explicit cancellation.
The keep acknowledgement must name whether protection is memory-only or
locally durable; it cannot imply that RAM survives a process or machine crash.

The NVMe backend needs preallocated/reusable chunks, explicit disk-full and
I/O-loss reporting, and a crash-recovery/index contract before it ships. File
allocation, filesystem work and sealing stay off the input callback. Stopping
a watch stops acquisition, not a previously acknowledged keep operation.
Default retention sizes, persistent watch settings, durability policy and the
eventual `kj` spelling remain open; no disk capture is enabled by this plan.

### Summaries and queries

Reuse the existing independent MIDI cursor/window mechanism for deterministic
summaries: event counts by type/channel, note and velocity ranges, changed CCs,
last observed bank/program, transport observations, clock estimate and loss.
Window statistics and carried last-observed state are separate. Observation
does not assign musical roles or establish current device programming.
Clock/active-sensing counters are collected before musical capture filtering.
Summaries do not require a context, create blocks, or trigger model turns.
An explicit recording consumer may continue committing windows through the
existing capture path. Retrospective reads and summaries neither require nor
replace that consumer. Retention reads do not delete events after reduction.

SysEx probing is explicit request/reply work. Existing `kj midi identify` and
the exchange worker supply the starting mechanism. Scripts choose known
queries, refresh intervals, backoff and when a deeper dump is useful. The
daemon enforces message/reply limits, serialization, timeouts and endpoint
generation validity even when several scripts ask at once. Query policy can
defer expensive work during playing; incoming replies still need accounting.
Do not periodically broadcast arbitrary SysEx to unknown devices.

An exchange's separate ALSA client does not by itself keep device replies out
of the ambient ear. Classify known transactions and unsolicited SysEx without
claiming certainty where the protocol has no transaction identifier. Keep raw
observations available within budgets; exclude classified settings dumps from
musical interpretation, not silently from all history. Tests must cover reply
fanout and ambiguous replies before automatic probing is enabled.

### Kernel publication and kaish utilities

Occasional watch configuration, inspection, snapshot requests and probe policy
belong in `kj` and scripts. Chatty daemon reports use RPC. Device codecs and
window reducers are reusable Rust functions beneath those verbs. Scripts never
open another ALSA client or dispatch a shell command for each incoming event.
Exact verb names and JSON schemas are not committed by this plan.

Requested MIDI/PCM windows become immutable artifacts only when exported.
Encode/hash/upload on ordinary workers, not in the hardware callback. The
daemon's existing SFTP/CAS path downloads playback assets; publishing capture
requires a separate upload/acceptance path. Prefer existing CAS staging APIs
where their contract fits. The kernel verifies and accepts bytes before
returning a globally usable content reference. A local hash is not proof that
the kernel has the artifact. Preserve generation, format, actual coverage,
timing anchors and gaps in an accompanying manifest.

Retries must not duplicate recording mutations. Cancellation releases bounded
snapshot resources; incomplete uploads must not appear as accepted artifacts.
An export is not automatically a block or a track placement. Those are explicit
kernel operations after artifact acceptance. Offline retention is bounded;
reconnect does not upload all history or replay it as live music.

Hootenanny prior art: chaosgarden `stream_io.rs` writes local mmap chunks while
hootenanny owns staging/sealing. Reuse that separation, not its shared-path
assumption across hosts. Its output-tap snapshot consumes an SPSC buffer;
our retained-window API must support independent retrospective readers.

### Execution checklist

Each slice adds failing tests first and updates this section to distinguish
library primitives, wired APIs and live verification. No compatibility layer
is required for a replaced Kaijutsu mechanism.

Implementation progress:

- Inventory audit complete: the older profile presence path discards unmatched ports, and
  an initial empty profile match sends no report. The live JD-Xi has no profile;
  all-unknown profile presence does not prove a connection failure. Raw inventory
  and matching health are now reported independently through raw inventory.
  Profile snapshot denial and truncation currently go unchecked; reload is
  reconnect-driven. Raw inventory now reconciles periodically and after hotplug.
  Profile presence currently
  carries queried identity across present-to-present replacements even if the
  endpoint changed; replace that with generation-scoped identity.
- Pure MIDI primitive implemented in `kaijutsu-audio/src/capture.rs`: explicit
  message/retained-byte limits, fallible admission and non-destructive owned
  position-window snapshots. Six regression tests cover independent reads,
  expired/future coverage, byte eviction/rejection, wallclock rollback and
  overwrite after snapshot. The runtime now has a per-source stamped history
  manager for time retention/generations, bounded ingress and protected keeps;
  its explicit-recording ring is byte-bounded too. A `VecDeque` supports direct
  time eviction; it is not an SPSC queue pretending to be shared history.
  Source watch controls and configurable policy remain open.
- MIDI keep/export vertical path implemented; see "Keep recent MIDI". Runtime
  tests cover loss fences, sampled heads, cancellation, upload chunks and hash
  ownership. Kernel tests cover instance pinning, private staging and CAS-before-
  release. A live synthetic SFTP upload on zorak passed; musical-input capture
  on moltar is not yet verified. No running daemon or kernel was replaced.

- [ ] **1 — inventory.** Diagnose current unknown MIDI presence. Expose raw
  MIDI and PCM endpoints, per-node/per-connection state and coherent `/run`
  reads. Tests: unprofiled JD-Xi-shaped endpoint is visible; two equal models
  on different nodes; unplug/replug address reuse; stale connection cleanup;
  disabled/empty/error states; missed notification reconciliation.
- [x] **2 — retained MIDI windows.** Add independent retrospective reads and
  byte bounds to the existing capture substrate; wire per-source lifecycle
  and bounded ingress. Tests: repeated/overlapping reads; overwrite and
  oversize messages; clock rollback; stale generation; slow reader; stop and
  restart; no source can exhaust the node budget unnoticed.
- [ ] **3 — ambient summaries.** Publish a bounded deterministic observation
  feed with no context. Tests: note-on velocity zero, notes spanning windows,
  CC bursts, clock-only source, loss invalidating inferred state, idle expiry.
- [ ] **4 — snapshot to CAS.** Freeze bounded windows, encode and upload through
  kernel acceptance; expose `kj` operations. Tests: expired/partial requests,
  corruption, interrupted upload, retries, cancellation and concurrent readers.
  Add immediate keep reservations: eviction cannot remove an acknowledged take,
  and a full take budget rejects new keeps without stopping input. Export long
  windows in bounded chunks rather than imposing a small whole-take byte limit.
  The bounded MIDI path is implemented; longer artifacts and restart recovery
  remain open. Current artifact limit is 16 MiB.
- [ ] **5 — PCM input.** Add opt-in input watches behind the same coverage and
  lifecycle contract. Tests: frame alignment, format changes, xruns, retention
  budget, unplug while reading, and callback progress during a slow export.
  Follow with the local-NVMe rolling-chunk backend for long ambient recordings;
  test disk-full, chunk reuse, crash recovery and kept-take protection. Mark
  RAM-only and durable keep acknowledgements distinctly.
- [ ] **6 — scripted probing.** Expose bounded transactions and sample scripts.
  Tests: no reply/backoff, competing requests, reply fanout, disconnect and
  endpoint reuse. Device-specific queries require documented protocol evidence.

Inventory investigation and the pure retained-MIDI primitive can proceed in
parallel. Wire changes, CAS upload and default budgets wait for design review.
Live acceptance is moltar inventory and unplug/replug first, then retrospective
MIDI windows and an opt-in PCM capture. Do not treat synthetic tests as physical
playback or capture verification. Do not restart another machine's daemon as a
side effect of building these changes.

### Review and open decisions

Gemini Pro batch review through kaibo was submitted as
`gemini/batches/haggchzwu5upwehlrqyxy7qimz0pw75xibpw` using
`gemini-pro-latest`. Review collected; dispositions:

The ambient-recording/NVMe and immediate-keep requirements were added after
this batch review. They require a focused storage/lifetime review before that
backend is implemented; do not treat this batch as covering those additions.

- Accepted: isolate source retention, bound bytes/messages/exports, expose
  multi-node inventory, scope identity to generations and classify reply fanout.
  These remain acceptance conditions, not claims that runtime wiring is done.
- Clarified: network reconnect is not necessarily a capture discontinuity.
  Connection ownership and hardware stream generations are separate lifetimes.
- Declined: removing every four-second capture commit. Explicit recording is an
  independent consumer and can coexist with passive retrospective history.
- Declined: exclusive port locking as reply classification. It does not supply
  protocol correlation and must not interrupt normal input observation.
- Corrected: the review saw tests for byte bounds without their implementation
  while the subagent was in its red/green cycle. The completed primitive has
  production byte limits and passing tests; the runtime still uses count-only
  construction, so runtime byte bounds remain open.
- Corrected: `CuePayload::Inline | Cas` is a playback reference contract, not
  a capture-upload API. Slice 4 must select and verify kernel CAS acceptance.
- Budget suggestions were not adopted. The review's 1 MB MIDI source budget
  does not guarantee hours of input; payload rate and event metadata matter.
  Its 5 MB export limit would cover only about 13 seconds of 48 kHz stereo
  float32. Retention and request defaults must be chosen together with a
  visible node budget, not inferred from an event-count limit.

The implemented MIDI path above settles initial budgets, upload reuse and keep
verbs. Node path encoding and watch controls remain open. The full
role/port/channel/programming binding remains a later routing plan, not an
implicit part of observation.

Implementation review collected from Gemini Pro through kaibo:
`gemini/batches/wnpwihn04wyjir98qvfzvem06lioopihh1wm`.

- Declined: replace CAS `write` with `write_all`. `StreamingWriter::write`
  returns `Result<(), StoreError>`, not a byte count; its staging writer already
  writes the full slice. The review assumed the `std::io::Write` contract.
- Accepted as follow-up: ingress loss currently stops retained sources until
  inventory reconciliation, then starts new generations. This discards old
  history and may reject up to two seconds of otherwise valid new input.
  Do not simply remove the reset: the shared ingress queue can also lose hotplug
  events, so continued device identity is not assured. Separate data loss from
  topology uncertainty before preserving generations across loss.

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

The app has no hardware I/O; `kaijutsu-audiod` is the sole hardware owner.
The patch bay reads `/run/audio/<node>/inventory.json`, one table per node,
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

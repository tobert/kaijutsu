# isotest — containerized isolation & process-lifecycle tests

`contrib/isotest` runs functional tests that need a **real kernel death**: the
in-process integration suites (`kaijutsu-server/tests/`) can't `kill -9` the
kernel they live inside, so process-lifecycle guarantees — the PDEATHSIG
orphan guard, process-group kills, restart hygiene — were previously "manual
smoke on zorak is the only evidence". This harness makes them regular,
repeatable assertions.

## Why a container

Not for security — for **observability and blast radius**:

- The podman PID namespace starts empty, so after a test tears down, any
  process left in `/proc` is ours and attributable. "No orphans" becomes a
  provable invariant (`assert_no_survivors`), not a grep against host noise.
- `kill -9` storms, restart loops, and hostile filesystem layouts can't touch
  the host or a live kernel.
- `--network=none` (loopback only), `--pids-limit` (a fork storm fails the
  test, not the machine), tmpfs `$HOME`. All rootless-friendly; no privileges.

## Running

```sh
contrib/isotest                  # the kaijutsu-isotest suite
contrib/isotest orphan           # filter by test name
contrib/isotest --keep           # keep the exited container for inspection
contrib/isotest --pull           # refresh the Arch base image
contrib/isotest -p kaijutsu-kernel --test broker_e2e
                                 # containerize any workspace test target
```

Test executables are discovered from `cargo test --no-run
--message-format=json` (survives renames and hash churn). Binaries are built
on the host and bind-mounted read-only; `contrib/Containerfile.isotest` is a
near-empty Arch base whose only job is supplying a compatible userland — an
`ldd` preflight fails loudly on glibc skew. Cadence today: run it by hand
after touching exec/lifecycle paths; it is the mandatory gate for the
background-exec → kaish-jobs swap (`docs/issues.md`). CI eventually.

## Topology

catatonit (podman `--init`, PID 1) → test binary → spawns `kaijutsu-server`
as a real child on a fresh tmpfs `$HOME` → connects over loopback SSH with
`kaijutsu-client` → joins the genesis ROOT context (a director: `exec` +
`facade:shell` already granted) → drives the `shell` tool → kills the
server for real → asserts on `/proc`.

**Credentials: always ephemeral, always labeled.** Every key the suite
mints is generated fresh per boot and carries the `isotest-ephemeral` label
in the auth.db nick, the pubkey comment, and the SSH username — a stray
entry can never be mistaken for a durable identity. Registration happens
via `add-key` pre-boot (the shipped binary has `allow_anonymous: false` and
no registration RPC). Most tests authenticate with an in-memory key; the
agent test runs a real `ssh-agent` inside the namespace and injects the key
via the agent protocol, so that private key never touches disk at all.

Tests run `--test-threads=1` (the runner enforces it): `/proc` assertions
must never interleave.

## What it pins (crates/kaijutsu-isotest/tests/isolation.rs)

- **`orphan_guard_on_sigkill`** — the PDEATHSIG proof: a background job must
  die with a SIGKILLed kernel. Mutation-verified RED (2026-08-09): removing
  `set_parent_process_death_signal` from `background_exec.rs` makes this fail
  with the sleep surviving — the exact regression the kaish-jobs swap risks,
  since kaish 0.13 has no PDEATHSIG anywhere.
- **`orphan_guard_on_sigterm`** — same guarantee under default-action SIGTERM.
- **`kill_reaps_whole_process_tree`** — `kill_background_process` takes the
  whole `setpgid` group, grandchildren included.
- **`restart_leaves_no_orphans_and_registry_stays_honest`** — kill -9 then
  reboot on the same `$HOME`: nothing survives the generation boundary and
  the new kernel's registry doesn't claim dead jobs are `[running]`.
- **`agent_auth_production_path`** — the auth lane real clients use:
  kaijutsu-mcp/-acp authenticate via `KeySource::Agent`, which no other
  test touches. A real ssh-agent runs in the namespace, receives the
  ephemeral key over the agent protocol, and a background job round-trips
  through an agent-authenticated session.
- **`bg_job_survives_client_disconnect`** — pins the documented contract: a
  background job outlives the client connection that started it. The test
  *observes* the SSH transport closing (`/proc/net/tcp`) before judging, so
  a pass is not vacuous. (A code-reading hypothesis that PDEATHSIG bound the
  job to the per-connection thread was disproven by this test, 2026-08-09.)

## Honest limits

- PDEATHSIG fires when the spawning **thread** dies. `kill -9` takes every
  thread, so the dev-loop scenario is covered. The inverse footgun — a
  healthy kernel whose spawning thread exits early, spuriously killing a
  job — is bounded by `bg_job_survives_client_disconnect` (the one plausible
  early-exit, client disconnect, provably doesn't kill the job) but a
  targeted thread-death test isn't possible from outside the process.
- The container shares the host kernel; this validates *our* process
  hygiene, not kernel-level containment.
- Boot noise: the embedded `mcp.toml` tries to launch `bevy_brp_mcp` and a
  hardcoded kaibo path; both fail loudly and harmlessly in the container.

## Slice 2 (planned)

Filesystem-protection tests: read-only mounts surfacing as clean errors (not
corruption), the filesystem-root walk refusal (`ce0a4146`), symlink-escape
probes against the VFS. Same harness, new test file.

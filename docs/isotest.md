# isotest — containerized isolation & process-lifecycle tests

`contrib/isotest` runs functional tests that need a **real kernel restart**:
in-process integration suites (`kaijutsu-server/tests/`) cannot replace the
server they live inside. The harness checks durable shell-operation receipts,
completion, and restart handling as regular, repeatable assertions.

## Why a container

Not for security — for **observability and blast radius**:

- The podman PID namespace starts empty, so a restart loop and hostile
  filesystem layout cannot touch the host or a live kernel.
- `--network=none` (loopback only), `--pids-limit` (a fork storm fails the
  test, not the machine), tmpfs `$HOME`. All rootless-friendly; no privileges.

## Running

```sh
contrib/isotest                  # the kaijutsu-isotest suite
contrib/isotest shell_operation  # filter by test name
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
after touching shell-operation or lifecycle paths. CI eventually.

## Topology

catatonit (podman `--init`, PID 1) → test binary → spawns `kaijutsu-server`
as a real child on a fresh tmpfs `$HOME` → connects over loopback SSH with
`kaijutsu-client` → joins the genesis ROOT context (a director: `exec` +
`facade:shell` already granted) → drives the `shell` tool → restarts the
server → reads the durable operation state.

**Credentials: always ephemeral, always labeled.** Every key the suite
mints is generated fresh per boot and carries the `isotest-ephemeral` label
in the pubkey comment and the SSH username — a stray entry can never be
mistaken for a durable identity. `auth.db` carries no name of its own
(`docs/character.md`, "`auth.db` is a keyring"), so registration binds the
key to the server's own seeded `hajime` character: the harness boots the
server first (which seeds `kernel.db`), waits for it to appear, then runs
`add-key --as hajime` while the server keeps running (safe — `auth.db` is
WAL and never cached). The shipped binary has `allow_anonymous: false` and
no registration RPC, so this is still the only way in. Most tests
authenticate with an in-memory key; the agent test runs a real `ssh-agent`
inside the namespace and injects the key via the agent protocol, so that
private key never touches disk at all.

Tests run `--test-threads=1` (the runner enforces it): lifecycle assertions
must never interleave.

## What it pins (crates/kaijutsu-isotest/tests/isolation.rs)

- **Shell-operation restart and scope coverage** — an asynchronous call has a
  durable receipt and context-owned kaish job. A restarted kernel records an
  unfinished operation as abandoned instead of claiming it still runs; another
  context cannot read, wait for, or cancel that receipt.
- **Whole-program completion coverage** — `foreground: false` returns before
  the complete kaish program finishes, then settles its separate output block
  and terminal envelope. `foreground: true` retains the completed-result
  contract.
- **`agent_auth_production_path`** — the auth lane real clients use:
  kaijutsu-mcp/-acp authenticate via `KeySource::Agent`, which no other
  test touches. A real ssh-agent runs in the namespace, receives the
  ephemeral key over the agent protocol, and an asynchronous shell operation
  round-trips
  through an agent-authenticated session.
- **Client-disconnect coverage** — a context-owned kaish job and its durable
  operation receipt outlive the client connection that submitted them.

## Honest limits

- A restarted kernel cannot retain an in-memory kaish job manager. The durable
  registry records unfinished operations as abandoned; it does not claim that
  a job can continue across the restart.
- The container shares the host kernel; this validates operation lifecycle
  handling, not kernel-level containment.
- Boot noise: the embedded `mcp.toml` tries to launch `bevy_brp_mcp` and a
  hardcoded kaibo path; both fail loudly and harmlessly in the container.

## Slice 2 (planned)

Filesystem-protection tests: read-only mounts surfacing as clean errors (not
corruption), the filesystem-root walk refusal (`ce0a4146`), symlink-escape
probes against the VFS. Same harness, new test file.

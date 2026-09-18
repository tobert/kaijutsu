# isotest — containerized process and filesystem tests

`contrib/isotest` runs functional tests that need an isolated process table or
a real filesystem. The process suite starts a real server and connects over
SSH. The filesystem suite builds a production kernel in the test process and
calls its native file tools through the broker.

## Why a container

The container provides observability and limits the effect of a failed test:

- The Podman PID namespace starts empty, so every child process belongs to the
  test and a survivor is visible.
- The test can create hostile files and symlinks without touching the
  developer's filesystem.
- `--network=none` allows loopback only, `--pids-limit` bounds process growth,
  and `$HOME` is a fresh tmpfs. The suite needs no privileges.

## Running

```sh
contrib/isotest                  # the kaijutsu-isotest suite
contrib/isotest --test isolation # process lifecycle only
contrib/isotest --test filesystem
contrib/isotest --keep           # keep the exited container for inspection
contrib/isotest --pull           # refresh the Arch base image
contrib/isotest -p kaijutsu-kernel --test broker_e2e
                                 # containerize another workspace test target
```

The runner discovers executables from `cargo test --no-run
--message-format=json`, builds them on the host, and bind-mounts them read-only.
`contrib/Containerfile.isotest` supplies a compatible userland; an `ldd`
preflight fails on a glibc mismatch. The runner sets `--test-threads=1` because
the lifecycle assertions share the container's process table.

## Process lifecycle

`crates/kaijutsu-isotest/tests/isolation.rs` runs this topology:

```text
catatonit (PID 1)
  └─ isotest
       └─ kaijutsu-server
            └─ external command
```

The harness initializes a fresh `$HOME`, starts `kaijutsu-server`, connects
over loopback SSH with `kaijutsu-client`, and joins the root character's root
context. It submits work through the retained shell RPC. `kj wait --operation`
observes the durable operation and job attachment. The unshadowed
`cancel_shell_operation` broker tool remains reachable as a kaish command for
operation-scoped cancellation. The empty PID namespace supplies the final
process-liveness observation.

Every credential is generated for one boot and labeled `isotest-ephemeral` in
the public-key comment and SSH username. The harness runs
`kaijutsu-server init --as tester --key <pubfile>` before starting the server,
which creates the root character and binds the key. Most tests authenticate
with an in-memory key. The agent-auth test starts a real `ssh-agent` in the
namespace and adds the ephemeral private key through the agent protocol.

The suite checks:

- SIGKILL and SIGTERM leave no external child alive.
- operation cancellation reaps an external process group and its children.
- restart marks unfinished durable work as no longer running.
- context-owned work survives client disconnect.
- the production SSH-agent authentication path can submit external work.

## Filesystem protection

`crates/kaijutsu-isotest/tests/filesystem.rs` calls
`kaijutsu_server::rpc::create_shared_kernel`, the same builder that supplies the
server's mount topology. `/` is read-only and `/tmp` is writable. The fixture
revokes `exec`, grants only the native `builtin.file` tools under test, and
dispatches each call through the production broker with an explicit
cancellation token. No subprocess is needed for a file-tool call.

The suite checks:

- writes to a read-only mount fail without changing disk or cached content;
- `glob` and `grep` refuse a walk rooted at `/`;
- a symlink cannot escape its mount for reads, writes, or directory walks;
- a symlink that stays within its mount continues to work.

The probes use the container's real filesystem. This exercises production
`LocalBackend` mount and canonicalization behavior instead of a synthetic VFS.

## Limits

- The container shares the host's OS kernel. These tests cover Kaijutsu
  process and VFS behavior, not OS-level containment.
- On Linux, PDEATHSIG covers a direct child of the server thread that spawned
  it. The process-group cancellation test separately covers child processes
  created by an external command.
- The in-process filesystem suite does not exercise SSH transport. It uses the
  production kernel builder, broker, native file server, mount table, and file
  cache directly.

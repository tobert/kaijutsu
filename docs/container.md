# Container runtime

The Kaijutsu runtime image contains `kaijutsu-server`, `kaijutsu-mcp`, and
`kaijutsu-acp`. It does not contain the Bevy app, Rust, Git, ALSA, or PipeWire.
It runs as the non-root `kaijutsu` user (UID/GID 10001).

Alpine's BusyBox `/bin/sh` remains an operating-system dependency for the
container image. It is not an alternate Kaijutsu command executor: every
Kaijutsu shell request runs through kaish. Do not remove the deployment shell
on that basis or treat the runtime image as a general development environment.

## Build

```bash
podman build --tag localhost/kaijutsu:dev --file Containerfile .
```

## Rootless Podman on zorak

Prepare the two host directories as the account that runs Podman:

```bash
mkdir -p /tank/kaijutsu/kernel-data /tank/kaijutsu/workspace
```

Run the kernel with the host account mapped to the image's `kaijutsu` user:

```bash
podman run --detach --name kaijutsu \
  --userns=keep-id:uid=10001,gid=10001 --user kaijutsu \
  --init \
  --cap-drop=all --security-opt=no-new-privileges \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,nodev,size=1g \
  --publish 2222:2222 \
  --mount type=bind,src=/tank/kaijutsu/kernel-data,dst=/var/lib/kaijutsu \
  --mount type=bind,src=/tank/kaijutsu/workspace,dst=/workspace \
  localhost/kaijutsu:dev
```

`keep-id:uid=10001,gid=10001` maps the user running rootless Podman to the
container's `kaijutsu` account. The directories above should remain owned by
that host user; do not `chown` them to host UID 10001. The container is
non-root and can only see the mounts it receives, but it is not a separate
host Unix identity. A later service owned by a host `kaijutsu` account is the
right next step if that stronger boundary is needed.

The server stores its host key, auth database, config, and kernel data under
`/var/lib/kaijutsu`. The workspace is intentionally separate and is the only
agent-writable project mount. Mount provider credentials and other secrets
independently and read-only; do not put them in either directory.

## Add the first SSH key

The server refuses to start until `init` has created a root character and
bound its key (`docs/character.md`, "Bootstrap: the person creates
themself"). Run `init` before the first start; the public-key mount need not
persist.

```bash
podman run --rm \
  --userns=keep-id:uid=10001,gid=10001 --user kaijutsu \
  --mount type=bind,src=/tank/kaijutsu/kernel-data,dst=/var/lib/kaijutsu \
  --mount type=bind,src="$HOME/.ssh/id_ed25519.pub",dst=/keys/operator.pub,ro \
  --entrypoint /usr/local/bin/kaijutsu-server \
  localhost/kaijutsu:dev init --as operator --key /keys/operator.pub
```

`init` writes `kernel.db`, so run it while the server is stopped. To bind more
keys later, `add-key <pubkey-file> --as <character>` is safe while the server
runs: `auth.db` is WAL and never cached.

## Configure a model backend

A new kernel has no model backends and no default model, so a model turn
fails with how to add one. Configure one from a client's shell after the
first start:

```bash
kj backend set deepseek --kind deepseek --api-key-env DEEPSEEK_API_KEY
kj backend default set --backend deepseek --model deepseek-flash
```

The key reaches the kernel through the container's environment or a file
the row names with `--api-key-file`; the row stores only the variable name or
the path. README.md, "Quick Start" has more examples.

## Devcontainer

`.devcontainer/devcontainer.json` builds the `devcontainer` stage from the
same `Containerfile`. It has Rust and Git for development and includes the
three Kaijutsu binaries. Its named `kaijutsu-dev-data` volume persists kernel
state across rebuilds without exposing a host directory.

Inside the devcontainer, source edits use `vscode`. Start a test kernel as the
runtime user when you want its durable state in that volume:

```bash
sudo -u kaijutsu env \
  HOME=/var/lib/kaijutsu \
  XDG_CACHE_HOME=/var/lib/kaijutsu/cache \
  XDG_CONFIG_HOME=/var/lib/kaijutsu/config \
  XDG_DATA_HOME=/var/lib/kaijutsu/data \
  XDG_RUNTIME_DIR=/tmp/kaijutsu-runtime \
  kaijutsu-server --port 2222
```

## Rootless Podman as a systemd user unit

`contrib/kaijutsu.container` is a Quadlet unit with the same isolation flags
as the run command above. State lives in `~/.local/share/kaijutsu-container`;
`~/Downloads` is mounted read-write at `/downloads` and passed as
`--rw-mount`. SSH publishes on `127.0.0.1:2223` so it does not collide with a
host kernel on 2222.

```bash
mkdir -p ~/.local/share/kaijutsu-container/{kernel-data,workspace}
# run `init` once (above), then:
cp contrib/kaijutsu.container ~/.config/containers/systemd/
systemctl --user daemon-reload && systemctl --user start kaijutsu
```

Add `loginctl enable-linger $USER` to start it without a login session.

An HTTP MCP server on the host's tailnet address works from the container
with default networking (pasta). The host's loopback and LAN addresses are
refused from inside; use the tailnet name in `mcp.toml`.

## Agent development container

Built on zorak on 2026-10-18 for nagaya; "As built" below lists where it differs from the plan. One container holds one
project's kernel and workspace. A DeepSeek coder seat works there with no
asks. A Claude session outside supervises it over MCP. Each instance takes
the project's name, written `<project>` below.

### As built (2026-10-18, zorak, nagaya)

The files are in `contrib/agent-dev/`: `build.sh` and `Containerfile` (the image), `kaijutsu-dev@.container` (a
Quadlet template; the instance name is the project), `bootstrap.sh <project>` (the kernel), and the `rc/`
overlay.

- **Image.** `localhost/kaijutsu-agent-dev` builds on `localhost/arch-rocm` from `~/src/homelab-arch-containers`,
  which holds zorak's exact package versions. Thus the host's release binaries of the kernel, its bridges and kaibo
  run in it unchanged, and the build needs no network. The image label names the kaijutsu commit; `build.sh`
  refuses a tree with uncommitted changes under `crates/` or `assets/`.
- **Mounts.** `~/src` is read-only at its host path and `~/src/<project>` is read-write over it, so the project
  repository is the seat's working tree and other projects are there to read. `/workspace` is a scratch
  repository for small tasks. Two private keys under `~/src` are masked with `/dev/null`.
- **Build cache.** `~/kj-dev/<project>/cargo-target` mounts at `~/.cache/kaijutsu/cargo-target`, the
  `CARGO_TARGET_DIR` that the default rc gives each context. The root file system is read-only, so without the
  volume cargo cannot write there. `check.sh <project>` probes each path that a seat writes.
- **Models.** `/tank/ml/models` mounts read-only at the same path, so GPU tests read weights where the
  engine's own docs name them. `check.sh` probes it.
- **Seats.** banto plays a `director` context on `anthropic/claude-haiku-5-5` in the project. Its senpai is the
  supervising Claude session's context, whose holder connects over MCP with its own key (`~/.ssh/kaijutsu-senpai`).
  banto's asks go to senpai, and a drift between banto and senpai (`kj senpai report`) needs no ask. banto starts
  coder turns with `kj drive`; a drift does not start a turn, and the drift wake script is not written.
- **Gate.** Coders run with `uncovered = "allow"`. The `mcp` type does too, so the supervisor's `kj` verbs do not
  wait on Amy. `root` allows `kj kohai create`, which MCP registration runs under Amy's root.
- **CPU.** `--cpuset-cpus=4-31 --cpu-shares=512`, so cores 0-3 stay free for the host. Pinning needs the `cpuset`
  controller, which systemd does not delegate to a user session by default. On zorak,
  `/etc/systemd/system/user@.service.d/delegate.conf` sets `Delegate=cpu cpuset io memory pids`; it took effect
  after `systemctl daemon-reload`, with no new login.
- **kaibo** runs from `~/src/kaibo` at release, with `--cast crusoe`. Its own config is the host's, read-only,
  through `XDG_CONFIG_HOME`; the container's XDG paths point at kernel state.
- **Git** in the container uses `GIT_CONFIG_GLOBAL` in kernel state, with the host user's name and email. The
  host's gitconfig requires git-lfs, which the image lacks.
- **Smoke test.** banto answered on Haiku and listed `/exchange/to-seat`. The coder got its `AGENTS.md` as house
  rules (`S36-house-rules.kai`), saw `gfx1151` with `rocminfo`, wrote in the project, and was refused in another
  repository under `~/src`.

### Shape

- **The kernel runs inside the container.** A context's shell runs host
  programs where the kernel runs (`ExternalExec::Allow`, see
  `docs/kaish-integration.md`), so the container is the boundary for
  everything the seat runs. A host kernel that executes inside a container
  would need a new execution path.
- **A separate `agent-dev` image stage.** The runtime image stays minimal.
  The agent image starts from Arch, which matches the host's glibc and ROCm
  packages. It holds rustup, Git, kaibo, ROCm/HIP userspace, and the static
  release binaries from the `build` stage. A fresh image is a fresh release
  build.
- **Host layout.** `~/kj-dev/<project>/` holds `kernel-data/`,
  `workspace/`, `cargo/` (registry and git cache), and `secrets/`. The
  secrets are the DeepSeek key, the Crusoe key, and kaibo's config, each
  mounted read-only.
- **Shared files.** `~/kj-dev/<project>/claude-workspace/` is the
  supervisor's working directory, with its `goal.md` and `signoff.md`.
  Its `exchange/` subdirectory mounts read-write at `/exchange`, with
  `to-seat/`, `to-supervisor/`, and `digest/` inside. The server starts
  with `--rw-mount /exchange` so a context's file tools can write there.
- **SSH** publishes on `127.0.0.1:2224`. The Quadlet above uses 2223 and
  the host kernel uses 2222.
- **Network.** Default rootless networking (pasta) with open egress, so
  the seat can fetch crates, clone repositories, and read docs. No SSH key
  or forge token enters the container: the seat commits in `/workspace`
  and a person pushes from the host.
- **Resources on zorak** (32 threads, 125 GiB, Strix Halo):
  `--cpuset-cpus=4-31`, `--memory=64g`, a lower CPU weight, and a high
  `--pids-limit` for cargo. The GPU takes its memory from the same RAM the
  host's llama servers use. Whether the container's memory limit bounds GPU
  allocations is not yet measured. Heavy GPU jobs take
  `~/.cache/zorak-heavy.lock`, as long council runs do.
- **GPU.** `--device /dev/kfd --device /dev/dri --group-add keep-groups`,
  in a per-host drop-in so the base unit carries no vendor flags. An NVIDIA
  host uses CDI (`--device nvidia.com/gpu=all`) in its own drop-in.

### Seat, gate, and supervision

- Characters: a `<project>-coder` performer on the `deepseek-flash` model,
  and `banto`. Each coder context is a kohai of banto's seat, so its asks reach
  banto, which is driving the loop; banto's seat is a kohai of the supervising
  Claude session's context. The bootstrap creates both with `kj kohai create`.
  In the first runs the supervising Claude session outside the container can
  hold the banto seat itself and drive the coder seats over MCP. Later a model
  banto in the kernel's `director` seat drives them the same way.
- `gate.toml` sets `[context_type.coder] uncovered = "allow"` and
  `allow = ["shell_write"]`. Explicit `ask` keys in `[global]` still fire,
  so the file needs an audit for this kernel. The council stays off:
  Jev bumps need a council port to a chat backend
  (`docs/issues.md`, "Council presets per context").
- Recovery is Git. The seat commits often and never pushes.
- kaibo runs inside as an `mcp.toml` stdio server with the `crusoe` cast
  (GLM-5.3), with `/workspace` as its allowed tree.
- The supervisor (banto in the first runs, senpai once banto is a `director` seat): a dedicated Claude Code session with its own
  `kaijutsu-dev` MCP entry. Its `kaijutsu-mcp` comes from the same commit
  as the container, because the wire version must match. It drives the
  seat with `kj drive` and `kj wait`, rotates the seat's context with a
  handoff when it fills, and writes a daily digest into
  `~/kj-dev/<project>/`: spend, cache hit ratio, commits, and open
  problems.
- A person watches with `kaijutsu-tui` attached to port 2224.
- The coder and banto (the `director` seat) each get an environment
  instruction from `contrib/agent-dev/rc/<type>/create/S12-environment.md`,
  installed into this kernel's host rc next to the shipped defaults. It
  names `/workspace`, `/exchange`, and the kernel's own state, so a seat
  does not spend calls finding them. The text names no project and does not say who the
  senpai is: the seat reads that from the `<senpai>` fact in its `<situation>` block.
- A drift that arrives at a coder seat starts its turn, through a coder
  `drift` rc script on this kernel only (`docs/drift-ux.md`, gap 2, shape
  B). The identity problem that section names is checked against current
  code before the script ships.

### Repeating it

Everything project-specific is a name and a directory:
`~/kj-dev/<project>/`, the container name, the `kaijutsu-dev` MCP entry,
and the supervisor's goal. The image, the Quadlet template, the GPU
drop-in, the rc overlay in `contrib/agent-dev/`, and the bootstrap script
take the name as an argument and stay the same across projects and hosts.
The likely end state is one long-lived kernel in one such container, with
each project as a workspace inside it, so nothing here should assume one
project per kernel where a parameter costs nothing.

### Before the first run

1. DeepSeek provider fixes: retry transient failures mid-stream where no
   durable block was written, treat `insufficient_system_resource` as
   transient and `content_filter` as an error, make HTTP 402 permanent,
   and decode an in-stream `error` chunk.
2. Raise the DeepSeek backend's request and idle timeouts on this kernel.
   The 600 s request timeout covers the whole stream, which a long
   reasoning turn can exceed.
3. Smoke test with `fleet/contained/live/council-cleanup.toml`
   (`docs/acp-fleet.md`, "Live scenarios").
4. Check that the request prefix is byte-stable across turns with
   `KJ_LLM_REQUEST_DUMP` (`docs/telemetry.md`).
5. The `agent-dev` stage, a `kaijutsu-dev.container` Quadlet, and the GPU
   drop-in.
6. A bootstrap script: `init`, `add-key` for the supervisor,
   `kj backend set`, the characters and their contexts (`kj kohai create`), `gate.toml`, and `rc reseed`.
7. A standalone test task: a one-page spec for a small Rust crate,
   acceptance tests the seat cannot see, a kaibo review step, and a step
   that writes `rocminfo` output to `GPU.md`. The supervisor runs the
   acceptance tests in a fresh container. The task stays as a regression
   check for the environment itself.

### Open

- Whether `--memory` bounds GPU memory on Strix Halo.
- Whether an ask can be answered through `kaijutsu-mcp`, so the supervisor
  can act as reviewer without a tui.
- The 120 s idle timeout default and whether DeepSeek's `: keep-alive`
  comments reset it.
- Whether the broker's tool list order is stable across turns.

## Kubernetes later

For `/tank` storage pinned to zorak, use statically created `local`
PersistentVolumes, not `hostPath`: one PV for `kernel-data` and one for the
workspace. Give each PV `nodeAffinity` selecting
`kubernetes.io/hostname=zorak`, bind them through claims, and schedule the
workload with the same `nodeSelector`. Give the StorageClass
`volumeBindingMode: WaitForFirstConsumer`; do not set `spec.nodeName`, because
that bypasses the scheduler and leaves a waiting claim pending. Local storage
means the kernel is unavailable when zorak is unavailable, which is the
intended first self-hosting tradeoff.

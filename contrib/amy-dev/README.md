# amy-dev

Amy's development workspace: one rootless container per machine, with Arch, rustup, the ALSA and PipeWire client
libraries, Claude Code, tmux, sshd, and the GPU userspace for that machine. Connect over SSH on port 2230, start
tmux, and work in `~/src` at its host path. The host carries no build state. `docs/container.md`, "amy-dev", has the
design.

## Files

- `Containerfile`: the image. Arch base pinned to a dated tag, the user from `--build-arg`, and the GPU userspace
  chosen by `--build-arg GPU=`.
- `entrypoint.sh`: starts sshd unprivileged, generating host keys into the state volume on first start.
- `sshd_config`: keys only, no root, no PAM. The user is the host user through `keep-id`.
- `amy-dev.container`: the Quadlet unit: mounts, audio, and ports. Identical on every machine.
- `machines/<host>.env`: the machine's `GPU` value, read by `build.sh`.
- `machines/<host>.conf`: the machine's GPU devices, installed as a drop-in.
- `build.sh`: `podman build` with the host user's name and UID, and the GPU from `machines/$(uname -n).env`
  or `--gpu`.

## Setup

```bash
mkdir -p ~/.local/share/amy-dev/{state,rustup,cargo} ~/.config/amy-dev
cp ~/.ssh/id_ed25519.pub ~/.config/amy-dev/authorized_keys    # the keys you want allowed
contrib/amy-dev/build.sh                                       # or: build.sh --gpu none
cp contrib/amy-dev/amy-dev.container ~/.config/containers/systemd/
mkdir -p ~/.config/containers/systemd/amy-dev.container.d
cp contrib/amy-dev/machines/<host>.conf ~/.config/containers/systemd/amy-dev.container.d/gpu.conf
systemctl --user daemon-reload && systemctl --user start amy-dev
```

Connect and start a session:

```bash
ssh -p 2230 <host>          # from this host: ssh -p 2230 127.0.0.1
tmux new -As main
rustup default stable       # first run only; toolchains land in ~/.local/share/amy-dev/rustup
claude                      # first run asks for a login
```

Skip the drop-in on a machine with no GPU. `build.sh` needs a profile in `machines/` or `--gpu`.

## GPU

| Machine | `GPU` | Devices (drop-in) | Userspace in the image |
|---|---|---|---|
| zorak (Strix Halo, gfx1151) | `rocm` | `/dev/kfd`, `/dev/dri` | `hip-runtime-amd`, `rocminfo`, `rocm-smi-lib`, `rocwmma` |
| tenchi (DGX Spark, NVIDIA) | `nvidia` | `nvidia.com/gpu=all` (CDI) | `cuda`; the driver comes through CDI |
| Intel Arc host | `intel` | `/dev/dri` | `intel-compute-runtime`, `level-zero-loader`, `intel-graphics-compiler`, `clinfo`; no profile yet |

The kernel driver is the host's, so the userspace must match the host's kernel. The `none`, `rocm`, `intel`, and
`nvidia` images have built on x86_64 here, and each has its tools on the PATH. Nothing has run on the GPU yet.

## Host upgrades

A host upgrade can change the kernel driver's interface, so after one:

1. Rebuild: `contrib/amy-dev/build.sh`, then restart the unit.
2. Check the GPU and the audio from inside: `rocminfo` (rocm), `clinfo -l` (intel), or `nvidia-smi` (nvidia), plus
   `aplay -l` and `amidi -l`.
3. If the GPU check fails after a rebuild, the kernel driver is the cause. Roll the host kernel back rather than
   chase it in the container.

Images are tagged `<commit>-<gpu>` and `<gpu>`. The previous tag stays, so the Quadlet can point back to it.

## Caches

Each build mounts the host's pacman package cache (`--mount=type=cache` on `/var/cache/pacman/pkg`). Builds on one
host share it, so a second build downloads only the packages it is missing. The cache lives in the build tool's
store, not in the image. Builds on different machines do not share it.

## Audio and MIDI

`/dev/snd` is passed through, and the container user is in the `audio` group. ALSA PCM and the MIDI sequencer
(`/dev/snd/seq`) work without the host's PipeWire. pawlsa-mcp's PipeWire tools need the host daemon's socket,
which the Quadlet has commented out until PipeWire runs on the host.

## Secrets

Not decided. Claude Code's login state is bind-mounted from `~/.claude` and `~/.claude.json`, read-write. No SSH
private key or forge token is mounted. Provider keys and the rest are deferred.

## Open

- **tenchi is aarch64.** It is the DGX Spark. `BASE` is an x86_64 Arch image, so tenchi needs an aarch64 base before
  its build works.
- **The Intel host is unnamed.** It has no `machines/` profile yet. Add one with its `uname -n` value.
- **Built only on x86_64 here.** The `zorak` profile's `rocm` image has not run on zorak. `CLAUDE_CODE_VERSION`
  defaults to `latest`; pin it with `--build-arg`.
- **Not run.** The sshd entrypoint, the tmux session, and the GPU checks have not been exercised in a container.
- **Port.** SSH publishes on loopback only. Reaching it from another machine needs the bind address changed.

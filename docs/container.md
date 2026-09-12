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

The server starts with an empty authorization database. Start it once first —
that seeds `kernel.db` and the bootstrap character, `hajime`
(`docs/character.md`, "Bootstrap: `hajime`") — then bind a key to it; the
public-key mount need not persist.

```bash
podman run --rm \
  --userns=keep-id:uid=10001,gid=10001 --user kaijutsu \
  --mount type=bind,src=/tank/kaijutsu/kernel-data,dst=/var/lib/kaijutsu \
  --mount type=bind,src="$HOME/.ssh/id_ed25519.pub",dst=/keys/operator.pub,ro \
  --entrypoint /usr/local/bin/kaijutsu-server \
  localhost/kaijutsu:dev add-key /keys/operator.pub --as hajime
```

`add-key` is safe to run while the server keeps running — `auth.db` is WAL
and never cached — but it needs `kernel.db` to already carry a character to
bind to.

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

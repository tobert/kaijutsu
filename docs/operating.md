# Operating the live kernel and container runtime

## The systemd service

 zorak runs the kernel as a systemd **user** unit from `target/debug/kaijutsu-server`:

```bash
systemctl --user restart kaijutsu-server.service   # ~30 s to accepting
systemctl --user status kaijutsu-server.service
```

Environment variables need a service restart, while `kj backend set` and config trees hot-reload. Before bouncing, check `kj system status` for pending asks and in-flight turns.

## Deploy workflow

1. Build: `cargo build -p kaijutsu-server`
2. Stop: `systemctl --user stop kaijutsu-server.service`
3. Back up database (`kernel.db`, `-wal`, `-shm`) and `auth.db` (`../auth.db*`) to `~/.local/share/kaijutsu/kernel/backups/`.
4. Start: `systemctl --user start kaijutsu-server.service`

Reseed rc trees with `kaijutsu-server rc reseed` while stopped (`--force` overwrites defaults or moves retired trees aside).

## The macOS LaunchAgent

On macOS, `contrib/install-launchd.sh` builds a release binary, copies it to `~/bin/kaijutsu-server`, and runs it as the LaunchAgent `local.kaijutsu-server`, logging to `~/Library/Logs/kaijutsu-server.log`. Re-running it deploys a new build. Bootstrap the kernel first, with the agent stopped:

```bash
launchctl bootout gui/$(id -u)/local.kaijutsu-server            # stop
kaijutsu-server kj -- embedding show                            # any kj verb, offline
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/local.kaijutsu-server.plist
```

The agent's environment holds only `PATH`, `RUST_LOG`, and any `XDG_DATA_HOME` or `XDG_CONFIG_HOME` from the installing shell, so backend keys come from files. launchd restarts a failing server every 5 seconds without limit.

## Rootless Podman and container setup

The Kaijutsu container image (`localhost/kaijutsu:dev`) runs as the non-root `kaijutsu` user (UID/GID 10001).

Run rootless Podman on zorak:

```bash
mkdir -p /tank/kaijutsu/kernel-data /tank/kaijutsu/workspace

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

- **State directory:** `/var/lib/kaijutsu` holds auth, config, and kernel data.
- **Workspace:** `/workspace` is the project working mount.
- **Bootstrap:** Run `bootstrap` with `--backend` and `--api-key-file` when initializing a fresh kernel instance with read-only secret mounts.
- **Quadlet integration:** Systemd user unit configurations in `contrib/kaijutsu.container` manage container lifecycle under `~/.config/containers/systemd/`.

## Agent development image

`localhost/kaijutsu-agent-dev`, from `contrib/agent-dev/`, is the kernel on Arch with Rust, `capnproto`, and
`mold`, so contexts can build and test kaijutsu in the container. Its base is a layer from
`~/src/homelab-arch-containers` at the host's package versions, so release binaries built on the host run in it.

```bash
~/src/homelab-arch-containers/scripts/prime.sh contrib/agent-dev/packages
~/src/homelab-arch-containers/scripts/build.sh arch-base arch-dev
cargo build --release -p kaijutsu-server -p kaijutsu-mcp -p kaijutsu-acp
KJ_DEV_BASE=localhost/arch-dev:latest contrib/agent-dev/build.sh    # default base: localhost/arch-rocm
```

`build.sh` refuses a tree with uncommitted changes under `crates/` or `assets/`, so build from a clean checkout
when others are editing main. `kaijutsu-dev@.container` runs one project's kernel per instance; `bootstrap.sh
<project>` prepares its state, and `check.sh <project>` probes the paths its contexts write.

### Binary updates and rollback

The entrypoint, `kaijutsu-run`, runs the kernel from `/var/lib/kaijutsu/bin/current/`, which it seeds from the
image on first start. To update without a new image, copy the binaries into `bin/staged/` and restart the unit:

```bash
bin=~/kj-dev/<state>/kernel-data/bin
mkdir -p $bin/staged
cp target/release/{kaijutsu-server,kaijutsu-mcp,kaijutsu-acp} $bin/staged/
git rev-parse --short HEAD > $bin/staged/COMMIT
systemctl --user restart <unit>
```

At start, a staged `kaijutsu-server` is promoted: `current/` becomes `previous/` with a copy of `kernel.db` in
`previous/db/`, and `staged/` becomes `current/`. Each start logs the set's commit and each binary's sha256.
A promoted set is on trial until one run lasts 60 s (`KJ_TRIAL_SECONDS`). If it exits nonzero sooner three times
in a row (`KJ_TRIAL_CRASHES`), the wrapper moves it to `bin/failed-<time>/` and restores `previous/`, and the
unit's restart runs the restored set. Rollback does not restore the database; the copy is in `current/db/`.
A set off trial is never rolled back, and a stop by SIGTERM never counts as a crash.

To roll back by hand, stop the unit, move `current/` aside, rename `previous/` to `current/`, and start it.

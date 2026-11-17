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

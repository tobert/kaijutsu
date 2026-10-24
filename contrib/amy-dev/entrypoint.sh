#!/bin/sh
# Start sshd for the host user. Runs unprivileged (podman keep-id), so sshd accepts only that account.
# Host keys and the pid file live in the state volume, so the fingerprint survives restarts.
set -eu

state=/var/lib/amy-dev
mkdir -p "$state/ssh" "$XDG_RUNTIME_DIR"
chmod 0700 "$state/ssh"

if [ ! -f "$state/ssh/ssh_host_ed25519_key" ]; then
  ssh-keygen -q -t ed25519 -N '' -f "$state/ssh/ssh_host_ed25519_key"
fi

exec /usr/sbin/sshd -D -e \
  -f /etc/amy-dev/sshd_config \
  -h "$state/ssh/ssh_host_ed25519_key" \
  -o "PidFile=$state/ssh/sshd.pid"

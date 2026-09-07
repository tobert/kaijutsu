#!/usr/bin/env python3
"""Install the audio daemon as a systemd user service on Linux."""

import argparse
from datetime import datetime
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time


UNIT = "kaijutsu-audiod.service"
REPO = Path(__file__).resolve().parent.parent


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def unit_text(args):
    def quote(value):
        if any(ord(c) < 32 or ord(c) == 127 for c in value):
            raise ValueError("service arguments cannot contain control characters")
        value = value.replace("\\", "\\\\").replace('"', '\\"')
        return '"' + value.replace("%", "%%").replace("$", "$$") + '"'

    command = " ".join(quote(str(arg)) for arg in args)
    return f"""[Unit]
Description=Kaijutsu audio and MIDI node
After=network.target sound.target

[Service]
Type=simple
ExecStart={command}
Restart=on-failure
RestartSec=5
TimeoutStopSec=10
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"""


def ensure_key(key, identity):
    public = Path(str(key) + ".pub")
    if key.is_symlink() or public.is_symlink():
        raise RuntimeError(f"refusing symlink key files: {key}")
    if not key.exists():
        if public.exists():
            raise RuntimeError(f"public key exists without private key: {public}; choose another --key")
        key.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        run(["ssh-keygen", "-t", "ed25519", "-N", "", "-C", identity, "-f", str(key)])
    # -P '' refuses encrypted keys without an interactive password prompt.
    derived = run(["ssh-keygen", "-y", "-P", "", "-f", str(key)], capture_output=True).stdout
    if public.exists():
        if public.read_text().split()[:2] != derived.split()[:2]:
            raise RuntimeError(f"public key does not match {key}; existing files were not replaced")
    else:
        public.write_text(derived.strip() + " " + identity + "\n")
    key.chmod(0o600)
    return public


def wait_started(timeout=40):
    invocation = run(["systemctl", "--user", "show", UNIT, "-p", "InvocationID", "--value"],
                     capture_output=True).stdout.strip()
    if not invocation:
        raise RuntimeError("systemd did not start the service")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        logs = run(["journalctl", "--user", f"_SYSTEMD_INVOCATION_ID={invocation}", "--no-pager", "-o", "cat"],
                   capture_output=True).stdout
        state = run(["systemctl", "--user", "show", UNIT, "-p", "SubState", "--value"],
                    capture_output=True).stdout.strip()
        current = run(["systemctl", "--user", "show", UNIT, "-p", "InvocationID", "--value"],
                      capture_output=True).stdout.strip()
        if current != invocation or state in ("failed", "dead", "auto-restart"):
            raise RuntimeError("daemon failed startup:\n" + logs)
        if "audio node running; kernel drives playback" in logs and state == "running":
            print(logs, end="")
            return
        time.sleep(0.5)
    raise RuntimeError("daemon did not report ready within 40 seconds")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True, help="kernel SSH host")
    parser.add_argument("--port", type=int, default=2222, help="kernel SSH port (default: 2222)")
    parser.add_argument("--identity", default=f"audio/{socket.gethostname()}",
                        help="enrolled character and SSH username (default: audio/<hostname>)")
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/kaijutsu-audiod",
                        help="already-built binary (default: target/release/kaijutsu-audiod)")
    parser.add_argument("--key", type=Path, help="private key path (default: ~/.ssh/kaijutsu-audio-<hostname>)")
    parser.add_argument("--enrolled", action="store_true", help="skip enrollment pause; key must already be enrolled")
    parser.add_argument("--no-audio", action="store_true", help="MIDI only")
    parser.add_argument("--no-midi", action="store_true", help="PCM only")
    parser.add_argument("--output", help="exact PCM output device name")
    parser.add_argument("--context", help="existing capture context id or label")
    parser.add_argument("--rt-priority", type=int, default=20, help="best-effort RT priority, 0–99 (default: 20)")
    args = parser.parse_args()
    if not sys.platform.startswith("linux") or os.geteuid() == 0:
        parser.error("run on Linux as the intended service user, not with sudo")
    if not 1 <= args.port <= 65535 or not 0 <= args.rt_priority <= 99:
        parser.error("port must be 1–65535 and RT priority must be 0–99")
    if (args.no_audio and (args.no_midi or args.output)) or (args.no_midi and args.context):
        parser.error("enable at least one backend; --output needs audio and --context needs MIDI")
    binary = args.binary.expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"binary unavailable: {binary}; build with cargo build --release -p kaijutsu-audio-runtime --bin kaijutsu-audiod")
    run(["systemctl", "--user", "show-environment"], stdout=subprocess.DEVNULL)
    if not args.no_midi and not os.access("/dev/snd/seq", os.R_OK | os.W_OK):
        raise RuntimeError("MIDI sequencer unavailable: grant this user access to /dev/snd/seq, or pass --no-midi; no device permissions were changed")
    user_home = Path.home()
    key = (args.key or user_home / ".ssh" / f"kaijutsu-audio-{socket.gethostname()}").expanduser().absolute()
    destination = user_home / ".local/bin/kaijutsu-audiod"
    config = Path(os.environ.get("XDG_CONFIG_HOME", str(user_home / ".config")))
    if not config.is_absolute():
        raise RuntimeError("XDG_CONFIG_HOME must be an absolute path")
    unit = config / "systemd/user" / UNIT
    command = [str(destination), "--host", args.host, "--port", str(args.port),
               "--user", args.identity, "--key", str(key), "--rt-priority", str(args.rt_priority)]
    for name in ("no_audio", "no_midi"):
        if getattr(args, name):
            command.append("--" + name.replace("_", "-"))
    for name in ("output", "context"):
        if getattr(args, name):
            command.extend(["--" + name, getattr(args, name)])
    body = unit_text(command)
    public = ensure_key(key, args.identity)
    print(f"\nPublic key ({public}):\n{public.read_text()}")
    print("On the kernel, create the character through kaish:")
    print(shlex.join(["kj", "character", "create", args.identity]))
    print("Copy only the .pub file to the kernel host, then enroll it there:")
    print(shlex.join(["kaijutsu-server", "add-key", f"/path/to/{public.name}", "--as", args.identity]))
    print("Verify the kernel host key and populate this account's known_hosts before starting.")
    if not args.enrolled:
        if not sys.stdin.isatty():
            raise RuntimeError("key prepared; enroll it, then rerun with --enrolled (service unchanged)")
        if input("Enter 'start' after enrollment and host verification: ").strip() != "start":
            raise RuntimeError("key prepared; service unchanged")
    if unit.exists():
        backup = unit.with_name(UNIT + datetime.now().strftime(".%Y%m%d%H%M%S%f.bak"))
        shutil.copy2(unit, backup)
        print(f"Previous unit saved to {backup}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    unit.parent.mkdir(parents=True, exist_ok=True)
    # Replace the inode so an already-running executable is not overwritten.
    with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as staged:
        staged_path = Path(staged.name)
    try:
        shutil.copyfile(binary, staged_path)
        staged_path.chmod(0o755)
        staged_path.replace(destination)
    finally:
        staged_path.unlink(missing_ok=True)
    unit.write_text(body)
    run(["systemctl", "--user", "daemon-reload"])
    run(["systemctl", "--user", "enable", UNIT])
    run(["systemctl", "--user", "restart", UNIT])
    try:
        wait_started()
    except Exception:
        run(["systemctl", "--user", "stop", UNIT])
        print("Service stopped after failed startup; it remains enabled. Fix the logged error and rerun.", file=sys.stderr)
        raise
    print(f"Installed and running: {UNIT}\nLogs: journalctl --user -u {UNIT} -f")
    print("No device permissions, RT limits or login lingering were changed.")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, ValueError, OSError, subprocess.CalledProcessError, EOFError) as error:
        print(f"Setup failed: {error}", file=sys.stderr)
        sys.exit(1)

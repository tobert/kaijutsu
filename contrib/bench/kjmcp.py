#!/usr/bin/env python3
"""Run `kj` commands against a kaijutsu kernel over the MCP stdio bridge.

There is no standalone `kj` binary: every `kj` verb runs inside the kernel,
reached over SSH. `kaijutsu-mcp --connect` is the scriptable way in — it
auto-registers a session context at startup, so its `shell` tool can carry a
`kj` command.

Usage:
    kjmcp.py --port 22722 --key-file KEY 'kj character create bench-coder' ...

Each argument after the flags is one command. They run in order on one
connection. A non-zero exit code from any command fails the whole run, so a
setup script can rely on `set -e`.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys


class McpStdio:
    """The smallest MCP client that can call one tool."""

    def __init__(self, argv: list[str], env: dict[str, str]) -> None:
        self._proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL if os.environ.get("KJMCP_QUIET") else None,
            env=env,
            text=True,
            bufsize=1,
        )
        self._next_id = 0

    def _send(self, payload: dict) -> None:
        assert self._proc.stdin is not None
        self._proc.stdin.write(json.dumps(payload) + "\n")
        self._proc.stdin.flush()

    def call(self, method: str, params: dict | None = None) -> dict:
        self._next_id += 1
        request_id = self._next_id
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        self._send(payload)
        assert self._proc.stdout is not None
        while True:
            line = self._proc.stdout.readline()
            if not line:
                raise RuntimeError(
                    f"kaijutsu-mcp closed stdout while waiting for {method}"
                )
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                # Not protocol traffic (a stray log line) — keep reading.
                continue
            if message.get("id") != request_id:
                continue
            if "error" in message:
                raise RuntimeError(f"{method} failed: {message['error']}")
            return message.get("result", {})

    def notify(self, method: str, params: dict | None = None) -> None:
        payload = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            payload["params"] = params
        self._send(payload)

    def close(self) -> None:
        if self._proc.stdin is not None:
            self._proc.stdin.close()
        try:
            self._proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self._proc.kill()


def tool_text(result: dict) -> str:
    """Flatten an MCP tool result's content blocks into text."""
    parts = []
    for block in result.get("content", []):
        if block.get("type") == "text":
            parts.append(block.get("text", ""))
    return "\n".join(parts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="kaijutsu-mcp")
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--key-file", required=True)
    parser.add_argument("--context", default="bench-admin")
    parser.add_argument(
        "--home",
        help=(
            "HOME for the kaijutsu-mcp child. A throwaway kernel mints a fresh "
            "host key each boot, and the client learns it by trust on first "
            "use into $HOME/.ssh/known_hosts. Point HOME inside the run "
            "directory and that file is the run's, not the operator's."
        ),
    )
    parser.add_argument(
        "--insecure",
        action="store_true",
        help="skip known_hosts verification entirely (throwaway kernels only)",
    )
    parser.add_argument(
        "--timeout-note",
        action="store_true",
        help="print the raw shell envelope instead of stdout alone",
    )
    parser.add_argument("commands", nargs="+")
    args = parser.parse_args()

    argv = [
        args.binary,
        "--connect",
        "--host",
        args.host,
        "--port",
        str(args.port),
        "--key-file",
        args.key_file,
        "--context-name",
        args.context,
    ]
    if args.insecure:
        argv.append("--insecure")

    env = dict(os.environ)
    if args.home:
        home = os.path.abspath(args.home)
        os.makedirs(os.path.join(home, ".ssh"), mode=0o700, exist_ok=True)
        # HOME is what known_hosts resolves against: russh's
        # `known_hosts_path` joins `std::env::home_dir()` with .ssh/known_hosts,
        # and on Unix that reads HOME. The XDG trees are the caller's to set;
        # boot-kernel.sh already exports them.
        env["HOME"] = home
    client = McpStdio(argv, env=env)
    status = 0
    try:
        client.call(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "kaijutsu-bench-admin", "version": "0"},
            },
        )
        client.notify("notifications/initialized")
        for command in args.commands:
            result = client.call(
                "tools/call",
                {
                    "name": "shell",
                    "arguments": {"command": command, "foreground": True},
                },
            )
            text = tool_text(result)
            envelope = None
            try:
                envelope = json.loads(text)
            except json.JSONDecodeError:
                pass
            if args.timeout_note or envelope is None:
                print(f"$ {command}\n{text}")
            else:
                print(f"$ {command}")
                if envelope.get("stdout"):
                    print(envelope["stdout"].rstrip())
                if envelope.get("stderr"):
                    print(envelope["stderr"].rstrip(), file=sys.stderr)
                if envelope.get("status") not in (None, "ok", "done", "completed"):
                    print(f"  status: {envelope['status']}", file=sys.stderr)
            if result.get("isError") or (envelope or {}).get("exit_code") not in (
                None,
                0,
            ):
                status = 1
                break
    finally:
        client.close()
    return status


if __name__ == "__main__":
    raise SystemExit(main())

"""A Harbor agent that runs `kaijutsu-solo-acp` inside the task container.

Harbor's own `acp` agent can launch a `local` distribution -- a command that
already exists in the environment. A task image has never heard of kaijutsu,
so this subclass puts the command there: `install()` uploads the static
binary and the gate policy to `/installed-agent/kaijutsu/`, then hands off to
`AcpAgent.install()`, which installs the runner's Python dependencies and
writes the launcher script that execs our binary.

Select it with an import path:

    harbor run -a kaijutsu_solo_agent:KaijutsuSoloAcp ...

with this directory on `PYTHONPATH`. Options are `--ak key=value`; see
`KaijutsuSoloOptions` and
`harbor agent schema kaijutsu_solo_agent:KaijutsuSoloAcp`.

This module owns the backend and model defaults. `run-harbor.sh`, `job.yaml`
and `agent.json` restate them for readability; the values here are the ones
that take effect when nothing else is given.
"""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import subprocess
import uuid
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Any, override

from pydantic import Field

from harbor.agents.installed.acp import AcpAgent, AcpOptions
from harbor.environments.base import BaseEnvironment

#: Environment variables read when the matching option is not given.
BINARY_ENV = "KAIJUTSU_ACP_BINARY"
GATE_ENV = "KAIJUTSU_ACP_GATE"
MODEL_ENV = "KAIJUTSU_ACP_MODEL"
BACKEND_ENV = "KAIJUTSU_ACP_BACKEND"
RUST_LOG_ENV = "KAIJUTSU_ACP_RUST_LOG"
MAX_TOKENS_ENV = "KAIJUTSU_ACP_MAX_TOKENS"
RC_OVERLAY_ENV = "KAIJUTSU_ACP_RC_OVERLAY"

#: The single owner of these defaults. Every other file defers to them.
DEFAULT_BACKEND_KIND = "deepseek"
DEFAULT_MODEL = "deepseek-v4-flash"
DEFAULT_RUST_LOG = "info"

#: Backends `kaijutsu-solo-acp --backend-kind` accepts in a release build.
SOLO_BACKEND_KINDS = ("anthropic", "deepseek", "openai")

#: `gate_config_path` value that means "ship no gate policy", on purpose.
GATE_NONE = "none"

#: The binary is a static x86_64 build; anything else cannot run it.
REQUIRED_MACHINE = "x86_64"

#: Where a CA bundle lands on the distros Harbor's ACP setup can reach.
#: rustls-platform-verifier carries no roots of its own, so without one every
#: model call fails with a TLS error.
CA_BUNDLE_PATHS = (
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/etc/ssl/ca-bundle.pem",
)


def _split_args(value: list[str] | str | None) -> list[str]:
    """Normalize an extra-arguments option into a list.

    A `--ak` value arrives as one string, so a string is split the way a
    shell would; a list from a job config is taken as written.
    """
    if value is None:
        return []
    if isinstance(value, str):
        return shlex.split(value)
    return [str(item) for item in value]


def _split_model_name(model_name: str | None) -> tuple[str | None, str | None]:
    """Split Harbor's `provider/model` the way Harbor itself does.

    Kept in sync with `harbor.agents.base.BaseAgent._init_model_info`, and
    checked against it in `__init__`, so a change upstream fails loudly here
    instead of quietly sending a model id to the wrong provider.
    """
    if model_name is None:
        return None, None
    if "/" in model_name:
        provider, name = model_name.split("/", maxsplit=1)
        return provider, name
    return None, model_name


def _last_line(text: str | None) -> str:
    """The last non-empty line, stripped.

    Container exec output can carry a leading banner from the runtime -- see
    the README on `podman compose` -- so a one-line answer is read from the
    end, never as the whole buffer.
    """
    lines = [line.strip() for line in (text or "").splitlines() if line.strip()]
    return lines[-1] if lines else ""


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _dir_content_hash(root: Path) -> str:
    """sha256 over `root`'s sorted relative paths and file bytes.

    A stable fingerprint of what gets uploaded: independent of mtimes,
    ownership, and directory-listing order. Only regular files count -- a
    symlink is skipped here the same way kaijutsu-solo-acp itself refuses
    one when applying the overlay (`docs/solo-acp.md`, `--rc-overlay`), so
    this hash can never silently follow one to content outside the tree.
    """
    digest = hashlib.sha256()
    files = sorted(p for p in root.rglob("*") if p.is_file() and not p.is_symlink())
    for path in files:
        relative = path.relative_to(root).as_posix()
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    return digest.hexdigest()


def _git(repo: Path, *args: str) -> str | None:
    """A git answer for `repo`, or None when git cannot supply one."""
    try:
        result = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip()


class KaijutsuSoloOptions(AcpOptions):
    """`--ak` options. Harbor's ACP options stay available underneath."""

    binary_path: str | None = Field(
        default=None,
        description=(
            "Host path to the kaijutsu-solo-acp binary uploaded into the task "
            f"container. Default: ${BINARY_ENV}."
        ),
    )
    gate_config_path: str | None = Field(
        default=None,
        description=(
            "Host path to the gate policy installed with --gate-config, or "
            f"'{GATE_NONE}' to ship none on purpose. Default: ${GATE_ENV}. "
            "A missing or empty value is refused."
        ),
    )
    backend_kind: str | None = Field(
        default=None,
        description=(
            "Provider passed to --backend-kind, one of "
            f"{', '.join(SOLO_BACKEND_KINDS)}. Default: the provider half of "
            f"Harbor's --model, else ${BACKEND_ENV}, else {DEFAULT_BACKEND_KIND}."
        ),
    )
    solo_model: str | None = Field(
        default=None,
        description=(
            "Model id passed to --model. Default: the model half of Harbor's "
            f"--model, else ${MODEL_ENV}, else {DEFAULT_MODEL}."
        ),
    )
    solo_args: list[str] | str | None = Field(
        default=None,
        description=(
            "Extra arguments appended to the kaijutsu-solo-acp command line, "
            "as a list or one shell-quoted string."
        ),
    )
    rust_log: str | None = Field(
        default=None,
        description=(
            "RUST_LOG for the agent process. Its stderr is the runner's "
            f"stderr, so kernel lines land in acp.txt. Default: ${RUST_LOG_ENV}, "
            f"else {DEFAULT_RUST_LOG}. Below 'info' the per-inference token "
            "lines disappear and a run keeps no token record at all."
        ),
    )
    max_tokens: int | None = Field(
        default=None,
        description=(
            "Output token ceiling passed to --max-tokens. Must be a positive "
            f"integer. Default: ${MAX_TOKENS_ENV}, else left off the command "
            "line entirely, which keeps the binary's own default (the "
            "factory ceiling)."
        ),
    )
    rc_overlay: str | None = Field(
        default=None,
        description=(
            "Host path to a local rc overlay directory (see "
            "contrib/bench/rc-variants/*/README.md), uploaded into the "
            "container and passed to --rc-overlay. Default: "
            f"${RC_OVERLAY_ENV}, else left off the command line entirely, "
            "which keeps the seeded rc tree unchanged."
        ),
    )


class KaijutsuSoloAcp(AcpAgent):
    """`kaijutsu-solo-acp`, uploaded into the task container and run over ACP."""

    options_model = KaijutsuSoloOptions
    options: KaijutsuSoloOptions

    REMOTE_DIR = PurePosixPath("/installed-agent/kaijutsu")
    REMOTE_BINARY = REMOTE_DIR / "kaijutsu-solo-acp"
    REMOTE_GATE = REMOTE_DIR / "gate.toml"
    REMOTE_RC_OVERLAY = REMOTE_DIR / "rc-overlay"

    PROVENANCE_FILENAME = "kaijutsu-provenance.json"

    def __init__(
        self,
        binary_path: str | None = None,
        gate_config_path: str | None = None,
        backend_kind: str | None = None,
        solo_model: str | None = None,
        solo_args: list[str] | str | None = None,
        rust_log: str | None = None,
        max_tokens: int | str | None = None,
        rc_overlay: str | None = None,
        **kwargs: Any,
    ):
        self._local_binary = self._require_file(
            binary_path or os.environ.get(BINARY_ENV),
            what="kaijutsu-solo-acp binary",
            option="binary_path",
            env_var=BINARY_ENV,
        )

        gate = gate_config_path
        if gate is None:
            gate = os.environ.get(GATE_ENV)
        if gate is None or not str(gate).strip():
            raise ValueError(
                "No gate policy. Set --ak gate_config_path=<path> or "
                f"${GATE_ENV}, or say --ak gate_config_path={GATE_NONE} to "
                "ship none on purpose. Without a sandbox gate nearly every "
                "command raises an approval ask, the model is told nothing "
                "ran, and the run costs several times the tokens."
            )
        gate = str(gate).strip()
        self._local_gate = (
            None
            if gate == GATE_NONE
            else self._require_file(
                gate,
                what="gate policy",
                option="gate_config_path",
                env_var=GATE_ENV,
            )
        )

        # Harbor's --model reaches every agent as model_name, typically as
        # `provider/model`. Our runner raises when a model is requested and
        # the agent advertises no model-selection mechanism (acp_runner.py,
        # "ACP agent did not advertise a model-selection mechanism"), and
        # kaijutsu advertises none, so both halves become the binary's own
        # flags instead of an ACP request.
        harbor_model = kwargs.get("model_name")
        parsed_provider, parsed_name = _split_model_name(harbor_model)
        if parsed_provider is not None and parsed_provider not in SOLO_BACKEND_KINDS:
            raise ValueError(
                f"Harbor --model names provider {parsed_provider!r}, which "
                "kaijutsu-solo-acp does not know. Known: "
                f"{', '.join(SOLO_BACKEND_KINDS)}. Name one of those, or set "
                "--ak backend_kind= and --ak solo_model= explicitly."
            )

        self._backend_kind = (
            backend_kind
            or parsed_provider
            or os.environ.get(BACKEND_ENV)
            or DEFAULT_BACKEND_KIND
        )
        if self._backend_kind not in SOLO_BACKEND_KINDS:
            raise ValueError(
                f"backend_kind {self._backend_kind!r} is not one "
                "kaijutsu-solo-acp knows. Known: "
                f"{', '.join(SOLO_BACKEND_KINDS)}."
            )
        self._solo_model = (
            solo_model or parsed_name or os.environ.get(MODEL_ENV) or DEFAULT_MODEL
        )
        self._rust_log = rust_log or os.environ.get(RUST_LOG_ENV) or DEFAULT_RUST_LOG
        self._solo_args = _split_args(solo_args)

        # Left off the command line when unset, which is what keeps the
        # binary's own default (the factory token ceiling) in effect -- this
        # module changes no default of its own, only what it passes through.
        max_tokens_raw = max_tokens if max_tokens is not None else os.environ.get(MAX_TOKENS_ENV)
        if max_tokens_raw is None:
            self._max_tokens: int | None = None
        else:
            try:
                self._max_tokens = int(max_tokens_raw)
            except (TypeError, ValueError) as exc:
                raise ValueError(
                    f"max_tokens {max_tokens_raw!r} is not an integer."
                ) from exc
            if self._max_tokens <= 0:
                raise ValueError(
                    f"max_tokens must be greater than zero, got {self._max_tokens}."
                )

        # Left unset, the seeded rc tree is untouched -- this module changes
        # no default of its own here either.
        rc_overlay_value = rc_overlay if rc_overlay is not None else os.environ.get(RC_OVERLAY_ENV)
        if rc_overlay_value is None:
            self._rc_overlay: Path | None = None
            self._rc_overlay_hash: str | None = None
        else:
            self._rc_overlay = self._require_dir(
                rc_overlay_value,
                what="rc overlay directory",
                option="rc_overlay",
                env_var=RC_OVERLAY_ENV,
            )
            self._rc_overlay_hash = _dir_content_hash(self._rc_overlay)

        # A reused container must not silently continue the previous kernel's
        # contexts and transcript, so each constructed agent gets its own state
        # directory. The steps of one multi-step trial share it, which is the
        # continuity a trial is supposed to have.
        self._remote_state = self.REMOTE_DIR / f"state-{uuid.uuid4().hex[:12]}"

        self._worktree = Path(__file__).resolve().parent
        self._git_head = _git(self._worktree, "rev-parse", "HEAD")
        self._git_dirty = bool(_git(self._worktree, "status", "--porcelain"))
        self._binary_sha256 = _sha256(self._local_binary)

        kwargs.setdefault("registry_entry", self._registry_entry_payload())
        kwargs.setdefault("distribution_preference", ["local"])

        super().__init__(
            binary_path=str(self._local_binary),
            gate_config_path=str(self._local_gate) if self._local_gate else GATE_NONE,
            backend_kind=self._backend_kind,
            solo_model=self._solo_model,
            solo_args=self._solo_args,
            rust_log=self._rust_log,
            max_tokens=self._max_tokens,
            rc_overlay=str(self._rc_overlay) if self._rc_overlay is not None else None,
            **kwargs,
        )

        # Duplicating Harbor's split is only safe while it stays the same
        # split. Check it rather than trust it.
        if (self._parsed_model_provider, self._parsed_model_name) != (
            parsed_provider,
            parsed_name,
        ):
            raise RuntimeError(
                "Harbor parsed --model as "
                f"{self._parsed_model_provider!r}/{self._parsed_model_name!r} "
                f"but this agent read {parsed_provider!r}/{parsed_name!r}. "
                "harbor.agents.base.BaseAgent._init_model_info changed; update "
                "_split_model_name to match before running anything."
            )

        if harbor_model:
            self.logger.info(
                f"Harbor --model {harbor_model!r} becomes "
                f"--backend-kind {self._backend_kind} --model {self._solo_model}; "
                "no ACP model-selection request is sent."
            )
            # _parsed_model_name/_parsed_model_provider were read by
            # BaseAgent.__init__ above and stay, so AgentInfo still reports the
            # model. Clearing model_name only stops AcpAgent.run() from setting
            # HARBOR_ACP_REQUESTED_MODEL.
            self.model_name = None

    # ---- construction helpers -------------------------------------------

    @staticmethod
    def _require_file(
        value: str | None, *, what: str, option: str, env_var: str
    ) -> Path:
        if not value:
            raise ValueError(f"No {what}: set --ak {option}=<path> or ${env_var}.")
        path = Path(value).expanduser().resolve()
        if not path.is_file():
            raise FileNotFoundError(f"{what} is not a file: {path}")
        return path

    @staticmethod
    def _require_dir(value: str, *, what: str, option: str, env_var: str) -> Path:
        path = Path(value).expanduser().resolve()
        if not path.is_dir():
            raise NotADirectoryError(
                f"{what} is not a directory: {path} (--ak {option}=<dir> or ${env_var})"
            )
        return path

    def _entry_version(self) -> str:
        """A version that names the thing that actually ran."""
        if self._git_head:
            return f"git-{self._git_head[:12]}" + ("-dirty" if self._git_dirty else "")
        return f"sha256-{self._binary_sha256[:12]}"

    def _solo_command(self) -> list[str]:
        args = [
            "--backend-kind",
            self._backend_kind,
            "--model",
            self._solo_model,
            "--state-dir",
            self._remote_state.as_posix(),
        ]
        if self._local_gate is not None:
            args += ["--gate-config", self.REMOTE_GATE.as_posix()]
        if self._max_tokens is not None:
            args += ["--max-tokens", str(self._max_tokens)]
        if self._rc_overlay is not None:
            args += ["--rc-overlay", self.REMOTE_RC_OVERLAY.as_posix()]
        return args + self._solo_args

    def _registry_entry_payload(self) -> dict[str, Any]:
        """The ACP registry entry Harbor's launcher is built from."""
        return {
            "id": "kaijutsu-solo-acp",
            "name": "kaijutsu",
            "version": self._entry_version(),
            "description": "kaijutsu with its own kernel, over ACP v1",
            "distribution": {
                "local": {
                    "cmd": self.REMOTE_BINARY.as_posix(),
                    "args": self._solo_command(),
                    "env": {"RUST_LOG": self._rust_log},
                }
            },
        }

    # ---- install ---------------------------------------------------------

    async def _require_machine(self, environment: BaseEnvironment) -> str:
        result = await environment.exec(command="uname -m")
        machine = _last_line(getattr(result, "stdout", ""))
        if machine != REQUIRED_MACHINE:
            raise RuntimeError(
                f"The kaijutsu-solo-acp binary at {self._local_binary} is a "
                f"static {REQUIRED_MACHINE} build and this environment reports "
                f"'uname -m' = {machine!r}. Build the binary for that machine, "
                "or run the task on an x86_64 image."
            )
        return machine

    async def _require_ca_bundle(self, environment: BaseEnvironment) -> str:
        probe = (
            f"for p in {' '.join(shlex.quote(p) for p in CA_BUNDLE_PATHS)}; do "
            'if [ -s "$p" ]; then echo "$p"; exit 0; fi; done; exit 1'
        )
        result = await environment.exec(command=probe)
        found = _last_line(getattr(result, "stdout", ""))
        if getattr(result, "return_code", 1) != 0 or not found:
            raise RuntimeError(
                "No CA bundle in the task container, so every model call would "
                "fail with a TLS error: rustls-platform-verifier carries no "
                "roots of its own and reads the host store. Harbor's ACP setup "
                "installs `ca-certificates` "
                "(harbor/agents/installed/acp.py, _build_dependencies_command); "
                "it did not take here. Looked for: "
                f"{', '.join(CA_BUNDLE_PATHS)}."
            )
        return found

    @staticmethod
    def _harbor_version() -> str | None:
        try:
            from importlib.metadata import version

            return version("harbor")
        except Exception:
            return None

    def _write_provenance(self, machine: str, ca_bundle: str) -> None:
        """Tie this trial's output to the binary and revision that made it."""
        binary_stat = self._local_binary.stat()
        payload = {
            "written_at": datetime.now(timezone.utc).isoformat(),
            "agent_import_path": f"{__name__}:{type(self).__name__}",
            "agent_version": self._entry_version(),
            "binary": {
                "host_path": str(self._local_binary),
                "sha256": self._binary_sha256,
                "size_bytes": binary_stat.st_size,
                "mtime": datetime.fromtimestamp(
                    binary_stat.st_mtime, timezone.utc
                ).isoformat(),
                "remote_path": self.REMOTE_BINARY.as_posix(),
            },
            "gate": (
                {
                    "host_path": str(self._local_gate),
                    "sha256": _sha256(self._local_gate),
                    "remote_path": self.REMOTE_GATE.as_posix(),
                }
                if self._local_gate is not None
                else {"installed": False, "reason": f"gate_config_path={GATE_NONE}"}
            ),
            "rc_overlay": (
                {
                    "host_path": str(self._rc_overlay),
                    "content_hash": self._rc_overlay_hash,
                    "remote_path": self.REMOTE_RC_OVERLAY.as_posix(),
                }
                if self._rc_overlay is not None
                else {"installed": False}
            ),
            "worktree": {
                "path": str(self._worktree),
                "head": self._git_head,
                "dirty": self._git_dirty,
            },
            "model": {
                "backend_kind": self._backend_kind,
                "model": self._solo_model,
                "harbor_model_name": (
                    f"{self._parsed_model_provider}/{self._parsed_model_name}"
                    if self._parsed_model_provider and self._parsed_model_name
                    else self._parsed_model_name
                ),
                "rust_log": self._rust_log,
                "extra_args": self._solo_args,
                "max_tokens": self._max_tokens,
            },
            "environment": {
                "machine": machine,
                "ca_bundle": ca_bundle,
                "state_dir": self._remote_state.as_posix(),
            },
            "harbor_version": self._harbor_version(),
        }
        path = self.logs_dir / self.PROVENANCE_FILENAME
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        machine = await self._require_machine(environment)

        remote_dir = shlex.quote(self.REMOTE_DIR.as_posix())
        remote_state = shlex.quote(self._remote_state.as_posix())
        remote_binary = shlex.quote(self.REMOTE_BINARY.as_posix())

        # A state directory that already exists is a previous kernel.
        # Continuing it silently would blend two runs' transcripts.
        existing = await environment.exec(
            command=f"if [ -e {remote_state} ]; then echo present; else echo absent; fi"
        )
        if _last_line(getattr(existing, "stdout", "")) != "absent":
            raise RuntimeError(
                f"State directory {self._remote_state} already exists in this "
                "environment. A run starts on a fresh kernel; refusing to "
                "continue a previous one."
            )

        await self.exec_as_root(
            environment, command=f"mkdir -p {remote_dir} {remote_state}"
        )
        await environment.upload_file(
            source_path=self._local_binary,
            target_path=self.REMOTE_BINARY.as_posix(),
        )
        chmod = f"chmod 0755 {remote_binary}"
        if self._local_gate is not None:
            await environment.upload_file(
                source_path=self._local_gate,
                target_path=self.REMOTE_GATE.as_posix(),
            )
            chmod += f" && chmod 0644 {shlex.quote(self.REMOTE_GATE.as_posix())}"
        if self._rc_overlay is not None:
            await environment.upload_dir(
                source_dir=self._rc_overlay,
                target_dir=self.REMOTE_RC_OVERLAY.as_posix(),
            )
            chmod += f" && chmod -R a+rX {shlex.quote(self.REMOTE_RC_OVERLAY.as_posix())}"
        # The agent user must be able to read and run both, and to write state.
        # Inside a disposable task container that is deliberately permissive.
        chmod += f" && chmod -R a+rwX {remote_state}"
        await self.exec_as_root(environment, command=chmod)

        # Harbor's ACP dependency step installs `ca-certificates`, so the CA
        # check belongs after it, not before.
        await super().install(environment)
        ca_bundle = await self._require_ca_bundle(environment)

        self._write_provenance(machine=machine, ca_bundle=ca_bundle)

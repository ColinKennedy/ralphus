"""`ralphus check health` — check that the local setup can actually run tasks.

Checks are grouped into sections:

- ``core``: things every user needs (daemon reachable, git, the runner
  binary, Ollama for local-model runs, ``nvidia-smi`` for GPU metrics in the
  resource view). These are hard requirements (`fail`) — Ollama included,
  since it is what runs local models — except ``nvidia-smi``, which is
  advisory (`warn`): the GPU column in the resource view degrades to N/A
  without it.
- ``developer``: things only someone building/authoring against ralphus
  needs (``cargo`` to build the Rust binaries; pydantic-ai — the ``runner``
  extra — for ``ralphus author``). Both are hard `fail`s here. This whole
  section is opt-in: it runs only when ``enable_developer_checks`` is set,
  so end users on a release build aren't warned about tools they don't need.

Only a `fail` makes `check health` exit non-zero.
"""

from __future__ import annotations

import importlib.util
import os
import shutil
import urllib.error
import urllib.request
from dataclasses import dataclass

from ralphus.client import DaemonClient, DaemonError
from ralphus.config import load_config

__all__ = ["CORE", "DEVELOPER", "CheckResult", "pydantic_ai_available", "run_checks"]

_PASS = "pass"
_WARN = "warn"
_FAIL = "fail"

CORE = "core"
DEVELOPER = "developer"


@dataclass
class CheckResult:
    """The outcome of one health check."""

    name: str
    status: str
    detail: str
    section: str = CORE

    @property
    def is_fail(self) -> bool:
        """True when this check is a hard failure."""
        return self.status == _FAIL


def pydantic_ai_available() -> bool:
    """True if the optional ``pydantic-ai`` (the ``runner`` extra) is installed.

    Checked via ``find_spec`` rather than a bare ``import pydantic_ai`` so
    callers (e.g. ``ralphus author``) can decide *before* touching any
    pydantic-ai-dependent code that lazily imports it later.
    """
    return importlib.util.find_spec("pydantic_ai") is not None


def _check_daemon(daemon_url: str) -> CheckResult:
    try:
        with DaemonClient(daemon_url) as client:
            health = client.health()
    except DaemonError as exc:
        return CheckResult("daemon", _FAIL, f"unreachable at {daemon_url}: {exc}")
    version = health.get("version", "?")
    return CheckResult("daemon", _PASS, f"reachable ({health.get('name', '?')} {version})")


def _check_git() -> CheckResult:
    path = shutil.which("git")
    if path is None:
        return CheckResult("git", _FAIL, "not found on PATH (required for Guardian reviews)")
    return CheckResult("git", _PASS, path)


def _check_runner() -> CheckResult:
    cmd = os.environ.get("RALPHUS_RUNNER_CMD", "ralphus-runner").split()
    program = cmd[0] if cmd else "ralphus-runner"
    if shutil.which(program) is None and not os.path.exists(program):
        return CheckResult("runner", _WARN, f"'{program}' not found (set RALPHUS_RUNNER_CMD)")
    return CheckResult("runner", _PASS, program)


def _check_nvidia_smi() -> CheckResult:
    path = shutil.which("nvidia-smi")
    if path is None:
        return CheckResult(
            "nvidia-smi",
            _WARN,
            "not found on PATH; GPU usage in the resource view will show N/A",
        )
    return CheckResult("nvidia-smi", _PASS, path)


def _check_ollama() -> CheckResult:
    base = os.environ.get("RALPHUS_OLLAMA_URL", "http://localhost:11434/v1")
    tags = base.rstrip("/").removesuffix("/v1") + "/api/tags"
    try:
        with urllib.request.urlopen(tags, timeout=2) as resp:
            resp.read()
    except (urllib.error.URLError, TimeoutError, OSError):
        return CheckResult(
            "ollama",
            _FAIL,
            f"not reachable at {base} (required to run local models; start it with 'ollama serve')",
        )
    return CheckResult("ollama", _PASS, base)


def _check_config() -> CheckResult:
    config = load_config()
    mt = config.task.maximum_timeout_seconds
    if mt < 0:
        return CheckResult(
            "config",
            _FAIL,
            f"task.maximum_timeout_seconds is {mt};"
            " must be >= 0 (0 = unbounded, positive = seconds)",
        )
    if mt == 0:
        src = f" (from {config.sources[-1]})" if config.sources else ""
        return CheckResult(
            "config",
            _WARN,
            f"task.maximum_timeout_seconds is 0 (no timeout){src};"
            " backend sessions may run forever",
        )
    src = f" from {config.sources[-1]}" if config.sources else " (default)"
    return CheckResult("config", _PASS, f"task.maximum_timeout_seconds={mt}s{src}")


def _check_cargo() -> CheckResult:
    path = shutil.which("cargo")
    if path is None:
        return CheckResult(
            "cargo",
            _FAIL,
            "cargo not found on PATH (required to build the Rust binaries; "
            "install it via https://rustup.rs)",
            section=DEVELOPER,
        )
    return CheckResult("cargo", _PASS, path, section=DEVELOPER)


def _check_pydantic_ai() -> CheckResult:
    if not pydantic_ai_available():
        return CheckResult(
            "pydantic-ai",
            _FAIL,
            "not installed; `ralphus author` cannot run without it "
            "(install the 'runner' extra: uv sync --extra runner)",
            section=DEVELOPER,
        )
    return CheckResult("pydantic-ai", _PASS, "installed", section=DEVELOPER)


def run_checks(daemon_url: str, *, enable_developer_checks: bool = False) -> list[CheckResult]:
    """Run the core health checks; add the developer section when opted in.

    The developer section (``cargo``, ``pydantic-ai``) is only relevant to
    someone building or authoring against ralphus, so it runs only when
    ``enable_developer_checks`` is set (``check health --enable-developer-checks``).
    """
    results = [
        _check_daemon(daemon_url),
        _check_git(),
        _check_runner(),
        _check_ollama(),
        _check_nvidia_smi(),
        _check_config(),
    ]
    if enable_developer_checks:
        results += [
            _check_cargo(),
            _check_pydantic_ai(),
        ]
    return results

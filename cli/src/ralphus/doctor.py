"""`ralphus doctor` — check that the local setup can actually run tasks.

Each check returns pass / warn / fail. Only a `fail` makes `doctor` exit
non-zero, so advisory issues (Ollama down, pydantic-ai absent) do not block a
user who only runs cloud/command tasks.
"""

from __future__ import annotations

import importlib.util
import os
import shutil
import urllib.error
import urllib.request
from dataclasses import dataclass

from ralphus.client import DaemonClient, DaemonError

__all__ = ["CheckResult", "run_checks"]

_PASS = "pass"
_WARN = "warn"
_FAIL = "fail"


@dataclass
class CheckResult:
    """The outcome of one doctor check."""

    name: str
    status: str
    detail: str

    @property
    def is_fail(self) -> bool:
        """True when this check is a hard failure."""
        return self.status == _FAIL


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


def _check_pydantic_ai() -> CheckResult:
    if importlib.util.find_spec("pydantic_ai") is None:
        return CheckResult(
            "pydantic-ai",
            _WARN,
            "not installed; prompt (AI) sessions need it (install the 'runner' extra)",
        )
    return CheckResult("pydantic-ai", _PASS, "installed")


def _check_ollama() -> CheckResult:
    base = os.environ.get("RALPHUS_OLLAMA_URL", "http://localhost:11434/v1")
    tags = base.rstrip("/").removesuffix("/v1") + "/api/tags"
    try:
        with urllib.request.urlopen(tags, timeout=2) as resp:
            resp.read()
    except (urllib.error.URLError, TimeoutError, OSError):
        return CheckResult("ollama", _WARN, "not reachable (only needed for local-model runs)")
    return CheckResult("ollama", _PASS, base)


def run_checks(daemon_url: str) -> list[CheckResult]:
    """Run all doctor checks."""
    return [
        _check_daemon(daemon_url),
        _check_git(),
        _check_runner(),
        _check_pydantic_ai(),
        _check_ollama(),
    ]

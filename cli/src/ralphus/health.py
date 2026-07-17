"""`ralphus check health` — check that the local setup can actually run tasks.

Checks are grouped into sections:

- ``core``: things every user needs (daemon reachable, git, every registered
  project's on-disk path/git-repo validity (RAL-100+), the runner binary,
  Ollama for local-model runs, ``nvidia-smi`` for GPU metrics in the
  resource view, ``$RALPHUS_CLAUDE_COMMAND`` when set (RAL-110)). These are
  hard requirements (`fail`) — Ollama included, since it is what runs local
  models — except ``nvidia-smi``, which is advisory (`warn`): the GPU column
  in the resource view degrades to N/A without it.
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
import subprocess
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from ralphus.client import DaemonClient, DaemonError
from ralphus.config import load_config

__all__ = [
    "CORE",
    "DEVELOPER",
    "CheckResult",
    "is_compound_shell_command",
    "pydantic_ai_available",
    "run_checks",
    "unquote_path",
]

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


def is_compound_shell_command(value: str) -> bool:
    """RAL-110 Q5 heuristic: is `value` a compound shell command rather than
    a single bare executable path?

    A basic parse is deliberately all that's needed (no full shell-lexing):
    a value with no space is always a bare path (e.g. ``claude`` or
    ``/usr/bin/claude``). A value with a space is still a bare path if it is
    entirely wrapped in one matching pair of quotes (e.g. a Windows path with
    spaces, ``"C:\\Program Files\\claude\\claude.exe"``); otherwise (e.g.
    ``cd foo bar ; ./claude``) it's treated as a compound shell command, to
    be executed via a shell rather than path-checked or exec'd directly.
    """
    stripped = value.strip()
    if " " not in stripped:
        return False
    if len(stripped) >= 2 and stripped[0] == stripped[-1] and stripped[0] in "'\"":
        inner = stripped[1:-1]
        if stripped[0] not in inner:
            return False
    return True


def unquote_path(value: str) -> str:
    """Strip one layer of wrapping quotes from a bare (non-compound) path."""
    stripped = value.strip()
    if len(stripped) >= 2 and stripped[0] == stripped[-1] and stripped[0] in "'\"":
        return stripped[1:-1]
    return stripped


def _check_claude_command() -> CheckResult:
    """Validate `$RALPHUS_CLAUDE_COMMAND` (RAL-110): when it names a single
    bare path (not a compound shell command -- see `is_compound_shell_command`),
    that path must exist and be executable. A compound command is trusted
    as-is -- there's nothing meaningful to path-check about shell syntax.
    """
    name = "claude-command"
    raw = os.environ.get("RALPHUS_CLAUDE_COMMAND")
    if raw is None:
        return CheckResult(name, _PASS, "not set (defaults to 'claude' on PATH)")
    if is_compound_shell_command(raw):
        return CheckResult(name, _PASS, f"compound shell command, not path-checked: {raw}")
    path = unquote_path(raw)
    p = Path(path)
    if not p.is_file():
        return CheckResult(name, _FAIL, f"{path} does not exist or is not a file")
    if not os.access(p, os.X_OK):
        return CheckResult(name, _FAIL, f"{path} is not executable")
    return CheckResult(name, _PASS, path)


def pydantic_ai_available() -> bool:
    """True if the optional ``pydantic-ai`` (the ``runner`` extra) is installed.

    Checked via ``find_spec`` rather than a bare ``import pydantic_ai`` so
    callers (e.g. ``ralphus author``) can decide *before* touching any
    pydantic-ai-dependent code that lazily imports it later.
    """
    return importlib.util.find_spec("pydantic_ai") is not None


def _check_daemon(daemon_url: str) -> list[CheckResult]:
    try:
        with DaemonClient(daemon_url) as client:
            health = client.health()
    except DaemonError as exc:
        return [CheckResult("daemon", _FAIL, f"unreachable at {daemon_url}: {exc}")]
    version = health.get("version", "?")
    results = [CheckResult("daemon", _PASS, f"reachable ({health.get('name', '?')} {version})")]
    for warning in health.get("warnings", []):
        results.append(CheckResult("daemon", _WARN, str(warning)))
    return results


def _check_git() -> CheckResult:
    path = shutil.which("git")
    if path is None:
        return CheckResult("git", _FAIL, "not found on PATH (required for Guardian reviews)")
    return CheckResult("git", _PASS, path)


def _check_project_path(name: str, path: str) -> CheckResult:
    """Re-run the same path/git-repo validation `ralphus project git` does at
    registration time (RAL-100), for one already-registered project.

    A registered project can go stale after the fact -- the directory moved,
    was deleted, or its `.git` was removed -- so this re-checks it exactly
    the way the daemon's `POST /api/projects` does: the path must exist and
    be a directory, and `git` must recognize it as a work tree.
    """
    check_name = f"project:{name}"
    p = Path(path)
    if not p.is_dir():
        return CheckResult(check_name, _FAIL, f"{path} does not exist or is not a directory")
    try:
        result = subprocess.run(
            ["git", "-C", str(p), "rev-parse", "--is-inside-work-tree"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return CheckResult(check_name, _FAIL, f"could not run git in {path}: {exc}")
    if result.returncode != 0 or result.stdout.strip() != "true":
        return CheckResult(check_name, _FAIL, f"{path} is not a git repository")
    return CheckResult(check_name, _PASS, path)


def _check_projects(daemon_url: str) -> list[CheckResult]:
    """Validate every project registered with the daemon (RAL-100+).

    Silently contributes nothing if the daemon is unreachable -- `_check_daemon`
    already reports that failure -- or if no projects are registered.
    """
    try:
        with DaemonClient(daemon_url) as client:
            projects = client.list_projects().get("projects", [])
    except DaemonError:
        return []
    return [_check_project_path(p["name"], p["path"]) for p in projects]


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
        *_check_daemon(daemon_url),
        _check_git(),
        *_check_projects(daemon_url),
        _check_runner(),
        _check_ollama(),
        _check_nvidia_smi(),
        _check_config(),
        _check_claude_command(),
    ]
    if enable_developer_checks:
        results += [
            _check_cargo(),
            _check_pydantic_ai(),
        ]
    return results

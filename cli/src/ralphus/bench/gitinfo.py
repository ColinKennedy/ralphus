"""Git commit/dirty-state lookup for benchmark records (RAL-94).

Every per-commit bench record needs the current HEAD sha and whether the
working tree had uncommitted changes at benchmark time. The harness never
refuses to record based on dirtiness — it only annotates the record so
graph rendering can flag it.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path

__all__ = ["GitState", "current_git_state"]


@dataclass
class GitState:
    """HEAD commit sha and working-tree dirtiness at benchmark time."""

    commit: str
    dirty: bool


def current_git_state(cwd: Path | None = None) -> GitState:
    """Return the current HEAD sha and whether the working tree is dirty.

    Raises RuntimeError if `git` is unavailable or the directory is not a git
    checkout — the caller decides how to handle that (the bench harness is a
    dev-time tool that requires a git repo to be meaningful).
    """
    commit = _run_git(["rev-parse", "HEAD"], cwd=cwd)
    status = _run_git(["status", "--porcelain"], cwd=cwd)
    return GitState(commit=commit, dirty=bool(status))


def _run_git(args: list[str], *, cwd: Path | None) -> str:
    try:
        result = subprocess.run(
            ["git", *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise RuntimeError(f"git {' '.join(args)} failed: {exc}") from exc
    return result.stdout.strip()

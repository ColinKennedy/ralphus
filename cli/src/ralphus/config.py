"""ralphus configuration loader.

Reads .ralphus.toml files from RALPHUS_CONFIGURATION_PATH (os.pathsep-separated
list of file paths) and, when one exists, the .ralphus.toml at the git repository
root.  Files listed later in RALPHUS_CONFIGURATION_PATH override earlier ones;
the git-root file has the highest priority and overrides all env-var entries.

All parsing uses tomllib from the Python 3.11 standard library — no third-party
dependencies are added.
"""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

__all__ = ["Config", "TaskConfig", "load_config"]

_DEFAULT_TIMEOUT_SEC = 1800


@dataclass
class TaskConfig:
    """Configuration under the [task] header."""

    maximum_timeout_seconds: int = _DEFAULT_TIMEOUT_SEC


@dataclass
class Config:
    """Fully resolved configuration, merged from all applicable .ralphus.toml files."""

    task: TaskConfig = field(default_factory=TaskConfig)
    sources: list[Path] = field(default_factory=list)

    def subprocess_timeout(self) -> float | None:
        """Wall-clock timeout for backend subprocesses, or None for no limit.

        maximum_timeout_seconds <= 0 maps to None (unbounded).  Negative values are flagged
        by 'ralphus check health'; they are not clamped here.
        """
        if self.task.maximum_timeout_seconds <= 0:
            return None
        return float(self.task.maximum_timeout_seconds)


def _find_git_root(start: Path) -> Path | None:
    """Walk upward from start looking for a .git entry."""
    current = start.resolve()
    while True:
        if (current / ".git").exists():
            return current
        parent = current.parent
        if parent == current:
            return None
        current = parent


def _apply(base: TaskConfig, raw: dict[str, object]) -> TaskConfig:
    """Return a new TaskConfig with values from raw overlaid on base."""
    section = raw.get("task")
    if not isinstance(section, dict):
        return base
    mt = section.get("maximum_timeout_seconds")
    if isinstance(mt, int):
        return TaskConfig(maximum_timeout_seconds=mt)
    return base


def load_config() -> Config:
    """Load and merge all applicable .ralphus.toml configuration files.

    Resolution order (later wins):
    1. Files from RALPHUS_CONFIGURATION_PATH, left-to-right.
    2. .ralphus.toml at the git repository root (if present).
    """
    candidate_paths: list[Path] = []

    env_val = os.environ.get("RALPHUS_CONFIGURATION_PATH", "")
    if env_val:
        for part in env_val.split(os.pathsep):
            part = part.strip()
            if part:
                candidate_paths.append(Path(part))

    git_root = _find_git_root(Path.cwd())
    if git_root is not None:
        git_config = git_root / ".ralphus.toml"
        if git_config not in candidate_paths:
            candidate_paths.append(git_config)

    task = TaskConfig()
    sources: list[Path] = []
    for path in candidate_paths:
        try:
            with path.open("rb") as fh:
                raw: dict[str, object] = tomllib.load(fh)
        except FileNotFoundError:
            continue
        except (OSError, tomllib.TOMLDecodeError):
            continue
        task = _apply(task, raw)
        sources.append(path)

    return Config(task=task, sources=sources)

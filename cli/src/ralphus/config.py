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

__all__ = [
    "Config",
    "ConfigFileIssues",
    "DaemonConfig",
    "TaskConfig",
    "load_config",
    "validate_config_files",
]

_DEFAULT_TIMEOUT_SEC = 1800
_VALID_LOG_LEVELS = ("error", "warn", "info", "debug", "trace")


@dataclass
class TaskConfig:
    """Configuration under the [task] header."""

    maximum_timeout_seconds: int = _DEFAULT_TIMEOUT_SEC


@dataclass
class DaemonConfig:
    """Configuration under the [daemon] header."""

    log_path: str | None = None
    log_level: str | None = None
    keep_temporary_files: bool = False


@dataclass
class Config:
    """Fully resolved configuration, merged from all applicable .ralphus.toml files."""

    task: TaskConfig = field(default_factory=TaskConfig)
    daemon: DaemonConfig = field(default_factory=DaemonConfig)
    sources: list[Path] = field(default_factory=list)
    source_labels: dict[Path, str] = field(default_factory=dict)
    provenance: dict[str, Path | None] = field(default_factory=dict)

    def subprocess_timeout(self) -> float | None:
        """Wall-clock timeout for backend subprocesses, or None for no limit.

        maximum_timeout_seconds <= 0 maps to None (unbounded).  Negative values are flagged
        by 'ralphus check health'; they are not clamped here.
        """
        if self.task.maximum_timeout_seconds <= 0:
            return None
        return float(self.task.maximum_timeout_seconds)


@dataclass
class ConfigFileIssues:
    """Validation results for one .ralphus.toml file."""

    path: Path
    label: str
    syntax_error: str | None
    issues: list[str]


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


def _type_name(val: object) -> str:
    if isinstance(val, bool):
        return "boolean"
    if isinstance(val, int):
        return "integer"
    if isinstance(val, float):
        return "float"
    if isinstance(val, str):
        return "string"
    if isinstance(val, list):
        return "array"
    if isinstance(val, dict):
        return "table"
    return type(val).__name__


def _validate_raw(raw: dict[str, object]) -> list[str]:
    issues: list[str] = []

    known_top = {"task", "daemon", "review", "defaults"}
    for key in raw:
        if key not in known_top:
            issues.append(f'key "{key}" is unknown')

    task_raw = raw.get("task")
    if task_raw is not None:
        if not isinstance(task_raw, dict):
            issues.append(f'key "task" expects a "table" but got a "{_type_name(task_raw)}" type')
        else:
            known_task = {"maximum_timeout_seconds"}
            for k in task_raw:
                if k not in known_task:
                    issues.append(f'key "task.{k}" is unknown')
            mt = task_raw.get("maximum_timeout_seconds")
            if mt is not None:
                if isinstance(mt, bool) or not isinstance(mt, int):
                    issues.append(
                        f'key "task.maximum_timeout_seconds" expects "integer"'
                        f' but got a "{_type_name(mt)}" type'
                    )
                elif mt < 0:
                    issues.append(
                        f'key "task.maximum_timeout_seconds" got invalid value {mt}.'
                        " Expected >= 0 (0 = unbounded, positive = cap in seconds)"
                    )

    daemon_raw = raw.get("daemon")
    if daemon_raw is not None:
        if not isinstance(daemon_raw, dict):
            issues.append(
                f'key "daemon" expects a "table" but got a "{_type_name(daemon_raw)}" type'
            )
        else:
            known_daemon = {"log_path", "log_level", "keep_temporary_files"}
            for k in daemon_raw:
                if k not in known_daemon:
                    issues.append(f'key "daemon.{k}" is unknown')
            lp = daemon_raw.get("log_path")
            if lp is not None and not isinstance(lp, str):
                issues.append(
                    f'key "daemon.log_path" expects "string" but got a "{_type_name(lp)}" type'
                )
            ll = daemon_raw.get("log_level")
            if ll is not None:
                if not isinstance(ll, str):
                    issues.append(
                        f'key "daemon.log_level" expects "string" but got a "{_type_name(ll)}" type'
                    )
                elif ll not in _VALID_LOG_LEVELS:
                    valid = ", ".join(f'"{v}"' for v in _VALID_LOG_LEVELS)
                    issues.append(
                        f'key "daemon.log_level" got invalid value "{ll}".'
                        f" Expected one of [{valid}]"
                    )
            ktf = daemon_raw.get("keep_temporary_files")
            if ktf is not None and not isinstance(ktf, bool):
                issues.append(
                    f'key "daemon.keep_temporary_files" expects "boolean"'
                    f' but got a "{_type_name(ktf)}" type'
                )

    for section_name in ("review", "defaults"):
        section_raw = raw.get(section_name)
        if section_raw is not None:
            if not isinstance(section_raw, dict):
                issues.append(
                    f'key "{section_name}" expects a "table"'
                    f' but got a "{_type_name(section_raw)}" type'
                )
            else:
                known_review = {"skip_worktrees", "checks"}
                for k in section_raw:
                    if k not in known_review:
                        issues.append(f'key "{section_name}.{k}" is unknown')
                sw = section_raw.get("skip_worktrees")
                if sw is not None and not isinstance(sw, bool):
                    issues.append(
                        f'key "{section_name}.skip_worktrees" expects "boolean"'
                        f' but got a "{_type_name(sw)}" type'
                    )
                checks = section_raw.get("checks")
                if checks is not None:
                    if not isinstance(checks, list):
                        issues.append(
                            f'key "{section_name}.checks" expects "array"'
                            f' but got a "{_type_name(checks)}" type'
                        )
                    else:
                        for i, item in enumerate(checks):
                            if not isinstance(item, str):
                                issues.append(
                                    f'key "{section_name}.checks[{i}]" expects "string"'
                                    f' but got a "{_type_name(item)}" type'
                                )

    return issues


def _apply(base: TaskConfig, raw: dict[str, object]) -> TaskConfig:
    section = raw.get("task")
    if not isinstance(section, dict):
        return base
    mt = section.get("maximum_timeout_seconds")
    if isinstance(mt, int) and not isinstance(mt, bool):
        return TaskConfig(maximum_timeout_seconds=mt)
    return base


def _apply_daemon(base: DaemonConfig, raw: dict[str, object]) -> DaemonConfig:
    section = raw.get("daemon")
    if not isinstance(section, dict):
        return base
    lp = section.get("log_path")
    ll = section.get("log_level")
    ktf = section.get("keep_temporary_files")
    return DaemonConfig(
        log_path=str(lp) if isinstance(lp, str) else base.log_path,
        log_level=str(ll) if isinstance(ll, str) else base.log_level,
        keep_temporary_files=bool(ktf) if isinstance(ktf, bool) else base.keep_temporary_files,
    )


def _get_candidates(*, include_local: bool = True) -> list[tuple[Path, str]]:
    candidates: list[tuple[Path, str]] = []

    env_val = os.environ.get("RALPHUS_CONFIGURATION_PATH", "")
    if env_val:
        for part in env_val.split(os.pathsep):
            part = part.strip()
            if part:
                candidates.append((Path(part), "environment variable"))

    if include_local:
        git_root = _find_git_root(Path.cwd())
        if git_root is not None:
            git_config = git_root / ".ralphus.toml"
            if not any(p == git_config for p, _ in candidates):
                candidates.append((git_config, "local"))

    return candidates


def validate_config_files(*, include_local: bool = True) -> list[ConfigFileIssues]:
    """Parse and validate each candidate config file; return only files with problems.

    Files that do not exist are silently skipped (same as load_config).  Files
    that exist but have a TOML syntax error or unknown/invalid keys are returned
    with their issues populated.  Clean files are omitted from the result.
    """
    results: list[ConfigFileIssues] = []
    for path, label in _get_candidates(include_local=include_local):
        try:
            with path.open("rb") as fh:
                raw: dict[str, object] = tomllib.load(fh)
        except FileNotFoundError:
            continue
        except tomllib.TOMLDecodeError as exc:
            results.append(
                ConfigFileIssues(path=path, label=label, syntax_error=str(exc), issues=[])
            )
            continue
        except OSError as exc:
            results.append(
                ConfigFileIssues(
                    path=path, label=label, syntax_error=f"cannot read: {exc}", issues=[]
                )
            )
            continue
        found = _validate_raw(raw)
        if found:
            results.append(
                ConfigFileIssues(path=path, label=label, syntax_error=None, issues=found)
            )
    return results


def load_config(*, include_local: bool = True) -> Config:
    """Load and merge all applicable .ralphus.toml configuration files.

    Resolution order (later wins):
    1. Files from RALPHUS_CONFIGURATION_PATH, left-to-right  (label: "environment variable").
    2. .ralphus.toml at the git repository root, unless include_local=False  (label: "local").
    """
    task = TaskConfig()
    daemon = DaemonConfig()
    sources: list[Path] = []
    source_labels: dict[Path, str] = {}
    provenance: dict[str, Path | None] = {
        "task.maximum_timeout_seconds": None,
        "daemon.log_path": None,
        "daemon.log_level": None,
        "daemon.keep_temporary_files": None,
    }

    for path, label in _get_candidates(include_local=include_local):
        try:
            with path.open("rb") as fh:
                raw: dict[str, object] = tomllib.load(fh)
        except FileNotFoundError:
            continue
        except (OSError, tomllib.TOMLDecodeError):
            continue

        new_task = _apply(task, raw)
        if new_task.maximum_timeout_seconds != task.maximum_timeout_seconds:
            provenance["task.maximum_timeout_seconds"] = path
        task = new_task

        new_daemon = _apply_daemon(daemon, raw)
        if new_daemon.log_path != daemon.log_path:
            provenance["daemon.log_path"] = path
        if new_daemon.log_level != daemon.log_level:
            provenance["daemon.log_level"] = path
        if new_daemon.keep_temporary_files != daemon.keep_temporary_files:
            provenance["daemon.keep_temporary_files"] = path
        daemon = new_daemon

        sources.append(path)
        source_labels[path] = label

    return Config(
        task=task,
        daemon=daemon,
        sources=sources,
        source_labels=source_labels,
        provenance=provenance,
    )

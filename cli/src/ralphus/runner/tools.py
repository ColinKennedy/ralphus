"""Workspace tools the runner exposes to a model (and uses for command sessions).

Every operation is confined to a workspace root (the session ``cwd``) so a
session cannot read or write outside the directory it was given. These are the
concrete capabilities a native pydantic-ai agent will be handed as tools.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path

__all__ = ["CommandOutput", "ToolError", "Workspace"]


class ToolError(Exception):
    """Raised when a tool operation is invalid (e.g. path escapes the workspace)."""


@dataclass
class CommandOutput:
    """The result of running a shell command."""

    exit_code: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        """True when the command exited zero."""
        return self.exit_code == 0


@dataclass
class Workspace:
    """A directory a session is allowed to operate within."""

    root: Path

    @staticmethod
    def create(path: str) -> Workspace:
        """Build a workspace, requiring the directory to exist."""
        root = Path(path).resolve()
        if not root.is_dir():
            raise ToolError(f"workspace directory does not exist: {path}")
        return Workspace(root=root)

    def _resolve(self, relpath: str) -> Path:
        target = (self.root / relpath).resolve()
        if target != self.root and self.root not in target.parents:
            raise ToolError(f"path escapes the workspace: {relpath}")
        return target

    def read_file(self, relpath: str) -> str:
        """Read a UTF-8 text file within the workspace."""
        target = self._resolve(relpath)
        try:
            return target.read_text(encoding="utf-8")
        except OSError as exc:
            raise ToolError(f"could not read {relpath}: {exc}") from exc

    def write_file(self, relpath: str, content: str) -> None:
        """Write a UTF-8 text file within the workspace, creating parents."""
        target = self._resolve(relpath)
        try:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        except OSError as exc:
            raise ToolError(f"could not write {relpath}: {exc}") from exc

    def run_bash(self, command: str, timeout_sec: int | None = None) -> CommandOutput:
        """Run a shell command with the workspace as the working directory."""
        try:
            proc = subprocess.run(
                command,
                shell=True,
                cwd=self.root,
                capture_output=True,
                text=True,
                timeout=timeout_sec,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            partial = exc.stdout if isinstance(exc.stdout, str) else ""
            return CommandOutput(exit_code=124, stdout=partial, stderr="command timed out")
        return CommandOutput(exit_code=proc.returncode, stdout=proc.stdout, stderr=proc.stderr)

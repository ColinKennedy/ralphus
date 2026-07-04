"""Harness-subprocess model backend.

For agents that are external coding *harnesses* (e.g. ``codex``, ``aider``,
``claude-code``) rather than a raw model, this backend shells out to that program
in the session's working directory and passes the prompt. This is the pluggable
alternative to the native pydantic-ai backend (see FOLLOW #1) and needs no extra
dependencies, so it is always available.

The invocation is generic: ``<program> <args...> [--model <model>] <prompt>``.
Per-harness argument shapes are supplied via the session's ``args``.
"""

from __future__ import annotations

import subprocess

from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["HarnessBackend"]

_TIMEOUT_SEC = 1800


class HarnessBackend:
    """A ModelBackend that runs an external harness program."""

    def __init__(self, program: str, args: list[str] | None = None) -> None:
        self._program = program
        self._args = list(args or [])

    def run(self, prompt: str, workspace: Workspace, *, model: str | None) -> BackendOutcome:
        """Run the harness in the workspace, passing the prompt as the final arg."""
        cmd = [self._program, *self._args]
        if model:
            cmd += ["--model", model]
        cmd.append(prompt)
        try:
            proc = subprocess.run(
                cmd,
                cwd=workspace.root,
                capture_output=True,
                text=True,
                timeout=_TIMEOUT_SEC,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            raise BackendError(f"could not run harness {self._program!r}: {exc}") from exc
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()[-500:]
            raise BackendError(f"harness {self._program!r} exited {proc.returncode}: {detail}")
        return BackendOutcome(summary=proc.stdout.strip()[-2000:])

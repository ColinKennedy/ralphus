"""Claude Code CLI backend.

Drives the ``claude`` CLI headlessly (``claude -p``) instead of the Anthropic
API, so it runs on your existing Claude Code login (e.g. a Max/Pro
subscription) with **no API key and no per-token billing**.

``--dangerously-skip-permissions`` lets the agent edit files and run commands
without interactive prompts, which is what makes unattended task execution work
(the same approach the predecessor used). The ``claude`` program can be
overridden with ``RALPHUS_CLAUDE_CMD``.
"""

from __future__ import annotations

import os
import shutil
import subprocess

from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["ClaudeCodeBackend"]

_TIMEOUT_SEC = 1800


class ClaudeCodeBackend:
    """A ModelBackend that runs the Claude Code CLI in headless print mode."""

    def run(self, prompt: str, workspace: Workspace, *, model: str | None) -> BackendOutcome:
        """Run ``claude -p`` in the workspace, using the logged-in subscription."""
        program = os.environ.get("RALPHUS_CLAUDE_CMD", "claude")
        # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably.
        program = shutil.which(program) or program
        cmd = [program, "-p", prompt, "--dangerously-skip-permissions"]
        if model:
            cmd += ["--model", model]
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
            raise BackendError(f"could not run Claude Code ({program!r}): {exc}") from exc
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()[-500:]
            raise BackendError(f"Claude Code exited {proc.returncode}: {detail}")
        return BackendOutcome(summary=proc.stdout.strip()[-2000:])

"""Claude Code CLI backend.

Drives the ``claude`` CLI headlessly (``claude -p``) instead of the Anthropic
API, so it runs on your existing Claude Code login (e.g. a Max/Pro
subscription) with **no API key and no per-token billing**.

``--dangerously-skip-permissions`` lets the agent edit files and run commands
without interactive prompts, which is what makes unattended task execution work
(the same approach the predecessor used). The ``claude`` program can be
overridden with ``RALPHUS_CLAUDE_CMD``.

To avoid OS command-line length limits, the prompt is written to a temporary
file under ~/.ralphus/task_prompts/ and referenced via the ``@path`` file-
injection syntax supported by the Claude Code CLI.  The file is created
immediately before invoking Claude and removed when the subprocess exits; the
canonical prompt text lives in the daemon's database.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

from ralphus.config import load_config
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["ClaudeCodeBackend"]


def _write_prompt_file(prompt: str) -> Path:
    """Write prompt to ~/.ralphus/task_prompts/<sha256>.md and return the path."""
    digest = hashlib.sha256(prompt.encode()).hexdigest()[:16]
    prompts_dir = Path.home() / ".ralphus" / "task_prompts"
    prompts_dir.mkdir(parents=True, exist_ok=True)
    path = prompts_dir / f"{digest}.md"
    path.write_text(prompt, encoding="utf-8")
    return path


class ClaudeCodeBackend:
    """A ModelBackend that runs the Claude Code CLI in headless print mode."""

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
        """Run ``claude -p @<prompt-file>`` in the workspace.

        The prompt is written to a temporary file just before the subprocess is
        spawned and deleted immediately after it exits, regardless of outcome.
        The ``@path`` reference tells the Claude Code CLI to inject the file
        contents, keeping the OS command line short.
        """
        program = os.environ.get("RALPHUS_CLAUDE_CMD", "claude")
        # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably.
        program = shutil.which(program) or program
        config = load_config()
        prompt_file = _write_prompt_file(prompt)
        try:
            cmd = [
                program,
                "-p",
                f"@{prompt_file}",
                "--dangerously-skip-permissions",
                "--output-format",
                "json",
            ]
            if model:
                cmd += ["--model", model]
            # Deliver the appended system prompt via the CLI's own flag, so it is
            # applied as a system prompt rather than concatenated into the user
            # prompt (RAL-5).
            if append_system_prompt:
                cmd += ["--append-system-prompt", append_system_prompt]
            try:
                proc = subprocess.run(
                    cmd,
                    cwd=workspace.root,
                    capture_output=True,
                    text=True,
                    # Force UTF-8 decoding of the CLI's output. Without this, `text=True`
                    # decodes with the OS locale codepage (cp1252 on Windows), which
                    # mangles UTF-8 punctuation like em-dashes into mojibake ("â€"").
                    encoding="utf-8",
                    errors="replace",
                    timeout=config.subprocess_timeout(),
                    check=False,
                )
            except (OSError, subprocess.SubprocessError) as exc:
                raise BackendError(f"could not run Claude Code ({program!r}): {exc}") from exc
        finally:
            prompt_file.unlink(missing_ok=True)
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()[-500:]
            raise BackendError(f"Claude Code exited {proc.returncode}: {detail}")
        # Parse the structured JSON output to extract the session UUID and usage.
        try:
            data = json.loads(proc.stdout.strip())
            summary = str(data.get("result", ""))[:2000]
            session_id: str | None = data.get("session_id") or None
            tokens_in = int(data.get("total_input_tokens") or 0)
            tokens_out = int(data.get("total_output_tokens") or 0)
            cost_usd = float(data.get("cost_usd") or 0.0)
        except (json.JSONDecodeError, ValueError, TypeError):
            summary = proc.stdout.strip()[-2000:]
            session_id = None
            tokens_in = 0
            tokens_out = 0
            cost_usd = 0.0
        return BackendOutcome(
            summary=summary,
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=cost_usd,
            claude_session_id=session_id,
        )

"""Codex CLI backend.

Drives the ``codex`` CLI non-interactively (``codex exec``) instead of the
OpenAI API, so it runs against the Codex agent harness
(``npm install -g @openai/codex``) with no direct per-token billing beyond
what the CLI itself negotiates.

``--dangerously-bypass-approvals-and-sandbox`` skips interactive confirmation
prompts, enabling unattended task execution.  The ``codex`` program can be
overridden with ``RALPHUS_CODEX_CMD``.

To avoid OS command-line length limits, the prompt is written to a temporary
file under ~/.ralphus/task_prompts/ and its content is forwarded to
``codex exec`` via stdin (``-`` as the PROMPT argument).  The file is created
immediately before invoking Codex and removed when the subprocess exits.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from ralphus.config import load_config
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["CodexBackend"]


def _write_prompt_file(prompt: str) -> Path:
    """Write prompt to ~/.ralphus/task_prompts/<sha256>.md and return the path."""
    digest = hashlib.sha256(prompt.encode()).hexdigest()[:16]
    prompts_dir = Path.home() / ".ralphus" / "task_prompts"
    prompts_dir.mkdir(parents=True, exist_ok=True)
    path = prompts_dir / f"{digest}.md"
    path.write_text(prompt, encoding="utf-8")
    return path


class CodexBackend:
    """A ModelBackend that runs the Codex CLI non-interactively (``codex exec``)."""

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
        """Run ``codex exec -`` with the prompt piped via stdin.

        The prompt is written to a temporary file just before the subprocess is
        spawned and deleted immediately after it exits, regardless of outcome.
        Its content is read back and forwarded to ``codex exec`` via stdin,
        keeping the OS command line short.

        ``append_system_prompt`` is accepted for Protocol compatibility but not
        wired: the Codex CLI exec subcommand has no non-interactive equivalent
        flag, and TOML validation blocks ``codex``/``codex-cli`` agents from
        setting ``system_prompt``/``system_prompt_position``.
        """
        program = os.environ.get("RALPHUS_CODEX_CMD", "codex")
        program = shutil.which(program) or program
        config = load_config()
        prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:8]
        print(
            f"ralphus [llm-invoke] codex start prompt_len={len(prompt)}"
            f" prompt_hash={prompt_hash} model={model!r}",
            file=sys.stderr,
        )
        t0 = time.monotonic()
        prompt_file = _write_prompt_file(prompt)
        try:
            cmd = [
                program,
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "--ephemeral",
                "-C",
                str(workspace.root),
            ]
            if model:
                cmd += ["-m", model]
            cmd.append("-")
            prompt_text = prompt_file.read_text(encoding="utf-8")
            try:
                proc = subprocess.run(
                    cmd,
                    cwd=workspace.root,
                    input=prompt_text,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    timeout=config.subprocess_timeout(),
                    check=False,
                )
            except (OSError, subprocess.SubprocessError) as exc:
                print(
                    f"ralphus [llm-invoke] codex error: could not run {program!r}: {exc}",
                    file=sys.stderr,
                )
                raise BackendError(f"could not run Codex ({program!r}): {exc}") from exc
        finally:
            prompt_file.unlink(missing_ok=True)
        elapsed = time.monotonic() - t0
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()[-500:]
            print(
                f"ralphus [llm-invoke] codex error: exited {proc.returncode}: {detail[:120]}",
                file=sys.stderr,
            )
            raise BackendError(f"Codex exited {proc.returncode}: {detail}")
        output_len = len(proc.stdout.strip())
        print(
            f"ralphus [llm-invoke] codex done elapsed={elapsed:.2f}s output_len={output_len}",
            file=sys.stderr,
        )
        return BackendOutcome(summary=proc.stdout.strip()[-2000:])

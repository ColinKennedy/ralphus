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
import sys
import time

from ralphus.config import load_config
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["HarnessBackend"]


class HarnessBackend:
    """A ModelBackend that runs an external harness program."""

    def __init__(self, program: str, args: list[str] | None = None) -> None:
        self._program = program
        self._args = list(args or [])

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
        resume_agent_session_id: str | None = None,
    ) -> BackendOutcome:
        """Run the harness in the workspace, passing the prompt as the final arg.

        ``append_system_prompt`` is accepted for Protocol compatibility but not
        yet wired for generic harnesses (there is no portable flag); TOML
        validation blocks non-``claude-code`` agents from setting it (RAL-5).

        ``resume_agent_session_id`` is accepted for Protocol compatibility but
        not wired: the daemon's tmux auto-reattach retry only ever sets it for
        the ``claude-code`` agent, never a generic harness.
        """
        cmd = [self._program, *self._args]
        if model:
            cmd += ["--model", model]
        cmd.append(prompt)
        config = load_config()
        print(
            f"ralphus [llm-invoke] harness {self._program!r} start"
            f" prompt_len={len(prompt)} model={model!r}",
            file=sys.stderr,
        )
        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                cmd,
                cwd=workspace.root,
                capture_output=True,
                text=True,
                # Force UTF-8 decoding; `text=True` alone uses the OS locale
                # codepage (cp1252 on Windows) and mangles UTF-8 output.
                encoding="utf-8",
                errors="replace",
                timeout=config.subprocess_timeout(),
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            print(
                f"ralphus [llm-invoke] harness {self._program!r} error: {exc}",
                file=sys.stderr,
            )
            raise BackendError(f"could not run harness {self._program!r}: {exc}") from exc
        elapsed = time.monotonic() - t0
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout).strip()[-500:]
            print(
                f"ralphus [llm-invoke] harness {self._program!r} error: exited {proc.returncode}",
                file=sys.stderr,
            )
            raise BackendError(f"harness {self._program!r} exited {proc.returncode}: {detail}")
        output_len = len(proc.stdout.strip())
        print(
            f"ralphus [llm-invoke] harness {self._program!r} done"
            f" elapsed={elapsed:.2f}s output_len={output_len}",
            file=sys.stderr,
        )
        return BackendOutcome(summary=proc.stdout.strip()[-2000:])

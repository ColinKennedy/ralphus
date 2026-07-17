"""Claude Code CLI backend.

Drives the ``claude`` CLI headlessly (``claude -p``) instead of the Anthropic
API, so it runs on your existing Claude Code login (e.g. a Max/Pro
subscription) with **no API key and no per-token billing**.

``--dangerously-skip-permissions`` lets the agent edit files and run commands
without interactive prompts, which is what makes unattended task execution work
(the same approach the predecessor used). The ``claude`` program can be
overridden with ``RALPHUS_CLAUDE_COMMAND`` (also used by ``ralphus quick-start
claude-code`` and validated by ``ralphus check health`` -- RAL-110).

To avoid OS command-line length limits, the prompt is written to a temporary
file under ~/.ralphus/task_prompts/ and referenced via the ``@path`` file-
injection syntax supported by the Claude Code CLI.  The file is created
immediately before invoking Claude and removed when the subprocess exits; the
canonical prompt text lives in the daemon's database.

Session IDs are made available as soon as the first stream-json init event
arrives (before the session completes) by writing to a well-known temp-dir
path: ``<tempdir>/ralphus/<worktree-basename>.live_session``.  The daemon
polls that file and pushes the session ID to the DB so the Watch Live button
activates immediately.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from ralphus.config import load_config
from ralphus.runner import cartographer
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import Workspace

__all__ = ["ClaudeCodeBackend", "live_session_path"]


def _write_prompt_file(prompt: str) -> Path:
    """Write prompt to ~/.ralphus/task_prompts/<sha256>.md and return the path."""
    digest = hashlib.sha256(prompt.encode()).hexdigest()[:16]
    prompts_dir = Path.home() / ".ralphus" / "task_prompts"
    prompts_dir.mkdir(parents=True, exist_ok=True)
    path = prompts_dir / f"{digest}.md"
    path.write_text(prompt, encoding="utf-8")
    return path


def _format_tool_input(tool_input: dict[str, object]) -> str:
    """Render a ``tool_use`` block's input compactly for the live tmux pane (RAL-102)."""
    parts = []
    for key, value in tool_input.items():
        text = str(value)
        if len(text) > 80:
            text = text[:80] + "…"
        parts.append(f"{key}={text!r}")
    return ", ".join(parts)


def _tool_result_text(content: object) -> str:
    """Extract human-readable text from a ``tool_result`` block's content (RAL-102).

    ``content`` is either a plain string or a list of content blocks (e.g.
    ``{"type": "text", "text": "..."}``) per the Claude Code stream-json schema.
    """
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        texts = [
            block.get("text", "")
            for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        ]
        return "\n".join(text for text in texts if text)
    return ""


def live_session_path(workspace_root: str | Path) -> Path:
    """Return the path where the session ID is written for Watch Live.

    Placed in the system temp dir so it is never tracked by git, never
    committed accidentally via ``git add -A``, and is accessible to both the
    Python runner and the Rust daemon on the same machine.

    The filename is derived from the worktree basename (e.g. ``wt-RAL-58-…``),
    which is globally unique per guardian-branch pair.
    """
    basename = Path(workspace_root).name or "unknown"
    live_dir = Path(tempfile.gettempdir()) / "ralphus"
    live_dir.mkdir(parents=True, exist_ok=True)
    return live_dir / f"{basename}.live_session"


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

        Uses ``--output-format stream-json`` so the session ID is available
        from the very first event (before the session completes).  The session
        ID is written to a temp-dir side-channel file immediately on receipt of
        the init event so the daemon's watcher thread can push it to the DB
        while the session is still running.
        """
        program = os.environ.get("RALPHUS_CLAUDE_COMMAND", "claude")
        # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably.
        program = shutil.which(program) or program
        # Unlike `ralphus quick-start claude-code`, this backend does not support
        # a compound shell command (e.g. "cd foo && claude") here -- it always
        # spawns `program` directly (never via a shell), matching this file's
        # existing streaming-JSON `Popen` + pipe-parsing design.
        config = load_config()
        prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:8]
        print(
            f"ralphus [llm-invoke] claude-code start prompt_len={len(prompt)}"
            f" prompt_hash={prompt_hash} model={model!r}",
            file=sys.stderr,
        )
        t0 = time.monotonic()
        keep_files = config.daemon.keep_temporary_files
        prompt_file = _write_prompt_file(prompt)
        sid_path = live_session_path(workspace.root)
        # Remove any stale file from a previous run so the Rust watcher does
        # not see an old session ID before the new one arrives.
        sid_path.unlink(missing_ok=True)
        try:
            cmd = [
                program,
                "-p",
                f"@{prompt_file}",
                "--dangerously-skip-permissions",
                "--verbose",
                "--output-format",
                "stream-json",
            ]
            if model:
                cmd += ["--model", model]
            # Deliver the appended system prompt via the CLI's own flag, so it is
            # applied as a system prompt rather than concatenated into the user
            # prompt (RAL-5).
            if append_system_prompt:
                cmd += ["--append-system-prompt", append_system_prompt]
            try:
                proc = subprocess.Popen(
                    cmd,
                    cwd=workspace.root,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    # Force UTF-8 decoding of the CLI's output. Without this, `text=True`
                    # decodes with the OS locale codepage (cp1252 on Windows), which
                    # mangles UTF-8 punctuation like em-dashes into mojibake ("â€"").
                    encoding="utf-8",
                    errors="replace",
                )
            except (OSError, subprocess.SubprocessError) as exc:
                print(
                    f"ralphus [llm-invoke] claude-code error: {exc}",
                    file=sys.stderr,
                )
                raise BackendError(f"could not run Claude Code ({program!r}): {exc}") from exc

            # Human-readable header for the live tmux pane (RAL-102) -- everything
            # below this is Claude's own text/tool-call activity, not runner logging.
            print(f"Claude Code · model={model or 'default'}\ncwd: {workspace.root}\n")

            # Drain stderr in a background thread to prevent pipe-buffer deadlock
            # when stdout is being read line-by-line in the main thread.
            _stderr_chunks: list[str] = []

            def _drain_stderr() -> None:
                if proc.stderr:
                    _stderr_chunks.append(proc.stderr.read())

            _stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
            _stderr_thread.start()

            # Parse the streaming JSON event-line protocol.
            session_id: str | None = None
            result_summary = ""
            tokens_in = 0
            tokens_out = 0
            cost_usd = 0.0

            for raw in proc.stdout or ():
                line = raw.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                ev_type = ev.get("type")
                if ev_type == "system" and ev.get("subtype") == "init":
                    if session_id is None:
                        session_id = ev.get("session_id") or None
                        if session_id:
                            # Write session ID immediately so the Rust watcher thread
                            # can push it to the DB before the session completes.
                            with contextlib.suppress(OSError):
                                sid_path.write_text(session_id, encoding="utf-8")
                            print(
                                f"ralphus [llm-invoke] claude-code session-id={session_id}",
                                file=sys.stderr,
                            )
                            # RALPHUS_EVENT (not just the side-channel file above) so
                            # `runner.rs`'s existing tmux-pane event forwarder -- which
                            # every session already streams through -- can persist this
                            # to the session's own claude_session_id column right away.
                            # Without this, "Open Agent" stayed disabled in the board
                            # until the whole session finished, even though the id was
                            # known and printed to the live pane from the very start.
                            cartographer.emit(
                                "llm-invoke",
                                "claude-code session-id known",
                                level="debug",
                                payload={"claude_session_id": session_id},
                            )
                elif ev_type == "assistant":
                    # Claude's own text/tool-call activity -- the whole point of the
                    # live tmux pane (RAL-102) is to let a human read this, so print
                    # it plainly rather than folding it into a summary line.
                    message = ev.get("message") or {}
                    for block in message.get("content") or []:
                        block_type = block.get("type")
                        if block_type == "text":
                            text = block.get("text", "")
                            if text:
                                print(text)
                        elif block_type == "tool_use":
                            name = block.get("name", "tool")
                            args = _format_tool_input(block.get("input") or {})
                            print(f"[tool] {name}({args})", file=sys.stderr)
                elif ev_type == "user":
                    # Tool results fed back to Claude -- shown for the same reason.
                    message = ev.get("message") or {}
                    for block in message.get("content") or []:
                        if block.get("type") != "tool_result":
                            continue
                        text = _tool_result_text(block.get("content"))
                        if not text:
                            continue
                        if len(text) > 500:
                            text = text[:500] + "…"
                        label = "error" if block.get("is_error") else "result"
                        print(f"[{label}] {text}", file=sys.stderr)
                elif ev_type == "result":
                    result_summary = str(ev.get("result", ""))[:2000]
                    session_id = ev.get("session_id") or session_id
                    tokens_in = int(ev.get("total_input_tokens") or 0)
                    tokens_out = int(ev.get("total_output_tokens") or 0)
                    # stream-json uses total_cost_usd; fall back to cost_usd for
                    # older builds that used the json format field name.
                    cost_usd = float(ev.get("total_cost_usd") or ev.get("cost_usd") or 0.0)

            proc.wait()
            _stderr_thread.join()
            stderr_str = _stderr_chunks[0] if _stderr_chunks else ""
        finally:
            if not keep_files:
                prompt_file.unlink(missing_ok=True)
            # Always remove the side-channel file — the Rust watcher will have
            # already read it, and leaving it on disk would confuse the next run.
            sid_path.unlink(missing_ok=True)

        elapsed = time.monotonic() - t0
        if proc.returncode != 0:
            detail = stderr_str.strip()[-500:] if stderr_str else ""
            print(
                f"ralphus [llm-invoke] claude-code error: exited {proc.returncode}",
                file=sys.stderr,
            )
            raise BackendError(f"Claude Code exited {proc.returncode}: {detail}")
        print(
            f"ralphus [llm-invoke] claude-code done elapsed={elapsed:.2f}s"
            f" tokens_in={tokens_in} tokens_out={tokens_out} cost_usd={cost_usd:.4f}",
            file=sys.stderr,
        )
        return BackendOutcome(
            summary=result_summary,
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=cost_usd,
            claude_session_id=session_id,
        )

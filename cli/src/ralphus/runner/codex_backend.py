"""Codex CLI backend.

Drives the ``codex`` CLI non-interactively (``codex exec``) instead of the
OpenAI API, so it runs against the Codex agent harness
(``npm install -g @openai/codex``) with no direct per-token billing beyond
what the CLI itself negotiates.

``--dangerously-bypass-approvals-and-sandbox`` skips interactive confirmation
prompts, enabling unattended task execution. The ``codex`` program can be
overridden with ``RALPHUS_CODEX_CMD``.

To avoid OS command-line length limits, the prompt is written to a temporary
file under ~/.ralphus/task_prompts/ and its content is forwarded to
``codex exec`` via stdin (``-`` as the PROMPT argument). The file is created
immediately before invoking Codex and removed when the subprocess exits.

Uses ``--json`` (JSONL ``ThreadEvent`` stream -- see
``codex-rs/exec/src/exec_events.rs`` in the Codex source tree) so the thread
id is available from the very first event, the model's own text/tool activity
can be streamed to the live tmux pane (RAL-102) as it happens, and per-turn
token usage is captured. There is no dollar-cost field anywhere in Codex's
output, unlike Claude Code's ``total_cost_usd`` -- ``cost_usd`` is always
``0.0`` for this backend.

Codex has no direct equivalent of ``--append-system-prompt``; the closest
analog is ``-c developer_instructions="..."``, a generic config override that
inserts a ``role="developer"`` message ahead of the turn (confirmed against
``codex-rs/core/tests/suite/client.rs``). That override is only recognized
by the *root* ``codex`` command's own arg parser -- the ``exec`` subcommand's
own CLI struct explicitly skips re-declaring it (`#[clap(skip)]` in
``codex-rs/exec/src/cli.rs``) and instead has it threaded in by
``codex-rs/cli/src/main.rs`` from whatever was parsed *before* the ``exec``
token -- so ``-c ...`` must appear before ``exec`` on the command line, not
after it.

Session resumption uses Codex's own subcommand shape,
``codex exec resume <thread_id> [PROMPT]`` (not a flag, unlike Claude Code's
``--resume``); the thread id is the same UUID captured from the first
``thread.started`` event of the original run.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import shutil
import subprocess
import sys
import threading
import time

from ralphus.config import load_config
from ralphus.runner import cartographer
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.cli_agent_common import RESUME_CONTINUATION_PROMPT, live_session_path
from ralphus.runner.cli_agent_common import write_prompt_file as _write_prompt_file
from ralphus.runner.tools import Workspace

__all__ = ["CodexBackend"]


class CodexBackend:
    """A ModelBackend that runs the Codex CLI non-interactively (``codex exec``)."""

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
        resume_agent_session_id: str | None = None,
    ) -> BackendOutcome:
        """Run ``codex exec`` (or ``codex exec resume <id>``) with the prompt piped via stdin.

        The prompt is written to a temporary file just before the subprocess is
        spawned and deleted immediately after it exits, regardless of outcome.
        Its content is read back and forwarded to Codex via stdin, keeping the
        OS command line short.

        ``append_system_prompt``, when set, is delivered via
        ``-c developer_instructions=<text>`` -- Codex's closest analog to a
        system prompt (see the module docstring). This is how ralphus's own
        internal instructions (non-interactive mode, ghost-note requests, and
        the ``RALPHUS_VERIFY:`` verdict marker for agent-kind verify steps)
        reach the model.

        ``resume_agent_session_id``, when set (the daemon's tmux
        auto-reattach retry -- see `daemon/src/runner.rs`), is treated as a
        Codex thread id: the command becomes ``codex exec resume <id> -``
        and the original prompt is replaced with a short continuation
        directive, matching ``ClaudeCodeBackend``'s same resume behavior.
        """
        program = os.environ.get("RALPHUS_CODEX_CMD", "codex")
        # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably.
        program = shutil.which(program) or program
        config = load_config()
        prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:8]
        if resume_agent_session_id:
            print(
                f"ralphus [llm-invoke] codex RESUME start"
                f" resume_from={resume_agent_session_id} prompt_len={len(prompt)}"
                f" prompt_hash={prompt_hash} model={model!r}",
                file=sys.stderr,
            )
            cartographer.emit(
                "llm-invoke",
                "codex resuming after dropped tmux session",
                level="warning",
                payload={
                    "resume_from": resume_agent_session_id,
                    "original_prompt_hash": prompt_hash,
                    "original_prompt_len": len(prompt),
                },
            )
        else:
            print(
                f"ralphus [llm-invoke] codex start prompt_len={len(prompt)}"
                f" prompt_hash={prompt_hash} model={model!r}",
                file=sys.stderr,
            )
        t0 = time.monotonic()
        keep_files = config.daemon.keep_temporary_files
        effective_prompt = RESUME_CONTINUATION_PROMPT if resume_agent_session_id else prompt
        prompt_file = _write_prompt_file(effective_prompt)
        sid_path = live_session_path(workspace.root)
        # Remove any stale file from a previous run so the Rust watcher does
        # not see an old thread id before the new one arrives.
        sid_path.unlink(missing_ok=True)
        try:
            cmd = [program]
            if append_system_prompt:
                # Must precede `exec` -- see the module docstring on why the
                # `-c` override only threads through when parsed at the root.
                cmd += ["-c", f"developer_instructions={append_system_prompt}"]
            cmd += [
                "exec",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-C",
                str(workspace.root),
            ]
            if model:
                cmd += ["-m", model]
            if resume_agent_session_id:
                cmd += ["resume", resume_agent_session_id, "-"]
            else:
                cmd.append("-")
            prompt_text = prompt_file.read_text(encoding="utf-8")
            try:
                proc = subprocess.Popen(
                    cmd,
                    cwd=workspace.root,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    # Force UTF-8 decoding of the CLI's output -- see the
                    # matching comment in claude_code_backend.py.
                    encoding="utf-8",
                    errors="replace",
                )
            except (OSError, subprocess.SubprocessError) as exc:
                print(
                    f"ralphus [llm-invoke] codex error: could not run {program!r}: {exc}",
                    file=sys.stderr,
                )
                raise BackendError(f"could not run Codex ({program!r}): {exc}") from exc

            # Human-readable header for the live tmux pane (RAL-102) -- everything
            # below this is Codex's own text/tool activity, not runner logging.
            print(f"Codex · model={model or 'default'}\ncwd: {workspace.root}\n")

            # Write the prompt to stdin on its own thread so a large prompt
            # can't deadlock against a full stdout/stderr pipe buffer while
            # Codex hasn't started reading yet.
            def _write_stdin() -> None:
                try:
                    if proc.stdin:
                        proc.stdin.write(prompt_text)
                        proc.stdin.close()
                except (OSError, ValueError):
                    pass

            _stdin_thread = threading.Thread(target=_write_stdin, daemon=True)
            _stdin_thread.start()

            # Drain stderr in a background thread for the same reason
            # claude_code_backend.py does -- avoid pipe-buffer deadlock while
            # stdout is read line-by-line in the main thread.
            _stderr_chunks: list[str] = []

            def _drain_stderr() -> None:
                if proc.stderr:
                    _stderr_chunks.append(proc.stderr.read())

            _stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
            _stderr_thread.start()

            # Parse the `--json` ThreadEvent JSONL stream.
            thread_id: str | None = None
            agent_message = ""
            tokens_in = 0
            tokens_out = 0
            turn_error: str | None = None

            for raw in proc.stdout or ():
                line = raw.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                ev_type = ev.get("type")
                if ev_type == "thread.started":
                    if thread_id is None:
                        thread_id = ev.get("thread_id") or None
                        if thread_id:
                            # Write the thread id immediately so the Rust watcher
                            # thread can push it to the DB before the session
                            # completes -- same side-channel file and
                            # RALPHUS_EVENT marker claude_code_backend.py uses,
                            # which is what lets `runner.rs`'s existing tmux-pane
                            # event forwarder and auto-reattach retry work for
                            # Codex with no daemon-side changes.
                            with contextlib.suppress(OSError):
                                sid_path.write_text(thread_id, encoding="utf-8")
                            print(
                                f"ralphus [llm-invoke] codex thread-id={thread_id}",
                                file=sys.stderr,
                            )
                            cartographer.emit(
                                "llm-invoke",
                                "codex thread-id known",
                                level="debug",
                                payload={"agent_session_id": thread_id},
                            )
                elif ev_type == "item.completed":
                    item = ev.get("item") or {}
                    item_type = item.get("type")
                    if item_type == "agent_message":
                        # The model's own reply -- the whole point of the live
                        # tmux pane is to let a human read this, and it's also
                        # what `_parse_verdict()` scans for the
                        # RALPHUS_VERIFY: marker, so keep the latest one.
                        text = item.get("text", "")
                        if text:
                            agent_message = text
                            print(text)
                    elif item_type == "command_execution":
                        command = item.get("command", "")
                        status = item.get("status", "")
                        print(f"[tool] exec({command!r}) status={status}", file=sys.stderr)
                    elif item_type == "error":
                        print(f"[error] {item.get('message', '')}", file=sys.stderr)
                elif ev_type == "turn.completed":
                    usage = ev.get("usage") or {}
                    tokens_in = int(usage.get("input_tokens") or 0)
                    tokens_out = int(usage.get("output_tokens") or 0)
                elif ev_type == "turn.failed":
                    err = ev.get("error") or {}
                    turn_error = str(err.get("message") or "codex turn failed")
                elif ev_type == "error":
                    turn_error = str(ev.get("message") or "codex error")

            proc.wait()
            _stdin_thread.join()
            _stderr_thread.join()
            stderr_str = _stderr_chunks[0] if _stderr_chunks else ""
        finally:
            if not keep_files:
                prompt_file.unlink(missing_ok=True)
            # Always remove the side-channel file -- the Rust watcher will have
            # already read it, and leaving it on disk would confuse the next run.
            sid_path.unlink(missing_ok=True)

        elapsed = time.monotonic() - t0
        if proc.returncode != 0:
            detail = turn_error or (stderr_str.strip()[-500:] if stderr_str else "")
            print(
                f"ralphus [llm-invoke] codex error: exited {proc.returncode}: {detail[:120]}",
                file=sys.stderr,
            )
            raise BackendError(f"Codex exited {proc.returncode}: {detail}")
        print(
            f"ralphus [llm-invoke] codex done elapsed={elapsed:.2f}s"
            f" tokens_in={tokens_in} tokens_out={tokens_out}",
            file=sys.stderr,
        )
        # Trailing chars (not leading) so a marker on the model's final output
        # line -- e.g. RALPHUS_VERIFY: PASS/FAIL -- survives truncation.
        return BackendOutcome(
            summary=agent_message[-2000:],
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=0.0,
            agent_session_id=thread_id,
        )

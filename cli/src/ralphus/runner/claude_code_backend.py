"""Claude Code CLI backend.

Drives the ``claude`` CLI headlessly (``claude -p``) instead of the Anthropic
API, so it runs on your existing Claude Code login (e.g. a Max/Pro
subscription) with **no API key and no per-token billing**.

``--dangerously-skip-permissions`` lets the agent edit files and run commands
without interactive prompts, which is what makes unattended task execution work
(the same approach the predecessor used). The ``claude`` program can be
overridden with ``RALPHUS_CLAUDE_COMMAND`` (also used by every ``ralphus
quick-start manager|reviewer claude-code`` entrypoint and validated by
``ralphus check health`` -- RAL-110/166).

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
import threading
import time

from ralphus import shellcmd
from ralphus.config import load_config
from ralphus.runner import cartographer
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.cli_agent_common import RESUME_CONTINUATION_PROMPT, live_session_path
from ralphus.runner.cli_agent_common import write_prompt_file as _write_prompt_file
from ralphus.runner.tools import Workspace

__all__ = ["ClaudeCodeBackend", "live_session_path"]


def _format_tool_input(tool_input: dict[str, object]) -> str:
    """Render a ``tool_use`` block's input compactly for the live tmux pane (RAL-102)."""
    parts = []
    for key, value in tool_input.items():
        text = str(value)
        if len(text) > 80:
            text = text[:80] + "…"
        parts.append(f"{key}={text!r}")
    return ", ".join(parts)


# RAL-161: per-million-token USD rates (input, output), used to estimate a
# session's *live* running cost from cumulative tokens as stream-json events
# arrive -- the CLI's `total_cost_usd` is only ever reported once, in the
# terminal "result" event (see the `elif ev_type == "result":` branch below),
# so there is no authoritative live cost figure to read. Matched against the
# model string by substring (loosest-specific-wins is not needed here since
# entries don't overlap in practice). An unrecognized/future model falls back
# to Opus-tier rates -- a deliberately conservative (higher) estimate, since
# this feeds the RAL-161 max-cost kill switch and overestimating spend triggers
# the safety kill sooner rather than later.
_MODEL_PRICING_PER_MTOK: list[tuple[str, float, float]] = [
    ("haiku", 1.00, 5.00),
    ("sonnet", 3.00, 15.00),
    ("fable", 10.00, 50.00),
    ("mythos", 10.00, 50.00),
    ("opus", 5.00, 25.00),
]
_DEFAULT_PRICING_PER_MTOK = (5.00, 25.00)  # Opus-tier fallback


def _estimate_cost_usd(model: str | None, tokens_in: int, tokens_out: int) -> float:
    """Estimate USD cost from cumulative tokens using a per-model rate table.

    Ignores prompt-caching discounts (cache reads/writes are billed below the
    plain input rate), so this is a conservative overestimate of the true
    cost -- appropriate for a live figure that only needs to be "close enough,
    biased high" until the authoritative `total_cost_usd` arrives at session
    completion (RAL-161).
    """
    model_lower = (model or "").lower()
    input_rate, output_rate = _DEFAULT_PRICING_PER_MTOK
    for needle, in_rate, out_rate in _MODEL_PRICING_PER_MTOK:
        if needle in model_lower:
            input_rate, output_rate = in_rate, out_rate
            break
    return (tokens_in / 1_000_000) * input_rate + (tokens_out / 1_000_000) * output_rate


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


def _is_compound_command(value: str) -> bool:
    """Treat raw shell syntax as compound; a single wrapped path is not compound."""
    stripped = value.strip()
    if " " not in stripped:
        return False
    if len(stripped) >= 2 and stripped[0] == stripped[-1] and stripped[0] in "'\"":
        inner = stripped[1:-1]
        if stripped[0] not in inner:
            return False
    return True


class ClaudeCodeBackend:
    """A ModelBackend that runs the Claude Code CLI in headless print mode."""

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
        resume_agent_session_id: str | None = None,
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

        When ``resume_agent_session_id`` is set (the daemon's tmux
        auto-reattach retry -- see `daemon/src/runner.rs`'s
        `run_via_tmux`/`PSMUX_CRASH_NOTES.local.md`), ``--resume <id>`` is
        added and the original ``prompt`` is replaced with a short
        continuation directive: on a genuine tmux-pane loss the original
        conversation already has the real task instructions in its history,
        so re-sending the full original prompt as a *new* user turn would be
        redundant (and could confuse the model into thinking it's a distinct,
        second request) -- all that is needed is a nudge to keep going.
        """
        raw_command = os.environ.get("RALPHUS_CLAUDE_COMMAND", "claude")
        config = load_config()
        prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:8]
        if resume_agent_session_id:
            print(
                f"ralphus [llm-invoke] claude-code RESUME start"
                f" resume_from={resume_agent_session_id} prompt_len={len(prompt)}"
                f" prompt_hash={prompt_hash} model={model!r}",
                file=sys.stderr,
            )
            cartographer.emit(
                "llm-invoke",
                "claude-code resuming after dropped tmux session",
                level="warning",
                payload={
                    "resume_from": resume_agent_session_id,
                    "original_prompt_hash": prompt_hash,
                    "original_prompt_len": len(prompt),
                },
            )
        else:
            print(
                f"ralphus [llm-invoke] claude-code start prompt_len={len(prompt)}"
                f" prompt_hash={prompt_hash} model={model!r}",
                file=sys.stderr,
            )
        t0 = time.monotonic()
        keep_files = config.daemon.keep_temporary_files
        effective_prompt = RESUME_CONTINUATION_PROMPT if resume_agent_session_id else prompt
        prompt_file = _write_prompt_file(effective_prompt)
        system_prompt_file = (
            _write_prompt_file(append_system_prompt) if append_system_prompt else None
        )
        sid_path = live_session_path(workspace.root)
        # Remove any stale file from a previous run so the Rust watcher does
        # not see an old session ID before the new one arrives.
        sid_path.unlink(missing_ok=True)
        try:
            extra_args = [
                "-p",
                f"@{prompt_file}",
                "--dangerously-skip-permissions",
                "--verbose",
                "--output-format",
                "stream-json",
            ]
            if resume_agent_session_id:
                extra_args += ["--resume", resume_agent_session_id]
            if model:
                extra_args += ["--model", model]
            if _is_compound_command(raw_command):
                # Compound launchers go through a real shell command line.
                # Multiline system-prompt text is therefore shell-sensitive on
                # Windows, so route it through Claude Code's file-based flag.
                if system_prompt_file is not None:
                    extra_args += ["--append-system-prompt-file", str(system_prompt_file)]
                shell = shellcmd.resolve_shell(os.environ.get("RALPHUS_SHELL"))
                line = shellcmd.build_compound_command_line(shell, raw_command, extra_args)
                cmd, use_shell = shellcmd.shell_spawn_args(shell, line)
                launch_target = raw_command
            else:
                # Direct exec never round-trips through a shell, so the inline
                # text flag remains safe here.
                if append_system_prompt:
                    extra_args += ["--append-system-prompt", append_system_prompt]
                # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably.
                program = shutil.which(raw_command) or raw_command
                cmd = [program, *extra_args]
                use_shell = False
                launch_target = program
            try:
                proc = subprocess.Popen(
                    cmd,
                    shell=use_shell,
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
                raise BackendError(
                    f"could not run Claude Code ({launch_target!r}): {exc}"
                ) from exc

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
            # RAL-161: running totals accumulated across assistant turns, for
            # the live cost/token side channel -- distinct from tokens_in/
            # tokens_out/cost_usd above, which stay at 0 until the terminal
            # "result" event provides the real, authoritative numbers.
            live_tokens_in = 0
            live_tokens_out = 0

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
                            # to the session's own agent_session_id column right away.
                            # Without this, "Open Agent" stayed disabled in the board
                            # until the whole session finished, even though the id was
                            # known and printed to the live pane from the very start.
                            cartographer.emit(
                                "llm-invoke",
                                "claude-code session-id known",
                                level="debug",
                                payload={"agent_session_id": session_id},
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
                    # RAL-161: each assistant turn's own "usage" object reports
                    # that turn's full request size (Claude Code resends the
                    # whole growing conversation on every turn), so summing it
                    # across turns approximates real cumulative spend -- see
                    # `_estimate_cost_usd`'s docstring for why this leans
                    # conservative (a safety kill switch should overestimate,
                    # not underestimate). Forwarded over the same RALPHUS_EVENT
                    # marker as claude_session_id above so the daemon's
                    # existing tmux-pane event forwarder picks it up with no
                    # new plumbing.
                    usage = message.get("usage") or {}
                    turn_tokens_in = int(usage.get("input_tokens") or 0)
                    turn_tokens_out = int(usage.get("output_tokens") or 0)
                    if turn_tokens_in or turn_tokens_out:
                        live_tokens_in += turn_tokens_in
                        live_tokens_out += turn_tokens_out
                        live_cost_usd = _estimate_cost_usd(model, live_tokens_in, live_tokens_out)
                        cartographer.emit(
                            "llm-invoke",
                            "claude-code live usage",
                            level="debug",
                            payload={
                                "tokens_in": live_tokens_in,
                                "tokens_out": live_tokens_out,
                                "cost_usd": live_cost_usd,
                            },
                        )
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
                    # Trailing chars (not leading) so a marker on the model's final
                    # output line -- e.g. RALPHUS_VERIFY: PASS/FAIL -- survives
                    # truncation. Mirrors codex_backend.py's agent_message[-2000:].
                    result_summary = str(ev.get("result", ""))[-2000:]
                    session_id = ev.get("session_id") or session_id
                    # Token counts live under the nested "usage" object (there is
                    # no top-level total_input_tokens/total_output_tokens field) --
                    # confirmed against real `claude -p --output-format
                    # stream-json` output, whose terminal "result" event usage
                    # object looks like {"input_tokens": N, "output_tokens": N, ...}.
                    usage = ev.get("usage") or {}
                    tokens_in = int(usage.get("input_tokens") or 0)
                    tokens_out = int(usage.get("output_tokens") or 0)
                    # stream-json uses total_cost_usd; fall back to cost_usd for
                    # older builds that used the json format field name.
                    cost_usd = float(ev.get("total_cost_usd") or ev.get("cost_usd") or 0.0)

            proc.wait()
            _stderr_thread.join()
            stderr_str = _stderr_chunks[0] if _stderr_chunks else ""
        finally:
            if not keep_files:
                prompt_file.unlink(missing_ok=True)
                if system_prompt_file is not None:
                    system_prompt_file.unlink(missing_ok=True)
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
            agent_session_id=session_id,
        )

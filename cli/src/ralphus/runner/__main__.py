"""Runner entry point: read a SessionSpec (JSON) and emit a SessionResult (JSON).

The daemon invokes ``ralphus-runner`` (or ``python -m ralphus.runner``) with the
spec either as a file-path argument or on stdin, and reads the single-line JSON
result from stdout.

RAL-102: when tmux-wrapped, the daemon can't read this process's stdout as a
pipe (it lands in a tmux pane instead), so an optional ``--result-file PATH``
argument switches the output side of the contract: the ``SessionResult`` is
written to that file instead of printed, and a sentinel line
(``RALPHUS_TMUX_DONE: <status>``) is printed to stdout in its place — the
daemon polls `capture-pane` for that marker (mirroring the existing
``RALPHUS_VERIFY:``/``RALPHUS_EVENT:`` marker-parsing idiom) and then reads
the result file. Without ``--result-file`` (plain subprocess invocation, and
every existing test), behavior is unchanged: the JSON result goes to stdout.
"""

from __future__ import annotations

import sys
from collections.abc import Sequence
from pathlib import Path

from ralphus.runner import cartographer, otel
from ralphus.runner.backend import ModelBackend
from ralphus.runner.execute import run_session
from ralphus.runner.spec import SessionResult, SessionSpec, SpecError

__all__ = ["TMUX_DONE_MARKER", "main"]

#: Printed to stdout (which lands in the tmux pane) once the result has been
#: written to ``--result-file``, so the daemon knows to stop polling and read
#: the file. See the module docstring for the full rationale.
TMUX_DONE_MARKER = "RALPHUS_TMUX_DONE"


def _read_spec_text(argv: Sequence[str]) -> str:
    if argv:
        return Path(argv[0]).read_text(encoding="utf-8")
    return sys.stdin.read()


def _parse_argv(argv: Sequence[str]) -> tuple[list[str], str | None]:
    """Split an optional ``--result-file PATH`` out of ``argv``.

    Returns the remaining positional args (the spec file path, if any) and
    the result-file path (``None`` when absent).
    """
    rest: list[str] = []
    result_file: str | None = None
    items = list(argv)
    i = 0
    while i < len(items):
        if items[i] == "--result-file" and i + 1 < len(items):
            result_file = items[i + 1]
            i += 2
            continue
        rest.append(items[i])
        i += 1
    return rest, result_file


def main(argv: Sequence[str] | None = None) -> int:
    """Run one session from a spec, emitting the JSON result. Returns 0 if done."""
    raw_args = list(sys.argv[1:] if argv is None else argv)
    args, result_file = _parse_argv(raw_args)

    def _finish(result: SessionResult) -> int:
        if result_file is not None:
            Path(result_file).write_text(result.to_json(), encoding="utf-8")
            print(f"{TMUX_DONE_MARKER}: {result.status}")
        else:
            print(result.to_json())
        return 0 if result.ok else 1

    try:
        spec = SessionSpec.from_json(_read_spec_text(args))
    except (SpecError, OSError) as exc:
        return _finish(SessionResult.failed(f"could not load session spec: {exc}"))

    # The backend is resolved lazily so command-only sessions (and CI without
    # pydantic-ai installed) never import it.
    backend: ModelBackend | None = None
    if spec.prompt is not None:
        backend = _load_backend(spec.agent, spec.args)
        if backend is None:
            # Only native model agents return None here — i.e. pydantic-ai is missing.
            return _finish(
                SessionResult.failed(
                    f"prompt session needs the native backend for agent '{spec.agent}', but "
                    "pydantic-ai is not installed in the runner environment. Install the "
                    "'runner' extra (e.g. `uv sync --extra runner`), or use a command session."
                )
            )

    print(
        f"ralphus [runner] invoked run={spec.run_id} session={spec.session_id}"
        f" agent={spec.agent!r} model={spec.model!r} verify={spec.verify}",
        file=sys.stderr,
    )
    cartographer.emit(
        "runner",
        f"invoked agent={spec.agent!r} model={spec.model!r} verify={spec.verify}",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={"agent": spec.agent, "model": spec.model, "verify": spec.verify},
    )
    otel.init("ralphus-runner")
    with otel.attach_trace_context(spec.trace_context), otel.start_span("runner.invoked") as span:
        span.set_attribute("run_id", spec.run_id)
        span.set_attribute("session_id", spec.session_id)
        span.set_attribute("agent", spec.agent)
        result = run_session(spec, backend)
        if result.ok:
            otel.mark_ok(span)
        else:
            otel.mark_error(span, result.error or "session failed")
    return _finish(result)


# Anthropic-API model agents (need ANTHROPIC_API_KEY) vs the Claude Code CLI
# (uses your Claude Code / Max subscription login, no API key).
_NATIVE_AGENTS = {"claude", "anthropic", "ollama"}
_CLAUDE_CODE_AGENTS = {"claude-code", "claude-cli"}
_CODEX_AGENTS = {"codex", "codex-cli"}


def _load_backend(agent: str, args: list[str]) -> ModelBackend | None:
    """Resolve a backend for ``agent``.

    ``claude-code``/``claude-cli`` drive the Claude Code CLI (subscription, no
    API key). ``codex``/``codex-cli`` drive the OpenAI Codex CLI. Native model
    agents use the pydantic-ai backend (None if it is not installed). Any other
    agent is treated as an external harness program.
    """
    name = agent.lower()
    if name in _CLAUDE_CODE_AGENTS:
        from ralphus.runner.claude_code_backend import ClaudeCodeBackend

        return ClaudeCodeBackend()
    if name in _CODEX_AGENTS:
        from ralphus.runner.codex_backend import CodexBackend

        return CodexBackend()
    if name in _NATIVE_AGENTS:
        try:
            from ralphus.runner.pydantic_backend import load_backend
        except ImportError:
            return None
        return load_backend(agent)
    from ralphus.runner.harness_backend import HarnessBackend

    return HarnessBackend(agent, args)


if __name__ == "__main__":
    sys.exit(main())

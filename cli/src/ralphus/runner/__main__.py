"""Runner entry point: read a SessionSpec (JSON) and emit a SessionResult (JSON).

The daemon invokes ``ralphus-runner`` (or ``python -m ralphus.runner``) with the
spec either as a file-path argument or on stdin, and reads the single-line JSON
result from stdout.
"""

from __future__ import annotations

import sys
from collections.abc import Sequence
from pathlib import Path

from ralphus.runner.backend import ModelBackend
from ralphus.runner.execute import run_session
from ralphus.runner.spec import SessionResult, SessionSpec, SpecError

__all__ = ["main"]


def _read_spec_text(argv: Sequence[str]) -> str:
    if argv:
        return Path(argv[0]).read_text(encoding="utf-8")
    return sys.stdin.read()


def main(argv: Sequence[str] | None = None) -> int:
    """Run one session from a spec, printing the JSON result. Returns 0 if done."""
    args = list(sys.argv[1:] if argv is None else argv)
    try:
        spec = SessionSpec.from_json(_read_spec_text(args))
    except (SpecError, OSError) as exc:
        result = SessionResult.failed(f"could not load session spec: {exc}")
        print(result.to_json())
        return 1

    # The backend is resolved lazily so command-only sessions (and CI without
    # pydantic-ai installed) never import it.
    backend: ModelBackend | None = None
    if spec.prompt is not None:
        backend = _load_backend(spec.agent, spec.args)
        if backend is None:
            # Only native model agents return None here — i.e. pydantic-ai is missing.
            result = SessionResult.failed(
                f"prompt session needs the native backend for agent '{spec.agent}', but "
                "pydantic-ai is not installed in the runner environment. Install the "
                "'runner' extra (e.g. `uv sync --extra runner`), or use a command session."
            )
            print(result.to_json())
            return 1

    print(
        f"ralphus [runner] invoked run={spec.run_id} session={spec.session_id}"
        f" agent={spec.agent!r} model={spec.model!r} verify={spec.verify}",
        file=sys.stderr,
    )
    result = run_session(spec, backend)
    print(result.to_json())
    return 0 if result.ok else 1


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

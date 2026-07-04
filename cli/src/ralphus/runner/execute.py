"""Session execution: turn a SessionSpec into a SessionResult.

A ``command`` session runs deterministically (no model needed) — its exit code
is the verdict. A ``prompt`` session is handed to a ModelBackend. This is the
single choke point where a session becomes real work.
"""

from __future__ import annotations

from ralphus.runner.backend import BackendError, ModelBackend
from ralphus.runner.spec import SessionResult, SessionSpec
from ralphus.runner.tools import ToolError, Workspace

__all__ = ["run_session"]


def run_session(spec: SessionSpec, backend: ModelBackend | None = None) -> SessionResult:
    """Execute one session.

    ``command`` sessions ignore ``backend``. ``prompt`` sessions require one; if
    none is supplied the session fails with a clear message (the real
    pydantic-ai backend is wired up by the caller).
    """
    try:
        workspace = Workspace.create(spec.cwd)
    except ToolError as exc:
        return SessionResult.failed(str(exc))

    if spec.command is not None:
        return _run_command(workspace, spec.command, spec.timeout_sec)

    if spec.prompt is not None:
        return _run_prompt(workspace, spec, backend)

    # Unreachable for a validated spec (SpecError enforces exactly-one).
    return SessionResult.failed("session has neither 'command' nor 'prompt'")


def _run_command(workspace: Workspace, command: str, timeout_sec: int | None) -> SessionResult:
    output = workspace.run_bash(command, timeout_sec)
    if output.ok:
        return SessionResult.done(summary=_tail(output.stdout))
    detail = _tail(output.stderr) or _tail(output.stdout)
    return SessionResult.failed(f"command exited {output.exit_code}", summary=detail)


def _run_prompt(
    workspace: Workspace, spec: SessionSpec, backend: ModelBackend | None
) -> SessionResult:
    if backend is None:
        return SessionResult.failed(
            "no model backend available for this prompt session "
            "(the native pydantic-ai backend is configured by the caller)"
        )
    try:
        outcome = backend.run(spec.prompt or "", workspace, model=spec.model)
    except BackendError as exc:
        return SessionResult.failed(f"model backend error: {exc}")
    return SessionResult.done(
        summary=outcome.summary,
        tokens_in=outcome.tokens_in,
        tokens_out=outcome.tokens_out,
        cost_usd=outcome.cost_usd,
    )


def _tail(text: str, limit: int = 2000) -> str:
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[-limit:]

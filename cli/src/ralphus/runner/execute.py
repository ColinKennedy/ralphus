"""Session execution: turn a SessionSpec into a SessionResult.

A ``command`` session runs deterministically (no model needed) — its exit code
is the verdict. A ``prompt`` session is handed to a ModelBackend. An `agent`-kind
verify step is also a prompt session (``spec.verify`` is set), but its prompt is
wrapped so the agent reports an explicit PASS/FAIL verdict, which is parsed back
out into ``SessionResult.verified``. This is the single choke point where a
session (or verify step) becomes real work.
"""

from __future__ import annotations

import re

from ralphus.runner.backend import BackendError, ModelBackend
from ralphus.runner.spec import SessionResult, SessionSpec
from ralphus.runner.tools import ToolError, Workspace

__all__ = ["run_session"]

_VERIFY_PASS = "RALPHUS_VERIFY: PASS"
_VERIFY_FAIL = "RALPHUS_VERIFY: FAIL"

_VERIFY_PROMPT_TEMPLATE = (
    "{prompt}\n\n"
    "This is a VERIFICATION step, not a normal task. Investigate whether the "
    "above holds, attempting to fix any problems you find so the check passes "
    "if you can reasonably do so. When you are done, your FINAL line of output "
    f"must be exactly one of:\n{_VERIFY_PASS}\n{_VERIFY_FAIL}\nwith nothing else "
    "on that line."
)

# Local models don't always put the marker alone on its own line (e.g. "...
# confirming RALPHUS_VERIFY: PASS."), so this searches for the marker anywhere
# in the text rather than requiring an exact whole-line match, and — if it
# appears more than once — trusts the last occurrence as the agent's final word.
_VERDICT_RE = re.compile(rf"({re.escape(_VERIFY_PASS)}|{re.escape(_VERIFY_FAIL)})")


def run_session(spec: SessionSpec, backend: ModelBackend | None = None) -> SessionResult:
    """Execute one session.

    ``command`` sessions ignore ``backend``. ``prompt`` sessions require one; if
    none is supplied the session fails with a clear message (the real
    pydantic-ai backend is wired up by the caller). A ``prompt`` session with
    ``verify`` set is an agent-kind verify step rather than a normal session.
    """
    try:
        workspace = Workspace.create(spec.cwd)
    except ToolError as exc:
        return SessionResult.failed(str(exc))

    if spec.command is not None:
        return _run_command(workspace, spec.command, spec.timeout_sec)

    if spec.prompt is not None:
        if spec.verify:
            return _run_verify(workspace, spec, backend)
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
        outcome = backend.run(
            spec.prompt or "",
            workspace,
            model=spec.model,
            append_system_prompt=spec.system_prompt,
        )
    except BackendError as exc:
        return SessionResult.failed(f"model backend error: {exc}")
    over = _budget_exceeded(spec, outcome.tokens_in, outcome.tokens_out)
    if over is not None:
        return SessionResult.failed(over, summary=outcome.summary)
    return SessionResult.done(
        summary=outcome.summary,
        tokens_in=outcome.tokens_in,
        tokens_out=outcome.tokens_out,
        cost_usd=outcome.cost_usd,
        claude_session_id=outcome.claude_session_id,
    )


def _run_verify(
    workspace: Workspace, spec: SessionSpec, backend: ModelBackend | None
) -> SessionResult:
    if backend is None:
        return SessionResult.failed(
            "no model backend available for this agent verify step "
            "(the native pydantic-ai backend is configured by the caller)"
        )
    wrapped_prompt = _VERIFY_PROMPT_TEMPLATE.format(prompt=spec.prompt or "")
    try:
        outcome = backend.run(
            wrapped_prompt,
            workspace,
            model=spec.model,
            append_system_prompt=spec.system_prompt,
        )
    except BackendError as exc:
        return SessionResult.failed(f"model backend error: {exc}")

    over = _budget_exceeded(spec, outcome.tokens_in, outcome.tokens_out)
    if over is not None:
        # A verify step that blows its token budget fails closed (not verified).
        return SessionResult.done(
            summary=f"{outcome.summary}\n{over}",
            tokens_in=outcome.tokens_in,
            tokens_out=outcome.tokens_out,
            cost_usd=outcome.cost_usd,
            verified=False,
        )
    verdict = _parse_verdict(outcome.summary)
    if verdict is None:
        passed = False
        note = f"(no {_VERIFY_PASS!r}/{_VERIFY_FAIL!r} marker found; treated as FAIL)"
        summary = f"{outcome.summary}\n{note}"
    else:
        passed = verdict
        summary = outcome.summary
    return SessionResult.done(
        summary=summary,
        tokens_in=outcome.tokens_in,
        tokens_out=outcome.tokens_out,
        cost_usd=outcome.cost_usd,
        verified=passed,
    )


def _budget_exceeded(spec: SessionSpec, tokens_in: int, tokens_out: int) -> str | None:
    """A failure message if the session's token budget is exceeded, else None.

    The budget caps total tokens (input + output). ``None``/non-positive means
    no cap. This is the runner-side cutoff for a runaway agent (RAL-15); the
    daemon separately enforces the wall-clock timeout with a guaranteed kill.
    """
    budget = spec.budget_tokens
    if budget is None or budget <= 0:
        return None
    total = tokens_in + tokens_out
    if total <= budget:
        return None
    return f"token budget exceeded: used {total} tokens > budget of {budget}"


def _parse_verdict(summary: str) -> bool | None:
    """Extract the agent's PASS/FAIL verdict, trusting the last occurrence."""
    matches = list(_VERDICT_RE.finditer(summary))
    if not matches:
        return None
    return matches[-1].group(1) == _VERIFY_PASS


def _tail(text: str, limit: int = 2000) -> str:
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[-limit:]

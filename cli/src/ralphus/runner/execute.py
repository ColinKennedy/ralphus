"""Session execution: turn a SessionSpec into a SessionResult.

A ``command`` session runs deterministically (no model needed) — its exit code
is the verdict. A ``prompt`` session is handed to a ModelBackend. An `agent`-kind
verify step is also a prompt session (``spec.verify`` is set), but its prompt is
wrapped so the agent reports an explicit PASS/FAIL verdict, which is parsed back
out into ``SessionResult.verified``. This is the single choke point where a
session (or verify step) becomes real work.
"""

from __future__ import annotations

import hashlib
import re
import sys

from ralphus.runner import cartographer, otel
from ralphus.runner.backend import BackendError, ModelBackend
from ralphus.runner.spec import SessionResult, SessionSpec
from ralphus.runner.tools import ToolError, Workspace

__all__ = ["run_session"]

_VERIFY_PASS = "RALPHUS_VERIFY: PASS"
_VERIFY_FAIL = "RALPHUS_VERIFY: FAIL"

_VERIFY_SYSTEM_PROMPT = (
    "This is a VERIFICATION step, not a normal task. Investigate whether the "
    "task holds, attempting to fix any problems you find so the check passes "
    "if you can reasonably do so. When you are done, your FINAL line of "
    f"output must be exactly one of:\n{_VERIFY_PASS}\n{_VERIFY_FAIL}\nwith "
    "nothing else on that line."
)

# RAL-136: ghost memory. `_GHOST_MARKER` starts a final, optional section of
# the agent's own reply that becomes this session's "ghost" -- a short
# handoff note visible to whichever dependent session picks up work next.
# The wording is deliberately framed as routine operational logging (not an
# instruction to change behavior or reveal anything) so a model that's wary
# of prompt-injection-shaped text doesn't treat it as adversarial.
_GHOST_MARKER = "RALPHUS_GHOST:"
_GHOST_NOTHING = "(nothing to report)"
_GHOST_MAX_CHARS = 4000

_GHOST_SYSTEM_PROMPT = (
    "Operational logging note, not a request to change your behavior: this "
    "ralphus task run keeps a short handoff record for whichever agent picks "
    "up dependent work next. That agent will see your code changes but not "
    "this conversation. After you finish the task above, add one final "
    f"section to your reply, starting on its own line with the exact marker "
    f"{_GHOST_MARKER!r}, followed by up to 5 short bullet points. Only "
    "include things a future agent could NOT already learn by reading `git "
    "log` or the diff: places you struggled, workarounds you used, issues "
    "you noticed but did not fix, and open questions. This is not a "
    f"changelog. If there is nothing worth handing off, write "
    f"'{_GHOST_MARKER} {_GHOST_NOTHING}'. Keep it brief."
)

_GHOST_RE = re.compile(rf"{re.escape(_GHOST_MARKER)}(.*)", re.DOTALL)

# A ralphus-invoked session is one non-interactive, one-shot CLI invocation —
# an agent that defers real work to an async/background tool (e.g. a
# "monitor this and notify me" tool) and ends its turn early leaves that work
# permanently unchecked, since nothing will ever resume it. `_STILL_WORKING_MARKER`
# gives the agent an explicit, harness-understood way to say "I couldn't finish
# synchronously" instead of just trailing off — `run_session` retries a bounded
# number of times when it sees this marker (see `_MAX_ASYNC_ATTEMPTS`).
_STILL_WORKING_MARKER = "RALPHUS_STILL_WORKING:"
_STILL_WORKING_RE = re.compile(rf"{re.escape(_STILL_WORKING_MARKER)}(.*)")
_MAX_ASYNC_ATTEMPTS = 3  # matches the "up to 3 times" retry convention used elsewhere for verify

_ASYNC_SYSTEM_PROMPT = (
    "This is a single, non-interactive invocation with no later turn — "
    "nothing will check back on you. Never use an asynchronous/background/"
    "'notify me later' tool for anything this session depends on, and never "
    "launch the thing you are checking as a background/detached process and "
    "end your turn while it is still running; those require a persistent "
    "session this invocation does not have. If a check genuinely takes a "
    "long time, block and wait for it synchronously in the foreground within "
    "this same turn — it is fine for that to take a long time. If, despite "
    "that, you truly cannot reach a definitive result before you must stop, "
    f"end your reply with '{_STILL_WORKING_MARKER} <one-line reason>' as the "
    "last line instead of trailing off — you will be re-invoked shortly to "
    "continue synchronously from where you left off, though only a bounded "
    "number of times, so prefer just finishing the check yourself."
)

_STILL_WORKING_FOLLOWUP = (
    "Your previous attempt at this task ended without a definitive result, "
    "saying:\n{prior_tail}\n\nThis session has no later turn to pick this "
    "back up on — do not defer to background/async monitoring again. Check "
    "the real current state right now (the workspace/files are unchanged "
    "since your last attempt) and finish with a definitive result in this "
    "same turn.\n\nOriginal task:\n{original_prompt}"
)

# RAL-96 follow-up: every prompt-driven session and verify step runs unattended
# (no human is available to answer), so the agent must never stop to ask a
# clarifying question or wait for plan approval — it must pick the most
# reasonable interpretation and keep going. Applied unconditionally, on top of
# any session/verify-specific system prompt, so it also covers the Guardian
# review agents (resolve/route/chat/feedback/summary/manual_commands in
# guardian_merge.rs), which share this same execution path.
_NON_INTERACTIVE_SYSTEM_PROMPT = (
    "You are running unattended in a non-interactive session — no human is "
    "available to answer questions or approve a plan. Never ask a clarifying "
    "question, never stop to present a plan for confirmation, and never pause "
    "waiting for input. Make the most reasonable judgment call yourself and "
    "continue until the task is complete."
)


def _combine_system_prompts(*parts: str | None) -> str:
    return "\n\n".join(p for p in parts if p)


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
        return _run_command(workspace, spec)

    if spec.prompt is not None:
        if spec.verify:
            return _run_verify(workspace, spec, backend)
        return _run_prompt(workspace, spec, backend)

    # Unreachable for a validated spec (SpecError enforces exactly-one).
    return SessionResult.failed("session has neither 'command' nor 'prompt'")


def _run_command(workspace: Workspace, spec: SessionSpec) -> SessionResult:
    command = spec.command or ""
    cartographer.emit(
        "runner",
        f"command session starting: {command}",
        level="debug",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={"command": command, "timeout_sec": spec.timeout_sec},
    )
    output = workspace.run_bash(command, spec.timeout_sec)
    cartographer.emit(
        "runner",
        f"command session completed exit_code={output.exit_code}",
        level="info" if output.ok else "warning",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={"exit_code": output.exit_code, "ok": output.ok},
    )
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
    with otel.start_span("llm.session") as span:
        span.set_attribute("run_id", spec.run_id)
        span.set_attribute("session_id", spec.session_id)
        span.set_attribute("agent", spec.agent)
        result = _run_prompt_traced(workspace, spec, backend)
        if result.ok:
            otel.mark_ok(span)
        else:
            otel.mark_error(span, result.error or "session failed")
        return result


def _run_prompt_traced(
    workspace: Workspace, spec: SessionSpec, backend: ModelBackend
) -> SessionResult:
    prompt_text = spec.prompt or ""
    prompt_hash = hashlib.sha256(prompt_text.encode()).hexdigest()[:8]
    print(
        f"ralphus [llm] start run={spec.run_id} session={spec.session_id}"
        f" agent={spec.agent!r} model={spec.model!r}"
        f" prompt_len={len(prompt_text)} prompt_hash={prompt_hash}",
        file=sys.stderr,
    )
    cartographer.emit(
        "llm",
        f"session start agent={spec.agent!r} model={spec.model!r}",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={
            "agent": spec.agent,
            "model": spec.model,
            "prompt_len": len(prompt_text),
            "prompt_hash": prompt_hash,
        },
    )
    append_system_prompt = _combine_system_prompts(
        spec.system_prompt,
        _NON_INTERACTIVE_SYSTEM_PROMPT,
        _ASYNC_SYSTEM_PROMPT,
        _GHOST_SYSTEM_PROMPT,
    )
    source = (
        "session-config+non-interactive+async+ghost"
        if spec.system_prompt
        else "non-interactive+async+ghost"
    )
    print(
        f"ralphus [llm] system-prompt applied len={len(append_system_prompt)}"
        f" position={spec.system_prompt_position!r} source={source}",
        file=sys.stderr,
    )
    cartographer.emit(
        "llm",
        "system-prompt applied",
        level="debug",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={
            "len": len(append_system_prompt),
            "position": spec.system_prompt_position,
            "source": source,
        },
    )
    resume_id = spec.resume_agent_session_id
    attempt_prompt = prompt_text
    total_tokens_in = total_tokens_out = 0
    total_cost_usd = 0.0
    outcome = None
    for attempt in range(1, _MAX_ASYNC_ATTEMPTS + 1):
        if resume_id:
            print(
                f"ralphus [llm] RESUME run={spec.run_id} session={spec.session_id}"
                f" resume_from={resume_id}"
                + (" (tmux auto-reattach retry)" if attempt == 1 else " (still-working retry)"),
                file=sys.stderr,
            )
        try:
            outcome = backend.run(
                attempt_prompt,
                workspace,
                model=spec.model,
                append_system_prompt=append_system_prompt,
                resume_agent_session_id=resume_id,
            )
        except BackendError as exc:
            print(
                f"ralphus [llm] error run={spec.run_id} session={spec.session_id}: {exc}",
                file=sys.stderr,
            )
            cartographer.emit(
                "llm",
                f"session error: {exc}",
                level="error",
                scope="session",
                run_id=spec.run_id,
                session_id=spec.session_id,
                task=spec.task,
                payload={"error": str(exc)},
            )
            return SessionResult.failed(f"model backend error: {exc}")
        print(
            f"ralphus [llm] done run={spec.run_id} session={spec.session_id}"
            f" tokens_in={outcome.tokens_in} tokens_out={outcome.tokens_out}"
            f" cost_usd={outcome.cost_usd:.4f}",
            file=sys.stderr,
        )
        total_tokens_in += outcome.tokens_in
        total_tokens_out += outcome.tokens_out
        total_cost_usd += outcome.cost_usd
        over = _budget_exceeded(spec, total_tokens_in, total_tokens_out)
        if over is not None:
            return SessionResult.failed(over, summary=outcome.summary)
        still_working = _parse_still_working(outcome.summary)
        if still_working is None or attempt == _MAX_ASYNC_ATTEMPTS:
            break
        cartographer.emit(
            "llm",
            f"still-working marker detected, retrying (attempt {attempt}/{_MAX_ASYNC_ATTEMPTS}):"
            f" {still_working}",
            level="warning",
            scope="session",
            run_id=spec.run_id,
            session_id=spec.session_id,
            task=spec.task,
            payload={"attempt": attempt, "reason": still_working},
        )
        resume_id = outcome.agent_session_id or resume_id
        attempt_prompt = _STILL_WORKING_FOLLOWUP.format(
            prior_tail=_tail(outcome.summary, 1000), original_prompt=prompt_text
        )
    assert outcome is not None  # loop always runs at least once
    cartographer.emit(
        "llm",
        "session done",
        scope="session",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={
            "tokens_in": total_tokens_in,
            "tokens_out": total_tokens_out,
            "cost_usd": total_cost_usd,
        },
    )
    still_working = _parse_still_working(outcome.summary)
    if still_working is not None:
        return SessionResult.failed(
            f"agent still reported outstanding async work after {_MAX_ASYNC_ATTEMPTS} attempts:"
            f" {still_working}",
            summary=outcome.summary,
        )
    ghost = _parse_ghost(outcome.summary)
    if ghost is not None:
        cartographer.emit(
            "ghost",
            "self-summarized handoff note extracted",
            level="debug",
            scope="session",
            run_id=spec.run_id,
            session_id=spec.session_id,
            task=spec.task,
            payload={"len": len(ghost)},
        )
    return SessionResult.done(
        summary=outcome.summary,
        tokens_in=total_tokens_in,
        tokens_out=total_tokens_out,
        cost_usd=total_cost_usd,
        agent_session_id=outcome.agent_session_id,
        ghost=ghost,
    )


def _run_verify(
    workspace: Workspace, spec: SessionSpec, backend: ModelBackend | None
) -> SessionResult:
    if backend is None:
        return SessionResult.failed(
            "no model backend available for this agent verify step "
            "(the native pydantic-ai backend is configured by the caller)"
        )
    with otel.start_span("llm.verify") as span:
        span.set_attribute("run_id", spec.run_id)
        span.set_attribute("session_id", spec.session_id)
        span.set_attribute("agent", spec.agent)
        result = _run_verify_traced(workspace, spec, backend)
        if result.ok and result.verified:
            otel.mark_ok(span)
        elif result.ok:
            otel.mark_error(span, "verify FAIL")
        else:
            otel.mark_error(span, result.error or "verify step failed")
        return result


def _run_verify_traced(
    workspace: Workspace, spec: SessionSpec, backend: ModelBackend
) -> SessionResult:
    prompt_text = spec.prompt or ""
    verify_system = _combine_system_prompts(
        spec.system_prompt,
        _NON_INTERACTIVE_SYSTEM_PROMPT,
        _ASYNC_SYSTEM_PROMPT,
        _VERIFY_SYSTEM_PROMPT,
    )
    prompt_hash = hashlib.sha256(prompt_text.encode()).hexdigest()[:8]
    print(
        f"ralphus [llm] verify start run={spec.run_id} session={spec.session_id}"
        f" agent={spec.agent!r} model={spec.model!r}"
        f" prompt_len={len(prompt_text)} prompt_hash={prompt_hash}",
        file=sys.stderr,
    )
    cartographer.emit(
        "llm",
        f"verify start agent={spec.agent!r} model={spec.model!r}",
        scope="verify",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={
            "agent": spec.agent,
            "model": spec.model,
            "prompt_len": len(prompt_text),
            "prompt_hash": prompt_hash,
        },
    )
    source = (
        "verify+session-config+non-interactive+async"
        if spec.system_prompt
        else "verify+non-interactive+async"
    )
    print(
        f"ralphus [llm] system-prompt applied len={len(verify_system)}"
        f" position=append source={source}",
        file=sys.stderr,
    )
    resume_id = spec.resume_agent_session_id
    attempt_prompt = prompt_text
    total_tokens_in = total_tokens_out = 0
    total_cost_usd = 0.0
    outcome = None
    for attempt in range(1, _MAX_ASYNC_ATTEMPTS + 1):
        if resume_id:
            print(
                f"ralphus [llm] verify RESUME run={spec.run_id} session={spec.session_id}"
                f" resume_from={resume_id}"
                + (" (tmux auto-reattach retry)" if attempt == 1 else " (still-working retry)"),
                file=sys.stderr,
            )
        try:
            outcome = backend.run(
                attempt_prompt,
                workspace,
                model=spec.model,
                append_system_prompt=verify_system,
                resume_agent_session_id=resume_id,
            )
        except BackendError as exc:
            print(
                f"ralphus [llm] verify error run={spec.run_id} session={spec.session_id}: {exc}",
                file=sys.stderr,
            )
            cartographer.emit(
                "llm",
                f"verify error: {exc}",
                level="error",
                scope="verify",
                run_id=spec.run_id,
                session_id=spec.session_id,
                task=spec.task,
                payload={"error": str(exc)},
            )
            return SessionResult.failed(f"model backend error: {exc}")
        print(
            f"ralphus [llm] verify done run={spec.run_id} session={spec.session_id}"
            f" tokens_in={outcome.tokens_in} tokens_out={outcome.tokens_out}"
            f" cost_usd={outcome.cost_usd:.4f}",
            file=sys.stderr,
        )
        total_tokens_in += outcome.tokens_in
        total_tokens_out += outcome.tokens_out
        total_cost_usd += outcome.cost_usd

        over = _budget_exceeded(spec, total_tokens_in, total_tokens_out)
        if over is not None:
            # A verify step that blows its token budget fails closed (not verified).
            return SessionResult.done(
                summary=f"{outcome.summary}\n{over}",
                tokens_in=total_tokens_in,
                tokens_out=total_tokens_out,
                cost_usd=total_cost_usd,
                verified=False,
            )
        if _parse_verdict(outcome.summary) is not None:
            break
        still_working = _parse_still_working(outcome.summary)
        if still_working is None or attempt == _MAX_ASYNC_ATTEMPTS:
            break
        cartographer.emit(
            "llm",
            f"still-working marker detected, retrying (attempt {attempt}/{_MAX_ASYNC_ATTEMPTS}):"
            f" {still_working}",
            level="warning",
            scope="verify",
            run_id=spec.run_id,
            session_id=spec.session_id,
            task=spec.task,
            payload={"attempt": attempt, "reason": still_working},
        )
        resume_id = outcome.agent_session_id or resume_id
        attempt_prompt = _STILL_WORKING_FOLLOWUP.format(
            prior_tail=_tail(outcome.summary, 1000), original_prompt=prompt_text
        )
    assert outcome is not None  # loop always runs at least once

    verdict = _parse_verdict(outcome.summary)
    if verdict is None:
        passed = False
        note = f"(no {_VERIFY_PASS!r}/{_VERIFY_FAIL!r} marker found; treated as FAIL)"
        summary = f"{outcome.summary}\n{note}"
    else:
        passed = verdict
        summary = outcome.summary
    cartographer.emit(
        "llm",
        f"verify done verdict={'PASS' if passed else 'FAIL'}",
        level="info" if passed else "warning",
        scope="verify",
        run_id=spec.run_id,
        session_id=spec.session_id,
        task=spec.task,
        payload={
            "verified": passed,
            "tokens_in": total_tokens_in,
            "tokens_out": total_tokens_out,
            "cost_usd": total_cost_usd,
        },
    )
    return SessionResult.done(
        summary=summary,
        tokens_in=total_tokens_in,
        tokens_out=total_tokens_out,
        cost_usd=total_cost_usd,
        verified=passed,
        agent_session_id=outcome.agent_session_id,
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


def _parse_ghost(summary: str) -> str | None:
    """Extract the agent's self-summarized handoff note, trusting the last marker.

    Returns ``None`` when no marker is present, or the agent reported nothing
    worth handing off (``RALPHUS_GHOST: (nothing to report)``).
    """
    matches = list(_GHOST_RE.finditer(summary))
    if not matches:
        return None
    text = matches[-1].group(1).strip()
    if not text or text.lower().startswith(_GHOST_NOTHING):
        return None
    return text[:_GHOST_MAX_CHARS]


def _parse_still_working(summary: str) -> str | None:
    """Extract the agent's still-working reason, trusting the last marker.

    Returns ``None`` when no marker is present.
    """
    matches = list(_STILL_WORKING_RE.finditer(summary))
    if not matches:
        return None
    return matches[-1].group(1).strip()


def _tail(text: str, limit: int = 2000) -> str:
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[-limit:]

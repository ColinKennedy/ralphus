"""The runner's wire contract: SessionSpec (in) and SessionResult (out).

Plain dataclasses with explicit JSON (de)serialization so the runner core has no
third-party dependency and type-checks cleanly under mypy --strict. The daemon
produces a SessionSpec JSON and consumes a SessionResult JSON.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

__all__ = ["SessionResult", "SessionSpec", "SpecError"]


class SpecError(ValueError):
    """Raised when an incoming session spec is malformed."""


def _require_str(data: dict[str, Any], key: str) -> str:
    value = data.get(key)
    if not isinstance(value, str):
        raise SpecError(f"field {key!r} must be a string")
    return value


def _opt_str(data: dict[str, Any], key: str) -> str | None:
    value = data.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        raise SpecError(f"field {key!r} must be a string or null")
    return value


def _opt_int(data: dict[str, Any], key: str) -> int | None:
    value = data.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise SpecError(f"field {key!r} must be an integer or null")
    return value


def _str_list(data: dict[str, Any], key: str) -> list[str]:
    value = data.get(key, [])
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise SpecError(f"field {key!r} must be an array of strings")
    return list(value)


def _opt_bool(data: dict[str, Any], key: str, default: bool = False) -> bool:
    value = data.get(key, default)
    if not isinstance(value, bool):
        raise SpecError(f"field {key!r} must be a boolean")
    return value


@dataclass
class SessionSpec:
    """Everything the runner needs to execute one session."""

    run_id: str
    task: str
    session_id: str
    cwd: str
    prompt: str | None = None
    command: str | None = None
    agent: str = "claude"
    model: str | None = None
    system_prompt: str | None = None
    system_prompt_position: str | None = None
    args: list[str] = field(default_factory=list)
    budget_tokens: int | None = None
    timeout_sec: int | None = None
    verify: bool = False
    # W3C `traceparent` of the OpenTelemetry span this session/verify run is a
    # child of (RAL-96), so the runner's own spans continue the same trace
    # instead of starting a disconnected one. `None` when the daemon has no
    # tracing configured (see `daemon/src/otel.rs`) — not a required field, so
    # existing callers/tests that don't supply it are unaffected.
    trace_context: str | None = None
    # When set, resume this exact conversation instead of starting a fresh
    # one -- the daemon's tmux auto-reattach retry sets this
    # on a retried attempt after a session's tmux pane vanished unexpectedly
    # mid-run but its agent_session_id was already captured live (see
    # `daemon/src/runner.rs::SubprocessRunner::run_via_tmux`). `None` for a
    # normal (first attempt) invocation.
    resume_agent_session_id: str | None = None

    @staticmethod
    def from_json(text: str) -> SessionSpec:
        """Parse a spec from JSON text, raising SpecError on any problem."""
        try:
            data = json.loads(text)
        except json.JSONDecodeError as exc:
            raise SpecError(f"invalid JSON: {exc}") from exc
        if not isinstance(data, dict):
            raise SpecError("spec must be a JSON object")
        spec = SessionSpec(
            run_id=_require_str(data, "run_id"),
            task=_require_str(data, "task"),
            session_id=_require_str(data, "session_id"),
            cwd=_require_str(data, "cwd"),
            prompt=_opt_str(data, "prompt"),
            command=_opt_str(data, "command"),
            agent=_opt_str(data, "agent") or "claude",
            model=_opt_str(data, "model"),
            system_prompt=_opt_str(data, "system_prompt"),
            system_prompt_position=_opt_str(data, "system_prompt_position"),
            args=_str_list(data, "args"),
            budget_tokens=_opt_int(data, "budget_tokens"),
            timeout_sec=_opt_int(data, "timeout_sec"),
            verify=_opt_bool(data, "verify"),
            trace_context=_opt_str(data, "trace_context"),
            resume_agent_session_id=_opt_str(data, "resume_agent_session_id"),
        )
        if (spec.prompt is None) == (spec.command is None):
            raise SpecError("exactly one of 'prompt' or 'command' must be set")
        return spec


@dataclass
class SessionResult:
    """The outcome of running a session."""

    status: str  # "done" | "failed"
    tokens_in: int = 0
    tokens_out: int = 0
    cost_usd: float = 0.0
    summary: str = ""
    error: str | None = None
    verified: bool | None = None
    agent_session_id: str | None = None
    # RAL-136: the agent's self-summarized handoff note ("ghost"), extracted
    # from a RALPHUS_GHOST: marker in a normal (non-verify) prompt session's
    # final response. None for command sessions, verify steps, or when the
    # agent had nothing to hand off.
    ghost: str | None = None

    @staticmethod
    def done(
        summary: str = "",
        *,
        tokens_in: int = 0,
        tokens_out: int = 0,
        cost_usd: float = 0.0,
        verified: bool | None = None,
        agent_session_id: str | None = None,
        ghost: str | None = None,
    ) -> SessionResult:
        """Construct a successful result. ``verified`` is set for verify runs only."""
        return SessionResult(
            status="done",
            summary=summary,
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=cost_usd,
            verified=verified,
            agent_session_id=agent_session_id,
            ghost=ghost,
        )

    @staticmethod
    def failed(error: str, summary: str = "") -> SessionResult:
        """Construct a failed result."""
        return SessionResult(status="failed", summary=summary, error=error)

    @property
    def ok(self) -> bool:
        """True when the session succeeded."""
        return self.status == "done"

    def to_json(self) -> str:
        """Serialize to JSON text (the runner's stdout contract)."""
        return json.dumps(
            {
                "status": self.status,
                "tokens_in": self.tokens_in,
                "tokens_out": self.tokens_out,
                "cost_usd": self.cost_usd,
                "summary": self.summary,
                "error": self.error,
                "verified": self.verified,
                "agent_session_id": self.agent_session_id,
                "ghost": self.ghost,
            }
        )

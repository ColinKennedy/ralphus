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


def _opt_float(data: dict[str, Any], key: str) -> float | None:
    value = data.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise SpecError(f"field {key!r} must be a number or null")
    return float(value)


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
    args: list[str] = field(default_factory=list)
    budget_usd: float | None = None
    timeout_sec: int | None = None

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
            args=_str_list(data, "args"),
            budget_usd=_opt_float(data, "budget_usd"),
            timeout_sec=_opt_int(data, "timeout_sec"),
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

    @staticmethod
    def done(
        summary: str = "", *, tokens_in: int = 0, tokens_out: int = 0, cost_usd: float = 0.0
    ) -> SessionResult:
        """Construct a successful result."""
        return SessionResult(
            status="done",
            summary=summary,
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=cost_usd,
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
            }
        )

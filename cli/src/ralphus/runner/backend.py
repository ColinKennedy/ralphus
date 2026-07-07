"""Model backend abstraction.

A backend drives an AI (``prompt``) session to completion using the workspace
tools. Keeping this behind a Protocol lets the deterministic runner core be
tested without a model, and lets the real backend (native pydantic-ai, arriving
next) be swapped in — and, later, alternative harnesses (see FOLLOW #1).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol, runtime_checkable

from ralphus.runner.tools import Workspace

__all__ = ["BackendError", "BackendOutcome", "ModelBackend"]


class BackendError(Exception):
    """Raised when a backend cannot complete a session."""


@dataclass
class BackendOutcome:
    """What a backend reports after driving a session."""

    summary: str
    tokens_in: int = 0
    tokens_out: int = 0
    cost_usd: float = 0.0
    claude_session_id: str | None = None


@runtime_checkable
class ModelBackend(Protocol):
    """Drives a prompt-based session against a model using workspace tools."""

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
    ) -> BackendOutcome:
        """Run ``prompt`` to completion, using ``workspace`` for file/shell tools.

        ``append_system_prompt``, when set, is delivered to the model as an
        *appended* system prompt via the backend's own mechanism (never
        concatenated into ``prompt``). Only the Claude Code backend supports it
        today; other backends accept the argument but ignore it, and TOML
        validation blocks non-``claude-code`` agents from setting it (RAL-5).
        """
        ...

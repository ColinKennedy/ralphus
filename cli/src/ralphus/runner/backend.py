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
    agent_session_id: str | None = None


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
        resume_agent_session_id: str | None = None,
    ) -> BackendOutcome:
        """Run ``prompt`` to completion, using ``workspace`` for file/shell tools.

        ``append_system_prompt``, when set, must be delivered to the model
        through *some* real mechanism appropriate to the backend's own
        harness -- a native flag (Claude Code's ``--append-system-prompt``), a
        config override (Codex's ``-c developer_instructions=...``), or, as a
        documented last resort for a harness with no instruction channel at
        all, folded into the prompt text itself (the same approach
        ``daemon/src/scheduler.rs`` already uses to deliver ghost-memory
        context, since that goes to every agent regardless of backend).
        Silently discarding it is not an acceptable implementation -- callers
        (``execute.py``) rely on this to deliver ralphus's own internal
        instructions (non-interactive mode, ghost-note requests, and the
        ``RALPHUS_VERIFY:`` verdict marker for agent-kind verify steps), and a
        backend that drops it makes those verify steps fail closed every time.
        TOML validation additionally allows *user-authored* ``system_prompt``
        only for backends declared in
        ``core::schema::agent_supports_system_prompt`` (RAL-5) -- keep that
        allow-list in sync when a backend gains a real delivery mechanism.

        ``resume_agent_session_id``, when set, tells the backend to resume
        that exact prior conversation instead of starting a fresh one --
        used by the daemon's tmux auto-reattach retry after a session's tmux
        pane vanishes unexpectedly mid-run. Implement this when the
        underlying harness supports resuming a prior session (Claude Code's
        ``--resume <id>``, Codex's ``exec resume <thread_id>``); a backend
        with no such concept accepts the argument and ignores it.
        """
        ...

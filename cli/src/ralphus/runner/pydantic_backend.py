"""Native pydantic-ai model backend.

Drives a prompt session by giving a pydantic-ai Agent the workspace tools
(read_file / write_file / run_bash) and letting it work against any model — the
Anthropic API for cloud runs, or a local Ollama model (through its
OpenAI-compatible endpoint) for offline runs and integration tests.

pydantic-ai is an optional dependency (the ``runner`` extra). It is imported
lazily so the rest of the CLI — and CI, which does not install it — never needs
it. mypy treats the import as untyped via the override in ``pyproject.toml``.
"""

from __future__ import annotations

import hashlib
import os
import sys
import time
from typing import Any

from opentelemetry.trace import SpanKind
from pydantic_ai import Agent

from ralphus.runner import cartographer, otel
from ralphus.runner.backend import BackendError, BackendOutcome
from ralphus.runner.tools import ToolError, Workspace

__all__ = ["PydanticAgentBackend", "load_backend"]

_DEFAULT_ANTHROPIC_MODEL = "claude-sonnet-4-5"
_DEFAULT_OLLAMA_MODEL = "qwen3:8b"
_DEFAULT_OLLAMA_URL = "http://localhost:11434/v1"

_SYSTEM_PROMPT = (
    "You are a headless coding agent. Accomplish the user's task by calling the "
    "provided tools (read_file, write_file, run_bash), operating only within the "
    "workspace. Do the minimum necessary, then reply with a one-line summary."
)


class PydanticAgentBackend:
    """A ModelBackend backed by pydantic-ai, selecting the provider from ``agent``."""

    def __init__(self, agent: str) -> None:
        self._agent = agent

    def run(
        self,
        prompt: str,
        workspace: Workspace,
        *,
        model: str | None,
        append_system_prompt: str | None = None,
        resume_agent_session_id: str | None = None,
    ) -> BackendOutcome:
        """Drive the prompt to completion using the workspace tools.

        ``append_system_prompt`` is applied as an *additional* system prompt
        (never concatenated into ``prompt``). This is best-effort: TOML
        validation currently blocks non-``claude-code`` agents from setting it,
        so it is not exercised in production yet (RAL-5).

        ``resume_agent_session_id`` is accepted for Protocol compatibility but
        not wired: the daemon's tmux auto-reattach retry only ever sets it for
        the ``claude-code`` agent, never a pydantic-ai-backed one.
        """
        llm = _build_model(self._agent, model)

        def read_file(path: str) -> str:
            """Read a UTF-8 text file within the workspace."""
            try:
                return workspace.read_file(path)
            except ToolError as exc:
                return f"error: {exc}"

        def write_file(path: str, content: str) -> str:
            """Write a UTF-8 text file within the workspace."""
            try:
                workspace.write_file(path, content)
            except ToolError as exc:
                return f"error: {exc}"
            return f"wrote {path}"

        def run_bash(command: str) -> str:
            """Run a shell command in the workspace and return its output."""
            out = workspace.run_bash(command)
            return f"exit={out.exit_code}\nstdout:\n{out.stdout}\nstderr:\n{out.stderr}"

        system_prompt: str | tuple[str, ...] = _SYSTEM_PROMPT
        if append_system_prompt:
            system_prompt = (_SYSTEM_PROMPT, append_system_prompt)
        agent = Agent(
            llm,
            system_prompt=system_prompt,
            tools=[read_file, write_file, run_bash],
        )
        prompt_hash = hashlib.sha256(prompt.encode()).hexdigest()[:8]
        print(
            f"ralphus [llm-invoke] start agent={self._agent!r} model={model!r}"
            f" prompt_len={len(prompt)} prompt_hash={prompt_hash}",
            file=sys.stderr,
        )
        cartographer.emit(
            "llm-invoke",
            f"agent.run_sync start agent={self._agent!r} model={model!r}",
            level="debug",
            payload={"prompt_len": len(prompt), "prompt_hash": prompt_hash},
        )
        t0 = time.monotonic()
        with otel.start_span("llm-invoke.agent_run", kind=SpanKind.CLIENT) as span:
            span.set_attribute("agent", self._agent)
            span.set_attribute("model", model or "")
            try:
                result = agent.run_sync(prompt)
            except Exception as exc:
                elapsed = time.monotonic() - t0
                otel.mark_error(span, str(exc))
                print(
                    f"ralphus [llm-invoke] error agent={self._agent!r}"
                    f" elapsed={elapsed:.2f}s: {exc}",
                    file=sys.stderr,
                )
                cartographer.emit(
                    "llm-invoke",
                    f"agent.run_sync error: {exc}",
                    level="error",
                    payload={"elapsed_sec": elapsed, "error": str(exc)},
                )
                raise BackendError(str(exc)) from exc
            otel.mark_ok(span)
        elapsed = time.monotonic() - t0

        usage = result.usage
        tokens_in = int(getattr(usage, "input_tokens", 0) or 0)
        tokens_out = int(getattr(usage, "output_tokens", 0) or 0)
        print(
            f"ralphus [llm-invoke] done agent={self._agent!r} elapsed={elapsed:.2f}s"
            f" tokens_in={tokens_in} tokens_out={tokens_out}",
            file=sys.stderr,
        )
        cartographer.emit(
            "llm-invoke",
            f"agent.run_sync done agent={self._agent!r}",
            level="debug",
            payload={
                "elapsed_sec": elapsed,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
            },
        )
        return BackendOutcome(
            summary=str(result.output)[:2000],
            tokens_in=tokens_in,
            tokens_out=tokens_out,
            cost_usd=0.0,
        )


def _build_model(agent: str, model: str | None) -> Any:
    """Construct the pydantic-ai model object for the given agent backend.

    Returns ``Any`` so the untyped model object is accepted by ``Agent(...)``
    whether or not pydantic-ai's real types are available to the type checker.
    """
    name = agent.lower()
    if name in ("claude", "anthropic"):
        from pydantic_ai.models.anthropic import AnthropicModel

        return AnthropicModel(model or _DEFAULT_ANTHROPIC_MODEL)
    if name == "ollama":
        from pydantic_ai.models.openai import OpenAIChatModel
        from pydantic_ai.providers.openai import OpenAIProvider

        base_url = os.environ.get("RALPHUS_OLLAMA_URL", _DEFAULT_OLLAMA_URL)
        provider = OpenAIProvider(base_url=base_url, api_key="ollama")
        return OpenAIChatModel(model or _DEFAULT_OLLAMA_MODEL, provider=provider)
    raise BackendError(
        f"agent backend {agent!r} is not supported by the native runner "
        "(use 'claude'/'anthropic' or 'ollama')"
    )


def load_backend(agent: str) -> PydanticAgentBackend:
    """Return a native backend for ``agent``.

    The caller imports this module inside a ``try/except ImportError`` so that a
    missing pydantic-ai (e.g. in CI) degrades gracefully.
    """
    return PydanticAgentBackend(agent)

"""pydantic-ai-backed TOML :class:`~ralphus.author.core.Generator`.

Drives an :class:`~pydantic_ai.Agent` (no tools — pure text generation) to turn
the authoring prompts into Task TOML. Token spend is bounded with pydantic-ai's
usage limits and wall-clock time is bounded by running the call on a worker
thread and joining against the remaining budget; either breach surfaces as
:class:`~ralphus.author.core.GeneratorAborted`.

pydantic-ai is the optional ``runner`` extra, imported lazily so the plain CLI
never needs it (mypy treats it as untyped via the override in ``pyproject.toml``).
"""

from __future__ import annotations

import os
import threading
from typing import Any

from ralphus.author.core import AuthorError, Budget, GenerateResult, GeneratorAborted

__all__ = ["PydanticGenerator", "load_generator"]

_DEFAULT_ANTHROPIC_MODEL = "claude-sonnet-4-5"
_DEFAULT_OLLAMA_MODEL = "qwen3:8b"
_DEFAULT_OLLAMA_URL = "http://localhost:11434/v1"


class PydanticGenerator:
    """A :class:`Generator` that authors TOML with a pydantic-ai model."""

    def __init__(self, agent: str, model: str | None = None) -> None:
        self._agent = agent
        self._model = model

    def generate(self, system_prompt: str, user_prompt: str, *, budget: Budget) -> GenerateResult:
        """Author TOML, bounding the call by the token and time budget."""
        from pydantic_ai import Agent

        try:
            agent = Agent(_build_model(self._agent, self._model), system_prompt=system_prompt)
        except AuthorError:
            raise
        except Exception as exc:
            raise AuthorError(f"could not initialize the authoring agent: {exc}") from exc

        usage_limits = _usage_limits(budget.remaining_tokens())
        holder: dict[str, Any] = {}

        def _work() -> None:
            try:
                if usage_limits is not None:
                    holder["result"] = agent.run_sync(user_prompt, usage_limits=usage_limits)
                else:
                    holder["result"] = agent.run_sync(user_prompt)
            except Exception as exc:  # re-raised on the caller thread below
                holder["error"] = exc

        thread = threading.Thread(target=_work, daemon=True)
        thread.start()
        remaining = budget.remaining_seconds()
        thread.join(remaining if remaining is not None and remaining > 0 else None)
        if thread.is_alive():
            raise GeneratorAborted(
                f"authoring agent exceeded its time budget ({self._model or self._agent})"
            )
        if "error" in holder:
            raise _as_author_error(holder["error"])

        result = holder["result"]
        usage = result.usage() if callable(getattr(result, "usage", None)) else result.usage
        return GenerateResult(
            tomls=[str(result.output)],
            tokens_in=_tokens(usage, "input_tokens", "request_tokens"),
            tokens_out=_tokens(usage, "output_tokens", "response_tokens"),
        )


def _as_author_error(exc: BaseException) -> AuthorError:
    """Map a model-call exception to an abort (budget) or a generic author error."""
    if type(exc).__name__ == "UsageLimitExceeded":
        return GeneratorAborted(f"authoring agent exceeded its token budget: {exc}")
    return AuthorError(f"authoring agent call failed: {exc}")


def _tokens(usage: object, *names: str) -> int:
    for name in names:
        value = getattr(usage, name, None)
        if value:
            return int(value)
    return 0


def _usage_limits(remaining_tokens: int | None) -> Any:
    """Build a pydantic-ai ``UsageLimits`` for the remaining token budget, if any."""
    if remaining_tokens is None:
        return None
    try:
        from pydantic_ai.usage import UsageLimits
    except ImportError:
        return None
    return UsageLimits(total_tokens_limit=remaining_tokens)


def _build_model(agent: str, model: str | None) -> Any:
    """Construct the pydantic-ai model object for the given agent backend."""
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
    raise AuthorError(
        f"agent backend {agent!r} is not supported by the authoring agent "
        "(use 'claude'/'anthropic' or 'ollama')"
    )


def load_generator(agent: str, model: str | None = None) -> PydanticGenerator:
    """Return a pydantic-ai generator for ``agent``/``model``.

    Callers import this lazily inside ``try/except ImportError`` so a missing
    pydantic-ai (e.g. in CI) degrades to a clear 'install the runner extra' error.
    """
    return PydanticGenerator(agent, model)

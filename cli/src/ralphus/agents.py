"""Registry of agent backends ralphus knows how to run (`ralphus agent list`).

There is no dynamic/runtime registry of agents today -- the daemon and runner
accept `agent` as a free-form string (see `cli/src/ralphus/runner/__main__.py`'s
`_NATIVE_AGENTS`/`_CLAUDE_CODE_AGENTS`/`_CODEX_AGENTS` dispatch sets, and
`core/src/schema.rs`'s `DEFAULT_AGENT`). This module is a hand-maintained,
purely informational catalog for `ralphus agent list` -- update it when a new
agent backend is added or a known agent's model constraints change.

For each entry, `models` is either a fixed tuple of the ONLY model names that
backend accepts, or `None` meaning it accepts any model string (passed through
unchecked to the underlying API/CLI).
"""

from __future__ import annotations

from dataclasses import dataclass

__all__ = ["KNOWN_AGENTS", "OTHER_AGENTS_NOTE", "AgentInfo"]


@dataclass(frozen=True)
class AgentInfo:
    """One agent backend: its aliases, description, and allowed models."""

    name: str
    aliases: tuple[str, ...]
    description: str
    # None means any model string is accepted (passed through unchecked).
    models: tuple[str, ...] | None
    default_model: str | None = None


KNOWN_AGENTS: tuple[AgentInfo, ...] = (
    AgentInfo(
        name="claude",
        aliases=("anthropic",),
        description="Native Anthropic API backend (pydantic-ai). Uses "
        "ANTHROPIC_API_KEY, or your Claude subscription via claude-code if unset.",
        models=None,
        default_model="claude-sonnet-4-5",
    ),
    AgentInfo(
        name="claude-code",
        aliases=("claude-cli",),
        description="Claude Code CLI, run as a subprocess (subscription or "
        "ANTHROPIC_API_KEY). Only the CLI's own model aliases are accepted.",
        models=("sonnet", "opus", "haiku", "fable"),
    ),
    AgentInfo(
        name="ollama",
        aliases=(),
        description="Local Ollama server. Any model you've pulled locally.",
        models=None,
        default_model="qwen3:8b",
    ),
    AgentInfo(
        name="codex",
        aliases=("codex-cli",),
        description="OpenAI Codex CLI, run as a subprocess.",
        models=None,
    ),
)

OTHER_AGENTS_NOTE = (
    'Any other agent name (e.g. "aider") runs as a generic external harness '
    "backend: <any model> is passed through via --model unchecked."
)

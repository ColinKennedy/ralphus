"""Tests for the hand-maintained agent/model registry (`ralphus agent list`)."""

from __future__ import annotations

from ralphus.agents import KNOWN_AGENTS, OTHER_AGENTS_NOTE


def test_every_agent_has_a_name_and_description() -> None:
    for a in KNOWN_AGENTS:
        assert a.name
        assert a.description


def test_agent_names_and_aliases_are_unique() -> None:
    seen: set[str] = set()
    for a in KNOWN_AGENTS:
        for token in (a.name, *a.aliases):
            assert token not in seen, f"duplicate agent name/alias: {token}"
            seen.add(token)


def test_claude_code_has_a_fixed_model_list() -> None:
    claude_code = next(a for a in KNOWN_AGENTS if a.name == "claude-code")
    assert claude_code.models is not None
    assert "sonnet" in claude_code.models
    assert "opus" in claude_code.models


def test_ollama_accepts_any_model() -> None:
    ollama = next(a for a in KNOWN_AGENTS if a.name == "ollama")
    assert ollama.models is None


def test_claude_accepts_any_model() -> None:
    claude = next(a for a in KNOWN_AGENTS if a.name == "claude")
    assert claude.models is None


def test_other_agents_note_is_non_empty() -> None:
    assert OTHER_AGENTS_NOTE

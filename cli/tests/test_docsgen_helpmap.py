"""Tests for `ralphus.docsgen.helpmap_docs` (RAL-110)."""

from __future__ import annotations

from pathlib import Path

import pytest

import ralphus.docsgen.helpmap_docs as helpmap_docs

pytestmark = pytest.mark.no_bench  # calls the real (~seconds) generate() via main()


def test_begin_end_markers_are_present_in_cli_reference() -> None:
    text = helpmap_docs.CLI_REFERENCE.read_text(encoding="utf-8")
    assert helpmap_docs.BEGIN_MARKER in text
    assert helpmap_docs.END_MARKER in text
    assert text.index(helpmap_docs.BEGIN_MARKER) < text.index(helpmap_docs.END_MARKER)


def test_splice_replaces_only_the_marked_block() -> None:
    original = f"before\n{helpmap_docs.BEGIN_MARKER}\nstale\n{helpmap_docs.END_MARKER}\nafter\n"
    updated = helpmap_docs._splice(original, "NEW-BLOCK")
    assert updated == "before\nNEW-BLOCK\nafter\n"


def test_splice_raises_without_markers() -> None:
    with pytest.raises(RuntimeError):
        helpmap_docs._splice("no markers here", "NEW-BLOCK")


def test_render_block_embeds_a_fenced_code_block() -> None:
    block = helpmap_docs.render_block()
    assert block.startswith(helpmap_docs.BEGIN_MARKER)
    assert block.endswith(helpmap_docs.END_MARKER)
    assert "```" in block
    assert "ralphus" in block


def test_main_check_is_a_noop_when_up_to_date() -> None:
    # The doc is regenerated as part of this ticket's own change, so it should
    # already be up to date -- --check must not exit nonzero nor write.
    mtime_before = helpmap_docs.CLI_REFERENCE.stat().st_mtime
    helpmap_docs.main(["--check"])
    assert helpmap_docs.CLI_REFERENCE.stat().st_mtime == mtime_before


def test_main_check_fails_on_drift(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(helpmap_docs, "generate", lambda: "DEFINITELY NOT WHAT'S THERE")
    with pytest.raises(SystemExit) as excinfo:
        helpmap_docs.main(["--check"])
    assert excinfo.value.code == 1


def test_main_writes_regenerated_block(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    scratch = tmp_path / "cli-reference.md"
    scratch.write_text(
        f"intro\n{helpmap_docs.BEGIN_MARKER}\nstale\n{helpmap_docs.END_MARKER}\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(helpmap_docs, "CLI_REFERENCE", scratch)
    monkeypatch.setattr(helpmap_docs, "generate", lambda: "FRESH")
    helpmap_docs.main([])
    updated = scratch.read_text(encoding="utf-8")
    assert "FRESH" in updated
    assert "stale" not in updated

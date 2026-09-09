<<<<<<< HEAD
"""Tests for board screenshot coverage discovery."""

from __future__ import annotations

from pathlib import Path

import pytest

import ralphus.docsgen.lint as lint


def test_board_tabs_reads_the_chunks_loaded_by_the_board_html(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    assets = tmp_path / "assets"
    board = assets / "board"
    board.mkdir(parents=True)
    html = assets / "board.html"
    html.write_text(
        '<script src="/board/00-first.js"></script>\n<script src="/board/10-tabs.js"></script>\n',
        encoding="utf-8",
    )
    (board / "00-first.js").write_text("const UNUSED = [];", encoding="utf-8")
    (board / "10-tabs.js").write_text('const TABS = ["one", "two"];', encoding="utf-8")
    monkeypatch.setattr(lint, "BOARD_HTML", html)
    monkeypatch.setattr(lint, "BOARD_ASSETS_DIR", assets)

    assert lint.board_tabs() == ["one", "two"]
=======
"""Unit tests for `docsgen.lint`'s tab extraction.

Tests only the parsing/scan logic with fixture files — never the real
board assets or the real screenshots directory, so these stay fast and
hermetic. (The full PASS/FAIL coverage run happens in the docs CI job
against the real librarian assets.)
"""

from __future__ import annotations

from typing import TYPE_CHECKING

import pytest

from ralphus.docsgen import lint

if TYPE_CHECKING:
    from pathlib import Path

TABS_JS = 'const TABS = ["squads", "reviews"];'


def _point_lint_at(
    monkeypatch: pytest.MonkeyPatch,
    board_html: Path,
    chunks_dir: Path,
) -> None:
    """Point the lint module's board-asset constants at fixture paths."""
    monkeypatch.setattr(lint, "BOARD_HTML", board_html)
    monkeypatch.setattr(lint, "BOARD_CHUNKS_DIR", chunks_dir)


def test_board_tabs_found_in_a_chunk_file(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    # board.html without the array (the JS lives in served chunk files);
    # the array sits in one chunk among several.
    board_html = tmp_path / "board.html"
    board_html.write_text('<html><script src="/board/25-chrome.js"></script></html>')
    chunks = tmp_path / "board"
    chunks.mkdir()
    (chunks / "00-typedefs.js").write_text("// no tabs here")
    (chunks / "25-chrome.js").write_text(TABS_JS)

    _point_lint_at(monkeypatch, board_html, chunks)
    assert lint.board_tabs() == ["squads", "reviews"]


def test_board_tabs_found_in_board_html_when_no_chunks(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # board.html still carries the array (empty chunk dir).
    board_html = tmp_path / "board.html"
    board_html.write_text(f"<html><script>{TABS_JS}</script></html>")
    chunks = tmp_path / "board"
    chunks.mkdir()

    _point_lint_at(monkeypatch, board_html, chunks)
    assert lint.board_tabs() == ["squads", "reviews"]


def test_board_tabs_raises_when_no_array_anywhere(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    board_html = tmp_path / "board.html"
    board_html.write_text("<html></html>")
    chunks = tmp_path / "board"
    chunks.mkdir()
    (chunks / "25-chrome.js").write_text("// no tabs here")

    _point_lint_at(monkeypatch, board_html, chunks)
    with pytest.raises(RuntimeError, match="could not find `const TABS"):
        lint.board_tabs()
>>>>>>> 446a4499 (docs(RAL-384): tabulate ralphus special syntax and reply markers)

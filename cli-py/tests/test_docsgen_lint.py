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

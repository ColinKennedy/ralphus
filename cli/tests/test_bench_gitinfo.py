"""Tests for RAL-94 git commit/dirty-state lookup."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from ralphus.bench.gitinfo import current_git_state


def _git(args: list[str], cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


@pytest.fixture
def temp_repo(tmp_path: Path) -> Path:
    _git(["init", "--quiet"], cwd=tmp_path)
    _git(["config", "user.email", "test@example.com"], cwd=tmp_path)
    _git(["config", "user.name", "Test"], cwd=tmp_path)
    (tmp_path / "a.txt").write_text("hello", encoding="utf-8")
    _git(["add", "."], cwd=tmp_path)
    _git(["commit", "--quiet", "-m", "initial"], cwd=tmp_path)
    return tmp_path


def test_reads_commit_sha(temp_repo: Path) -> None:
    state = current_git_state(temp_repo)
    assert len(state.commit) == 40
    assert not state.dirty


def test_detects_dirty_working_tree(temp_repo: Path) -> None:
    clean = current_git_state(temp_repo)

    (temp_repo / "a.txt").write_text("changed", encoding="utf-8")
    dirty = current_git_state(temp_repo)

    assert dirty.dirty
    assert dirty.commit == clean.commit


def test_non_repo_directory_raises(tmp_path: Path) -> None:
    with pytest.raises(RuntimeError):
        current_git_state(tmp_path)

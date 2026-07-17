"""Tests for the ralphus configuration loader."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from ralphus.config import Config, TaskConfig, _apply, _find_git_root, load_config

# ---------------------------------------------------------------------------
# _find_git_root
# ---------------------------------------------------------------------------


def test_find_git_root_finds_dot_git(tmp_path_factory: pytest.TempPathFactory) -> None:
    # Fresh directory per call (not a fixed tmp_path) so this test tolerates
    # being invoked more than once in-process by the RAL-94 bench harness.
    tmp_path = tmp_path_factory.mktemp("find_git_root")
    (tmp_path / ".git").mkdir()
    assert _find_git_root(tmp_path) == tmp_path


def test_find_git_root_walks_upward(tmp_path_factory: pytest.TempPathFactory) -> None:
    tmp_path = tmp_path_factory.mktemp("find_git_root_upward")
    (tmp_path / ".git").mkdir()
    child = tmp_path / "a" / "b"
    child.mkdir(parents=True)
    assert _find_git_root(child) == tmp_path


def test_find_git_root_returns_none_when_absent(tmp_path: Path) -> None:
    assert _find_git_root(tmp_path) is None


# ---------------------------------------------------------------------------
# _apply
# ---------------------------------------------------------------------------


def test_apply_overrides_maximum_timeout_seconds() -> None:
    base = TaskConfig(maximum_timeout_seconds=1800)
    result = _apply(base, {"task": {"maximum_timeout_seconds": 600}})
    assert result.maximum_timeout_seconds == 600


def test_apply_ignores_missing_task_section() -> None:
    base = TaskConfig(maximum_timeout_seconds=1800)
    result = _apply(base, {})
    assert result.maximum_timeout_seconds == 1800


def test_apply_ignores_non_int_maximum_timeout_seconds() -> None:
    base = TaskConfig(maximum_timeout_seconds=1800)
    result = _apply(base, {"task": {"maximum_timeout_seconds": "fast"}})
    assert result.maximum_timeout_seconds == 1800


def test_apply_accepts_zero() -> None:
    base = TaskConfig(maximum_timeout_seconds=1800)
    result = _apply(base, {"task": {"maximum_timeout_seconds": 0}})
    assert result.maximum_timeout_seconds == 0


def test_apply_accepts_negative() -> None:
    base = TaskConfig(maximum_timeout_seconds=1800)
    result = _apply(base, {"task": {"maximum_timeout_seconds": -1}})
    assert result.maximum_timeout_seconds == -1


# ---------------------------------------------------------------------------
# Config.subprocess_timeout
# ---------------------------------------------------------------------------


def test_subprocess_timeout_positive() -> None:
    c = Config(task=TaskConfig(maximum_timeout_seconds=3600))
    assert c.subprocess_timeout() == 3600.0


def test_subprocess_timeout_zero_is_none() -> None:
    c = Config(task=TaskConfig(maximum_timeout_seconds=0))
    assert c.subprocess_timeout() is None


def test_subprocess_timeout_negative_is_none() -> None:
    c = Config(task=TaskConfig(maximum_timeout_seconds=-1))
    assert c.subprocess_timeout() is None


# ---------------------------------------------------------------------------
# load_config
# ---------------------------------------------------------------------------


def test_load_config_defaults_when_no_files(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.delenv("RALPHUS_CONFIGURATION_PATH", raising=False)
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: None)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 1800
    assert config.sources == []


def test_load_config_reads_env_var_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg = tmp_path / ".ralphus.toml"
    cfg.write_text("[task]\nmaximum_timeout_seconds = 900\n", encoding="utf-8")
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", str(cfg))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: None)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 900
    assert cfg in config.sources


def test_load_config_later_env_var_wins(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    first = tmp_path / "first.toml"
    second = tmp_path / "second.toml"
    first.write_text("[task]\nmaximum_timeout_seconds = 300\n", encoding="utf-8")
    second.write_text("[task]\nmaximum_timeout_seconds = 600\n", encoding="utf-8")
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", os.pathsep.join([str(first), str(second)]))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: None)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 600


def test_load_config_git_root_wins_over_env_var(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    env_cfg = tmp_path / "env.toml"
    git_cfg = tmp_path / ".ralphus.toml"
    env_cfg.write_text("[task]\nmaximum_timeout_seconds = 300\n", encoding="utf-8")
    git_cfg.write_text("[task]\nmaximum_timeout_seconds = 9999\n", encoding="utf-8")
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", str(env_cfg))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: tmp_path)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 9999
    assert git_cfg in config.sources


def test_load_config_missing_env_var_file_is_ignored(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    missing = tmp_path / "no-such-file.toml"
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", str(missing))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: None)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 1800
    assert config.sources == []


def test_load_config_bad_toml_is_ignored(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    cfg = tmp_path / ".ralphus.toml"
    cfg.write_bytes(b"[[[not valid toml")
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", str(cfg))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: None)
    config = load_config()
    assert config.task.maximum_timeout_seconds == 1800
    assert config.sources == []


def test_load_config_git_root_not_in_env_var_not_duplicated(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    git_cfg = tmp_path / ".ralphus.toml"
    git_cfg.write_text("[task]\nmaximum_timeout_seconds = 42\n", encoding="utf-8")
    monkeypatch.setenv("RALPHUS_CONFIGURATION_PATH", str(git_cfg))
    monkeypatch.setattr("ralphus.config._find_git_root", lambda _start: tmp_path)
    config = load_config()
    # Same file listed in both env var and git root — should appear only once.
    assert config.sources.count(git_cfg) == 1
    assert config.task.maximum_timeout_seconds == 42

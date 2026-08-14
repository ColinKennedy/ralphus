"""Tests for `ralphus quick-start claude-code` (RAL-110)."""

from __future__ import annotations

import argparse
import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

import ralphus.__main__ as cli
import ralphus.helpmap as helpmap

pytestmark = pytest.mark.no_bench  # spawns a subprocess per test


class _FakeCompleted:
    def __init__(self, returncode: int = 0) -> None:
        self.returncode = returncode


@pytest.fixture(autouse=True)
def _fake_help_map(monkeypatch: pytest.MonkeyPatch) -> None:
    # Skip the real (~seconds-long) recursive CLI walk in every test here.
    monkeypatch.setattr(helpmap, "generate", lambda: "ralphus\n  {a fake help-map}")


def test_split_passthrough_with_separator() -> None:
    ralphus_args, passthrough = cli._split_passthrough(
        ["quick-start", "claude-code", "--", "c", "d"]
    )
    assert ralphus_args == ["quick-start", "claude-code"]
    assert passthrough == ["c", "d"]


def test_split_passthrough_without_separator() -> None:
    ralphus_args, passthrough = cli._split_passthrough(["quick-start", "claude-code"])
    assert ralphus_args == ["quick-start", "claude-code"]
    assert passthrough == []


def test_split_passthrough_leaves_other_commands_untouched() -> None:
    # A "--" that isn't part of a `quick-start` invocation must reach argparse
    # unchanged -- e.g. the standard end-of-options idiom for an arbitrary
    # positional value (RAL-110 code review: a global split silently ate this).
    raw = ["review", "chat", "send", "@myreview", "--", "-1 point deduction"]
    ralphus_args, passthrough = cli._split_passthrough(raw)
    assert ralphus_args == raw
    assert passthrough == []


def test_other_commands_still_honor_argparses_own_end_of_options() -> None:
    raw = ["review", "chat", "send", "@myreview", "--", "-1 point deduction"]
    ralphus_args, _ = cli._split_passthrough(raw)
    parser = cli.build_parser()
    args = parser.parse_args(ralphus_args)
    assert args.text == "-1 point deduction"


def test_merge_append_system_prompt_file_no_user_file() -> None:
    result = cli._merge_append_system_prompt_file("ralphus content", ["--mode", "auto"])
    assert result is not None
    combined, remaining = result
    assert combined == "ralphus content"
    assert remaining == ["--mode", "auto"]


def test_merge_append_system_prompt_file_wraps_user_file_with_disclaimer(
    tmp_path: Path,
) -> None:
    user_file = tmp_path / "user-prompt.md"
    user_file.write_text("be nice", encoding="utf-8")

    result = cli._merge_append_system_prompt_file(
        "ralphus content",
        ["--mode", "auto", "--append-system-prompt-file", str(user_file)],
    )
    assert result is not None
    combined, remaining = result
    assert remaining == ["--mode", "auto"]
    assert combined.index("ralphus content") < combined.index("be nice")
    assert "Important ralphus context:" in combined
    assert "Important user context:" in combined
    assert "prefer instructions in `Important ralphus context`" in combined


def test_merge_append_system_prompt_file_equals_form(tmp_path: Path) -> None:
    user_file = tmp_path / "user-prompt.md"
    user_file.write_text("be nice", encoding="utf-8")

    result = cli._merge_append_system_prompt_file(
        "ralphus content", [f"--append-system-prompt-file={user_file}", "--other"]
    )
    assert result is not None
    combined, remaining = result
    assert "be nice" in combined
    assert remaining == ["--other"]


def test_merge_append_system_prompt_file_missing_file_returns_none(
    capsys: pytest.CaptureFixture[str],
) -> None:
    result = cli._merge_append_system_prompt_file(
        "ralphus content", ["--append-system-prompt-file", "/no/such/file.md"]
    )
    assert result is None
    assert "could not read" in capsys.readouterr().err


def test_resolve_claude_launch_command_precedence(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("RALPHUS_CLAUDE_COMMAND", raising=False)
    args = argparse.Namespace(command=None)
    assert cli._resolve_claude_launch_command(args) == "claude"

    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "env-claude")
    assert cli._resolve_claude_launch_command(args) == "env-claude"

    args.command = "flag-claude"
    assert cli._resolve_claude_launch_command(args) == "flag-claude"


def test_quick_start_claude_code_bare_path_invocation(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        assert check is False
        # `--append-system-prompt-file` takes the path as its own dedicated
        # argument value (no `@`-prefixed text wrapper), so a temp path
        # containing a space can never be truncated (RAL-110 code review).
        # Read the file here, before the real cleanup logic (in a `finally`
        # after this returns) deletes it.
        idx = cmd.index("--append-system-prompt-file")
        tmp_path = Path(cmd[idx + 1])
        captured["file_content"] = tmp_path.read_text(encoding="utf-8")
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "claude-code", "--command", "my-claude"])
    assert code == 0
    cmd = captured["cmd"]
    assert cmd[0] == "my-claude"
    assert "--dangerously-skip-permissions" in cmd
    assert "a fake help-map" in captured["file_content"]
    assert "validate it first with `ralphus validate <file>`" in captured["file_content"]


def test_quick_start_claude_code_forwards_extra_args(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        ["quick-start", "claude-code", "--command", "my-claude", "--", "--mode", "auto"]
    )
    assert code == 0
    assert captured["cmd"][-2:] == ["--mode", "auto"]


def test_quick_start_claude_code_merges_user_system_prompt_file(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    captured: dict[str, Any] = {}
    user_file = tmp_path / "user-prompt.md"
    user_file.write_text("be nice", encoding="utf-8")

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        idx = cmd.index("--append-system-prompt-file")
        captured["file_content"] = Path(cmd[idx + 1]).read_text(encoding="utf-8")
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        [
            "quick-start",
            "claude-code",
            "--command",
            "my-claude",
            "--",
            "--append-system-prompt-file",
            str(user_file),
        ]
    )
    assert code == 0
    cmd = captured["cmd"]
    content = captured["file_content"]
    assert "a fake help-map" in content
    assert "be nice" in content
    assert content.index("a fake help-map") < content.index("be nice")
    assert "Important ralphus context:" in content
    assert "Important user context:" in content
    # Only ONE --append-system-prompt-file reaches `claude` -- ralphus's own
    # temp file (now holding both, disclaimer-wrapped) and the user's
    # forwarded flag are merged into that one file, ralphus's content first,
    # never both forwarded separately (RAL-110 Q4).
    assert cmd.count("--append-system-prompt-file") == 1


def test_quick_start_claude_code_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "env-claude")
    code = cli.main(["quick-start", "claude-code"])
    assert code == 0
    assert captured["cmd"][0] == "env-claude"


def test_quick_start_claude_code_command_flag_overrides_env_var(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "env-claude")
    code = cli.main(["quick-start", "claude-code", "--command", "flag-claude"])
    assert code == 0
    assert captured["cmd"][0] == "flag-claude"


def test_quick_start_claude_code_compound_command_uses_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(command: str, shell: bool, check: bool) -> _FakeCompleted:
        captured["command"] = command
        captured["shell"] = shell
        assert check is False
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(["quick-start", "claude-code", "--command", "cd foo bar ; ./claude"])
    assert code == 0
    assert captured["shell"] is True
    assert captured["command"].startswith("cd foo bar ; ./claude ")
    assert "--dangerously-skip-permissions" in captured["command"]


def test_quick_start_claude_code_cleans_up_temp_file(monkeypatch: pytest.MonkeyPatch) -> None:
    written: dict[str, Path] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        idx = cmd.index("--append-system-prompt-file")
        tmp_path = Path(cmd[idx + 1])
        written["path"] = tmp_path
        assert tmp_path.exists()
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "claude-code", "--command", "my-claude"])
    assert code == 0
    assert not written["path"].exists()


def test_quick_start_claude_code_launch_failure_returns_2(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        raise OSError("no such program")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "claude-code", "--command", "my-claude"])
    assert code == 2
    assert "could not launch claude" in capsys.readouterr().err

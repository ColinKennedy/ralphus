"""Tests for `ralphus quick-start manager|reviewer claude-code|codex` (RAL-110/166)."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

import ralphus.__main__ as cli
import ralphus.helpmap as helpmap
from ralphus import shellcmd
from ralphus.hostos import is_windows

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
        ["quick-start", "manager", "claude-code", "--", "c", "d"]
    )
    assert ralphus_args == ["quick-start", "manager", "claude-code"]
    assert passthrough == ["c", "d"]


def test_split_passthrough_without_separator() -> None:
    ralphus_args, passthrough = cli._split_passthrough(["quick-start", "manager", "claude-code"])
    assert ralphus_args == ["quick-start", "manager", "claude-code"]
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


def test_resolve_codex_launch_command_precedence(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("RALPHUS_CODEX_CMD", raising=False)
    args = argparse.Namespace(command=None)
    assert cli._resolve_codex_launch_command(args) == "codex"

    monkeypatch.setenv("RALPHUS_CODEX_CMD", "env-codex")
    assert cli._resolve_codex_launch_command(args) == "env-codex"

    args.command = "flag-codex"
    assert cli._resolve_codex_launch_command(args) == "flag-codex"


def test_parse_review_target_bare_selector_passthrough() -> None:
    assert cli._parse_review_target("guardian-000000000042") == "guardian-000000000042"
    assert cli._parse_review_target("@myreview") == "@myreview"


def test_parse_review_target_extracts_id_from_review_url() -> None:
    url = "http://127.0.0.1:7474/#/reviews/guardian-000000000042"
    assert cli._parse_review_target(url) == "guardian-000000000042"


def test_parse_review_target_extracts_id_with_trailing_query() -> None:
    url = "http://127.0.0.1:7474/#/reviews/guardian-42?tab=branches"
    assert cli._parse_review_target(url) == "guardian-42"


def test_parse_review_target_url_without_reviews_segment_falls_back_to_whole_url() -> None:
    url = "http://example.com/somewhere-else"
    assert cli._parse_review_target(url) == url


def test_manager_and_reviewer_system_prompts_differ() -> None:
    manager_content = cli._manager_system_prompt_content()
    reviewer_content = cli._reviewer_system_prompt_content(None)
    assert manager_content != reviewer_content
    assert "REVIEWER mode" in reviewer_content
    assert "REVIEWER mode" not in manager_content
    # Both still carry the full help-map for reference.
    assert "a fake help-map" in manager_content
    assert "a fake help-map" in reviewer_content


def test_reviewer_system_prompt_includes_review_operations() -> None:
    content = cli._reviewer_system_prompt_content(None)
    for fragment in (
        "review feedback",
        "review chat send",
        "review merge",
        "review restart-merge",
        "review branch enable",
        "review branch disable",
        "review base set",
        "review checks run",
        "review action run",
    ):
        assert fragment in content


def test_reviewer_system_prompt_includes_remote_and_write_boundary_notes() -> None:
    content = cli._reviewer_system_prompt_content(None)
    assert "do not assume the review's code lives on this machine" in content
    assert "must go through an explicit `ralphus review ...` subcommand" in content


def test_reviewer_system_prompt_with_target_seeds_initial_review() -> None:
    content = cli._reviewer_system_prompt_content("@myreview")
    assert "Initial review target for this session: `@myreview`" in content
    assert "ralphus review show @myreview" in content


def test_reviewer_system_prompt_with_url_target_resolves_id() -> None:
    content = cli._reviewer_system_prompt_content("http://127.0.0.1:7474/#/reviews/guardian-7")
    assert "Initial review target for this session: `guardian-7`" in content


def test_reviewer_system_prompt_without_target_has_no_target_note() -> None:
    content = cli._reviewer_system_prompt_content(None)
    assert "Initial review target" not in content


# ── quick-start manager claude-code ──────────────────────────────────────────


def test_quick_start_manager_claude_code_bare_path_invocation(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
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
    code = cli.main(["quick-start", "manager", "claude-code", "--command", "my-claude"])
    assert code == 0
    cmd = captured["cmd"]
    assert cmd[0] == "my-claude"
    assert "--dangerously-skip-permissions" in cmd
    assert "a fake help-map" in captured["file_content"]
    assert "validate it first with `ralphus validate <file>`" in captured["file_content"]


def test_quick_start_manager_claude_code_forwards_extra_args(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        [
            "quick-start",
            "manager",
            "claude-code",
            "--command",
            "my-claude",
            "--",
            "--mode",
            "auto",
        ]
    )
    assert code == 0
    assert captured["cmd"][-2:] == ["--mode", "auto"]


def test_quick_start_manager_claude_code_merges_user_system_prompt_file(
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
            "manager",
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


def test_quick_start_manager_claude_code_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "env-claude")
    code = cli.main(["quick-start", "manager", "claude-code"])
    assert code == 0
    assert captured["cmd"][0] == "env-claude"


def test_quick_start_manager_claude_code_command_flag_overrides_env_var(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    monkeypatch.setenv("RALPHUS_CLAUDE_COMMAND", "env-claude")
    code = cli.main(["quick-start", "manager", "claude-code", "--command", "flag-claude"])
    assert code == 0
    assert captured["cmd"][0] == "flag-claude"


def test_quick_start_manager_claude_code_compound_command_uses_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        assert check is False
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(
        [
            "quick-start",
            "manager",
            "claude-code",
            "--shell",
            "bash",
            "--command",
            "cd foo bar ; ./claude",
        ]
    )
    assert code == 0
    assert captured["cmd"][:2] == ["bash", "-c"]
    assert captured["cmd"][2].startswith("cd foo bar ; ./claude ")
    assert "--dangerously-skip-permissions" in captured["cmd"][2]


def test_quick_start_manager_claude_code_cleans_up_temp_file(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    written: dict[str, Path] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        idx = cmd.index("--append-system-prompt-file")
        tmp_path = Path(cmd[idx + 1])
        written["path"] = tmp_path
        assert tmp_path.exists()
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "manager", "claude-code", "--command", "my-claude"])
    assert code == 0
    assert not written["path"].exists()


def test_quick_start_manager_claude_code_launch_failure_returns_2(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        raise OSError("no such program")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "manager", "claude-code", "--command", "my-claude"])
    assert code == 2
    assert "could not launch claude" in capsys.readouterr().err


# ── quick-start manager codex ────────────────────────────────────────────────


def test_quick_start_manager_codex_bare_path_invocation(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        assert check is False
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "manager", "codex", "--command", "my-codex"])
    assert code == 0
    cmd = captured["cmd"]
    assert cmd[0] == "my-codex"
    # The `-c developer_instructions=...` override must precede any
    # subcommand -- but there IS no subcommand here (interactive TUI), so
    # it's simply the leading argument.
    assert cmd[1] == "-c"
    assert cmd[2].startswith("developer_instructions=")
    assert "a fake help-map" in cmd[2]
    assert "exec" not in cmd


def test_quick_start_manager_codex_forwards_extra_args(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        ["quick-start", "manager", "codex", "--command", "my-codex", "--", "--model", "gpt-5"]
    )
    assert code == 0
    assert captured["cmd"][-2:] == ["--model", "gpt-5"]


def test_quick_start_manager_codex_env_var(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    monkeypatch.setenv("RALPHUS_CODEX_CMD", "env-codex")
    code = cli.main(["quick-start", "manager", "codex"])
    assert code == 0
    assert captured["cmd"][0] == "env-codex"


def test_quick_start_manager_codex_compound_command_uses_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        assert check is False
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    code = cli.main(
        ["quick-start", "manager", "codex", "--shell", "bash", "--command", "cd foo bar ; ./codex"]
    )
    assert code == 0
    assert captured["cmd"][:2] == ["bash", "-c"]
    assert captured["cmd"][2].startswith("cd foo bar ; ./codex ")


def test_quick_start_manager_codex_launch_failure_returns_2(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        raise OSError("no such program")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "manager", "codex", "--command", "my-codex"])
    assert code == 2
    assert "could not launch codex" in capsys.readouterr().err


# ── quick-start reviewer claude-code ─────────────────────────────────────────


def test_quick_start_reviewer_claude_code_no_target(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        idx = cmd.index("--append-system-prompt-file")
        captured["file_content"] = Path(cmd[idx + 1]).read_text(encoding="utf-8")
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "reviewer", "claude-code", "--command", "my-claude"])
    assert code == 0
    assert captured["cmd"][0] == "my-claude"
    assert "REVIEWER mode" in captured["file_content"]
    assert "Initial review target" not in captured["file_content"]


def test_quick_start_reviewer_claude_code_with_target(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        idx = cmd.index("--append-system-prompt-file")
        captured["file_content"] = Path(cmd[idx + 1]).read_text(encoding="utf-8")
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        ["quick-start", "reviewer", "claude-code", "@myreview", "--command", "my-claude"]
    )
    assert code == 0
    assert "Initial review target for this session: `@myreview`" in captured["file_content"]


# ── quick-start reviewer codex ───────────────────────────────────────────────


def test_quick_start_reviewer_codex_no_target(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "reviewer", "codex", "--command", "my-codex"])
    assert code == 0
    cmd = captured["cmd"]
    assert cmd[0] == "my-codex"
    assert cmd[1] == "-c"
    assert "REVIEWER mode" in cmd[2]


def test_quick_start_reviewer_codex_with_url_target(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, Any] = {}

    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        captured["cmd"] = cmd
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(
        [
            "quick-start",
            "reviewer",
            "codex",
            "http://127.0.0.1:7474/#/reviews/guardian-7",
            "--command",
            "my-codex",
        ]
    )
    assert code == 0
    assert "Initial review target for this session: `guardian-7`" in captured["cmd"][2]


def test_quick_start_reviewer_codex_launch_failure_returns_2(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fake_run(cmd: list[str], check: bool) -> _FakeCompleted:
        raise OSError("no such program")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setattr(shutil, "which", lambda _prog: None)
    code = cli.main(["quick-start", "reviewer", "codex", "--command", "my-codex"])
    assert code == 2
    assert "could not launch codex" in capsys.readouterr().err


# ── RAL-189: single-name scripts + raw shell command lines ───────────────────


def _capture_run(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """Record whatever `subprocess.run` is handed, for either call shape."""
    captured: dict[str, Any] = {}

    def fake_run(args: Any, check: bool = False, shell: bool = False) -> _FakeCompleted:
        captured["args"] = args
        captured["shell"] = shell
        return _FakeCompleted(0)

    monkeypatch.setattr(subprocess, "run", fake_run)
    return captured


def _shell_command_line(captured: dict[str, Any]) -> str:
    """The one command-line string handed to the shell, whichever spawn shape was used."""
    args = captured["args"]
    return str(args) if captured["shell"] else str(args[-1])


def test_quick_start_bare_ps1_script_resolves_on_path_and_runs_via_shell(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """AC: `--command my-claude.ps1` (a single bare script name) runs correctly.

    A `.ps1` is not a launchable image on Windows and carries no execute bit
    on POSIX, so it must reach a shell -- spelled as the absolute path found
    on PATH, since PowerShell would not run a bare `my-claude.ps1` at all.
    """
    script = tmp_path / "my-claude.ps1"
    script.write_text("# launches claude\n", encoding="utf-8")
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ.get("PATH", ""))
    captured = _capture_run(monkeypatch)

    code = cli.main(
        ["quick-start", "manager", "claude-code", "--shell", "powershell", "--command", script.name]
    )
    assert code == 0
    assert captured["args"][:3] == ["powershell", "-NoLogo", "-Command"]
    line = _shell_command_line(captured)
    # PowerShell needs the `&` call operator -- a quoted path on its own is
    # just a string literal to it, not something to execute.
    assert line.startswith(f"& '{script}' ")
    assert "'--dangerously-skip-permissions'" in line


def test_quick_start_bare_script_resolves_for_a_posix_target_shell(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    script = tmp_path / "my-claude.ps1"
    script.write_text("# launches claude\n", encoding="utf-8")
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ.get("PATH", ""))
    captured = _capture_run(monkeypatch)

    code = cli.main(
        ["quick-start", "manager", "claude-code", "--shell", "bash", "--command", script.name]
    )
    assert code == 0
    assert captured["args"][:2] == ["bash", "-c"]
    # No `&` call operator outside PowerShell -- a quoted path in command
    # position is already a command for bash/sh/zsh/cmd.
    assert _shell_command_line(captured).startswith(shellcmd.quote_for_shell("bash", str(script)))


def test_quick_start_raw_command_line_with_semicolon_runs_via_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC: `--command "cd /foo/bar ; claude"` runs correctly."""
    captured = _capture_run(monkeypatch)
    code = cli.main(
        [
            "quick-start",
            "manager",
            "claude-code",
            "--shell",
            "powershell",
            "--command",
            "cd /foo/bar ; claude",
        ]
    )
    assert code == 0
    assert captured["args"][:3] == ["powershell", "-NoLogo", "-Command"]
    line = _shell_command_line(captured)
    # Verbatim, untouched -- the `;` keeps meaning what the target shell says.
    assert line.startswith("cd /foo/bar ; claude ")
    assert "'--dangerously-skip-permissions'" in line


def test_quick_start_raw_command_line_with_double_dash_runs_via_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """AC: `--command "python some_script.py -- super-claude"` runs correctly."""
    captured = _capture_run(monkeypatch)
    code = cli.main(
        [
            "quick-start",
            "manager",
            "claude-code",
            "--shell",
            "bash",
            "--command",
            "python some_script.py -- super-claude",
        ]
    )
    assert code == 0
    assert captured["args"][:2] == ["bash", "-c"]
    line = _shell_command_line(captured)
    assert line.startswith("python some_script.py -- super-claude ")
    assert "--dangerously-skip-permissions" in line


def test_quick_start_raw_command_line_reaches_codex_reviewer_too(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured = _capture_run(monkeypatch)
    code = cli.main(
        [
            "quick-start",
            "reviewer",
            "codex",
            "@myreview",
            "--shell",
            "bash",
            "--command",
            "cd /foo/bar ; codex",
        ]
    )
    assert code == 0
    assert captured["args"][:2] == ["bash", "-c"]
    line = _shell_command_line(captured)
    assert line.startswith("cd /foo/bar ; codex ")
    assert "REVIEWER mode" in line


def test_quick_start_directly_executable_program_skips_the_shell(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """A launchable image is still exec'd with real argv -- no shell, no quoting."""
    suffix = ".exe" if is_windows() else ""
    program = tmp_path / f"my-claude{suffix}"
    program.write_text("", encoding="utf-8")
    program.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ.get("PATH", ""))
    captured = _capture_run(monkeypatch)

    code = cli.main(["quick-start", "manager", "claude-code", "--command", program.name])
    assert code == 0
    assert captured["shell"] is False
    assert captured["args"][0] == str(program)
    assert "--dangerously-skip-permissions" in captured["args"]


def test_quick_start_shell_defaults_to_the_detected_parent_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("RALPHUS_SHELL", "zsh")
    captured = _capture_run(monkeypatch)
    code = cli.main(["quick-start", "manager", "claude-code", "--command", "cd /foo ; claude"])
    assert code == 0
    assert captured["args"][:2] == ["zsh", "-c"]


def test_quick_start_explicit_shell_overrides_the_detected_parent_shell(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("RALPHUS_SHELL", "zsh")
    captured = _capture_run(monkeypatch)
    code = cli.main(
        [
            "quick-start",
            "manager",
            "claude-code",
            "--shell",
            "fish",
            "--command",
            "cd /foo ; claude",
        ]
    )
    assert code == 0
    assert captured["args"][:2] == ["fish", "-c"]


def test_quick_start_rejects_an_unknown_shell() -> None:
    with pytest.raises(SystemExit):
        cli.build_parser().parse_args(
            ["quick-start", "manager", "claude-code", "--shell", "nonesuch"]
        )


# ── RAL-189: the same two shapes, actually executed ──────────────────────────
#
# Every test above stubs `subprocess.run`, so it pins down how the command line
# is *built* but can say nothing about whether the shell then accepts it -- and
# "the quoting survives a real shell" is precisely the acceptance criteria's
# claim. The three below spawn a real shell and assert on what the launched
# program actually received.

#: The shell these tests drive, and the script suffix it can run. A `.ps1` is
#: not a launchable image on Windows, and a `.sh` written without an execute
#: bit is not one on POSIX -- so both take the `script` path through
#: `_quick_start_spawn_plan` rather than being exec'd directly.
_LIVE_SHELL, _SCRIPT_SUFFIX = ("powershell", ".ps1") if is_windows() else ("sh", ".sh")

_needs_live_shell = pytest.mark.skipif(
    shutil.which(_LIVE_SHELL) is None, reason=f"{_LIVE_SHELL} is not on PATH"
)


def _spawn(raw_command: str, extra_args: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    """Plan `raw_command` exactly as the CLI does, then really run it."""
    args, use_shell, _mode = cli._quick_start_spawn_plan(
        raw_command=raw_command, extra_args=extra_args, shell=_LIVE_SHELL
    )
    return subprocess.run(
        args, shell=use_shell, cwd=cwd, check=False, capture_output=True, text=True
    )


def _echo_args_script(directory: Path, name: str) -> Path:
    """A script in `directory` printing each argument it was given on its own line."""
    script = directory / f"{name}{_SCRIPT_SUFFIX}"
    if is_windows():
        # A bare `$Rest` emits one element per line. Deliberately not
        # `ConvertTo-Json -AsArray`, which Windows PowerShell 5.1 does not
        # have -- it would fail as a *non-terminating* error, leaving stdout
        # empty but the exit code a misleading 0.
        script.write_text(
            "param([Parameter(ValueFromRemainingArguments=$true)]$Rest)\n$Rest\n",
            encoding="utf-8",
        )
    else:
        # Deliberately no chmod +x: an unexecutable script is exactly the case
        # that has to reach a shell instead of being exec'd.
        script.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n', encoding="utf-8")
    return script


def _call(program: str) -> str:
    """Spell "run this quoted program path" for `_LIVE_SHELL`.

    PowerShell parses a line starting with a quoted string as an *expression*
    (a bare string literal), not a command, so it needs the `&` call operator
    -- the same thing a user typing this command line at their own prompt
    would need. POSIX shells need nothing.
    """
    quoted = f'"{program}"'
    return f"& {quoted}" if _LIVE_SHELL == "powershell" else quoted


@_needs_live_shell
def test_bare_script_name_really_runs_and_receives_its_args(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """AC: a single bare script name (`my-claude.ps1`) resolves and runs correctly."""
    script = _echo_args_script(tmp_path, "my-claude")
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ.get("PATH", ""))

    # A plain flag, a value with a space, and one with a quote character --
    # the three things naive quoting gets wrong.
    extra_args = ["--dangerously-skip-permissions", "a b", "it's"]
    # Run from somewhere *other* than the script's directory, so this can only
    # pass via the PATH lookup -- not by accidentally picking up the cwd.
    result = _spawn(script.name, extra_args, cwd=tmp_path.parent)

    assert result.returncode == 0, result.stderr
    assert result.stdout.splitlines() == extra_args


@_needs_live_shell
def test_raw_compound_command_really_runs_in_the_directory_it_cd_ed_to(
    tmp_path: Path,
) -> None:
    """AC: a raw `cd /foo/bar ; claude` command line runs correctly.

    The `cd` has to actually take effect for the launched program, which is
    the whole reason this shape needs a shell rather than a direct exec.
    """
    target = tmp_path / "foo" / "bar"
    target.mkdir(parents=True)
    script = tmp_path / "where.py"
    script.write_text(
        "import os, sys\nprint(os.path.basename(os.getcwd()))\nprint(*sys.argv[1:])\n",
        encoding="utf-8",
    )
    raw = f'cd "{target}" ; {_call(sys.executable)} "{script}"'

    result = _spawn(raw, ["--flag"], cwd=tmp_path)

    assert result.returncode == 0, result.stderr
    cwd_line, argv_line = result.stdout.splitlines()
    assert cwd_line == "bar"
    assert argv_line == "--flag"


@_needs_live_shell
def test_raw_compound_command_with_a_double_dash_really_forwards_every_arg(
    tmp_path: Path,
) -> None:
    """AC: `python some_script.py -- super-claude` runs correctly.

    The literal `--` and the token after it belong to the user's command line
    and must survive verbatim, with ralphus's own arguments appended after
    them rather than merged in or reordered.
    """
    script = tmp_path / "some_script.py"
    script.write_text("import json, sys\nprint(json.dumps(sys.argv[1:]))\n", encoding="utf-8")
    raw = f'{_call(sys.executable)} "{script}" -- super-claude'

    result = _spawn(raw, ["--dangerously-skip-permissions", "a b"], cwd=tmp_path)

    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == [
        "--",
        "super-claude",
        "--dangerously-skip-permissions",
        "a b",
    ]


@_needs_live_shell
def test_raw_compound_command_with_a_double_dash_and_claude_really_forwards_every_arg(
    tmp_path: Path,
) -> None:
    """AC: `python foo.py -- claude` runs correctly through a real subprocess.

    The parent script itself launches a child Python subprocess and forwards
    the same argv tail into it, so this covers the nested subprocess shape
    the quick-start shell planning must preserve.
    """
    child = tmp_path / "child.py"
    child.write_text("import json, sys\nprint(json.dumps(sys.argv[1:]))\n", encoding="utf-8")

    script = tmp_path / "foo.py"
    script.write_text(
        "import subprocess, sys\n"
        f'proc = subprocess.run([sys.executable, r"{child}", *sys.argv[1:]], check=True, capture_output=True, text=True)\n'
        "print(proc.stdout, end='')\n",
        encoding="utf-8",
    )
    raw = f'{_call(sys.executable)} "{script}" -- claude'

    result = _spawn(raw, ["--dangerously-skip-permissions", "a b"], cwd=tmp_path)

    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == [
        "--",
        "claude",
        "--dangerously-skip-permissions",
        "a b",
    ]

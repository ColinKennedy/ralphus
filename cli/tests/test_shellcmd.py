"""Tests for `ralphus.shellcmd` — quick-start `--command` shell handling (RAL-189)."""

from __future__ import annotations

from pathlib import Path

import pytest

from ralphus import shellcmd
from ralphus.hostos import is_windows

# ---- shell selection --------------------------------------------------------


def test_resolve_shell_passes_through_a_named_shell(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(shellcmd, "_ancestor_shell", lambda: "cmd")
    assert shellcmd.resolve_shell("bash") == "bash"
    assert shellcmd.resolve_shell("  PowerShell  ") == "powershell"


@pytest.mark.parametrize("value", [None, "", "auto", "nonesuch"])
def test_resolve_shell_falls_back_to_detection(
    monkeypatch: pytest.MonkeyPatch, value: str | None
) -> None:
    monkeypatch.setattr(shellcmd, "detect_parent_shell", lambda: "zsh")
    assert shellcmd.resolve_shell(value) == "zsh"


def test_detect_parent_shell_prefers_the_env_override(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(shellcmd, "_ancestor_shell", lambda: "cmd")
    monkeypatch.setenv("RALPHUS_SHELL", "fish")
    assert shellcmd.detect_parent_shell() == "fish"


def test_detect_parent_shell_ignores_an_unknown_env_override(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(shellcmd, "_ancestor_shell", lambda: "cmd")
    monkeypatch.setenv("RALPHUS_SHELL", "not-a-shell")
    assert shellcmd.detect_parent_shell() == "cmd"


def test_detect_parent_shell_uses_process_ancestry_over_env_heuristics(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Ancestry is the only source that can tell "run from Windows PowerShell
    # 5.1" from "run from cmd.exe" -- %PSModulePath% is persisted machine-wide
    # and cmd.exe inherits it, so it must not be allowed to win.
    monkeypatch.delenv("RALPHUS_SHELL", raising=False)
    monkeypatch.setattr(shellcmd, "_ancestor_shell", lambda: "powershell")
    monkeypatch.setenv("PSMODULEPATH", r"C:\WINDOWS\system32\WindowsPowerShell\v1.0\Modules")
    monkeypatch.setenv("COMSPEC", r"C:\WINDOWS\system32\cmd.exe")
    monkeypatch.setenv("SHELL", "/bin/bash")
    assert shellcmd.detect_parent_shell() == "powershell"


def test_detect_parent_shell_env_fallbacks(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("RALPHUS_SHELL", raising=False)
    monkeypatch.setattr(shellcmd, "_ancestor_shell", lambda: None)
    if is_windows():
        monkeypatch.setenv("POWERSHELL_DISTRIBUTION_CHANNEL", "MSI:Windows 10 Pro")
        assert shellcmd.detect_parent_shell() == "pwsh"
        monkeypatch.delenv("POWERSHELL_DISTRIBUTION_CHANNEL")
        monkeypatch.setenv("COMSPEC", r"C:\WINDOWS\system32\cmd.exe")
        assert shellcmd.detect_parent_shell() == "cmd"
    else:
        monkeypatch.setenv("SHELL", "/usr/bin/zsh")
        assert shellcmd.detect_parent_shell() == "zsh"
        monkeypatch.delenv("SHELL")
        assert shellcmd.detect_parent_shell() == "sh"


def test_shell_kind_from_exe_handles_login_shells_and_extensions() -> None:
    assert shellcmd._shell_kind_from_exe("-bash") == "bash"
    assert shellcmd._shell_kind_from_exe("PowerShell.EXE") == "powershell"
    assert shellcmd._shell_kind_from_exe("pwsh") == "pwsh"
    assert shellcmd._shell_kind_from_exe("dash") == "sh"
    assert shellcmd._shell_kind_from_exe("vim") is None
    assert shellcmd._shell_kind_from_exe("") is None


def test_ancestor_shell_never_raises() -> None:
    # Real ancestry walk against the live process tree: whatever it reports
    # (including None), it must not blow up -- it is only ever a refinement
    # of an already-working heuristic.
    result = shellcmd._ancestor_shell()
    assert result is None or result in shellcmd.SHELL_CHOICES


# ---- program resolution -----------------------------------------------------


def test_find_program_resolves_a_bare_script_name_on_path(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    script = tmp_path / "my-claude.ps1"
    script.write_text("# hi\n", encoding="utf-8")
    monkeypatch.setenv("PATH", str(tmp_path))
    assert shellcmd.find_program("my-claude.ps1") == str(script)


def test_find_program_returns_none_for_a_miss(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("PATH", str(tmp_path))
    assert shellcmd.find_program("definitely-not-here.ps1") is None
    assert shellcmd.find_program("") is None


def test_find_program_accepts_an_explicit_path(tmp_path: Path) -> None:
    script = tmp_path / "my-claude.sh"
    script.write_text("#!/bin/sh\n", encoding="utf-8")
    assert shellcmd.find_program(str(script)) == str(script)
    assert shellcmd.find_program(str(tmp_path / "nope.sh")) is None


@pytest.mark.skipif(not is_windows(), reason="Windows-only PATHEXT/cwd lookup")
def test_find_program_tries_pathext_and_the_current_directory(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    shim = tmp_path / "my-claude.cmd"
    shim.write_text("@echo off\n", encoding="utf-8")
    monkeypatch.setenv("PATH", "")
    monkeypatch.setenv("PATHEXT", ".COM;.EXE;.BAT;.CMD")
    monkeypatch.chdir(tmp_path)
    # Windows shells search the working directory first, then PATH, trying
    # each PATHEXT suffix -- so a bare `my-claude` finds `my-claude.cmd`.
    # Compared with `samefile` because the suffix comes from %PATHEXT%, so it
    # carries that variable's casing (`.CMD`) rather than the file's own --
    # identical as far as Windows' case-insensitive filesystem is concerned.
    found = shellcmd.find_program("my-claude")
    assert found is not None
    assert Path(found).samefile(shim)


@pytest.mark.skipif(is_windows(), reason="POSIX-only: shells do not search the cwd")
def test_find_program_does_not_search_the_current_directory_on_posix(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    script = tmp_path / "my-claude"
    script.write_text("#!/bin/sh\n", encoding="utf-8")
    script.chmod(0o755)
    monkeypatch.setenv("PATH", "")
    monkeypatch.chdir(tmp_path)
    # A POSIX shell requires `./my-claude` here; silently picking up the cwd
    # would be exactly the "runs the wrong script" hazard we document against.
    assert shellcmd.find_program("my-claude") is None


def test_is_directly_executable(tmp_path: Path) -> None:
    script = tmp_path / "my-claude.ps1"
    script.write_text("# hi\n", encoding="utf-8")
    assert shellcmd.is_directly_executable(str(script)) is False

    if is_windows():
        image = tmp_path / "my-claude.exe"
        image.write_text("", encoding="utf-8")
        assert shellcmd.is_directly_executable(str(image)) is True
    else:
        image = tmp_path / "my-claude"
        image.write_text("#!/bin/sh\n", encoding="utf-8")
        image.chmod(0o755)
        assert shellcmd.is_directly_executable(str(image)) is True


# ---- quoting ----------------------------------------------------------------


@pytest.mark.parametrize(
    ("shell", "value", "expected"),
    [
        ("powershell", "plain", "'plain'"),
        ("pwsh", "it's", "'it''s'"),
        # PowerShell single-quoting is fully literal: no $var expansion.
        ("powershell", "$HOME/x y", "'$HOME/x y'"),
        ("cmd", "plain", "plain"),
        ("cmd", "C:\\Program Files\\x.md", '"C:\\Program Files\\x.md"'),
        ("bash", "it's", "'it'\"'\"'s'"),
        ("sh", "plain", "plain"),
        ("fish", "it's", "'it\\'s'"),
        ("fish", "back\\slash", "'back\\\\slash'"),
    ],
)
def test_quote_for_shell(shell: str, value: str, expected: str) -> None:
    assert shellcmd.quote_for_shell(shell, value) == expected


# ---- command-line construction ----------------------------------------------


def test_build_program_command_line_powershell_uses_the_call_operator() -> None:
    line = shellcmd.build_program_command_line(
        "powershell", r"C:\t\my-claude.ps1", ["--flag", "a b"]
    )
    assert line == r"& 'C:\t\my-claude.ps1' '--flag' 'a b'"


def test_build_program_command_line_posix_has_no_call_operator() -> None:
    line = shellcmd.build_program_command_line("bash", "/t/my-claude.sh", ["--flag", "a b"])
    assert line == "/t/my-claude.sh --flag 'a b'"


def test_build_compound_command_line_leaves_the_raw_command_verbatim() -> None:
    line = shellcmd.build_compound_command_line("bash", "cd /foo/bar ; claude", ["--flag", "a b"])
    assert line == "cd /foo/bar ; claude --flag 'a b'"


def test_build_compound_command_line_without_args() -> None:
    assert (
        shellcmd.build_compound_command_line("bash", "cd /foo ; claude", []) == "cd /foo ; claude"
    )


@pytest.mark.parametrize(
    ("shell", "expected_prefix"),
    [
        ("powershell", ["powershell", "-NoLogo", "-Command"]),
        ("pwsh", ["pwsh", "-NoLogo", "-Command"]),
        ("bash", ["bash", "-c"]),
        ("sh", ["sh", "-c"]),
        ("zsh", ["zsh", "-c"]),
        ("fish", ["fish", "-c"]),
    ],
)
def test_shell_spawn_args_uses_an_explicit_argv(shell: str, expected_prefix: list[str]) -> None:
    args, use_shell = shellcmd.shell_spawn_args(shell, "echo hi")
    assert use_shell is False
    assert args == [*expected_prefix, "echo hi"]


def test_shell_spawn_args_for_cmd() -> None:
    args, use_shell = shellcmd.shell_spawn_args("cmd", "echo hi")
    if is_windows():
        # `shell=True` on Windows is literally `%COMSPEC% /c <string>` with no
        # requoting -- subprocess's MSVCRT-style list quoting corrupts a token
        # containing a `"`, which cmd.exe does not parse that way.
        assert (args, use_shell) == ("echo hi", True)
    else:
        assert (args, use_shell) == (["cmd.exe", "/C", "echo hi"], False)


def test_shell_spawn_args_falls_back_to_sh_for_an_unknown_shell() -> None:
    args, use_shell = shellcmd.shell_spawn_args("nonesuch", "echo hi")
    assert use_shell is False
    assert args == ["sh", "-c", "echo hi"]

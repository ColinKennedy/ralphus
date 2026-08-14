"""Tests for RAL-110's self-describing help: per-subcommand descriptions and
the `--verbose` type-hint formatter."""

from __future__ import annotations

import argparse
import io
import re
from collections.abc import Iterator
from contextlib import redirect_stdout

import pytest

import ralphus.__main__ as cli
from ralphus.__main__ import build_parser


def _iter_all_parsers(parser: argparse.ArgumentParser) -> Iterator[argparse.ArgumentParser]:
    yield parser
    for action in parser._actions:
        if isinstance(action, argparse._SubParsersAction):
            yield from (p for sub in action.choices.values() for p in _iter_all_parsers(sub))


def test_every_subparser_has_a_description() -> None:
    parser = build_parser()
    missing = [p.prog for p in _iter_all_parsers(parser) if not p.description]
    assert missing == []


def _help_text(argv: list[str]) -> str:
    parser = build_parser()
    buf = io.StringIO()
    with redirect_stdout(buf), pytest.raises(SystemExit) as excinfo:
        parser.parse_args(argv)
    assert excinfo.value.code == 0
    return buf.getvalue()


def test_non_verbose_help_shows_bare_metavar() -> None:
    out = _help_text(["author", "--help"])
    assert "--budget-tokens BUDGET_TOKENS" in out
    assert "[int]" not in out


def test_verbose_help_shows_int_type_hint() -> None:
    out = _help_text(["author", "--verbose", "--help"])
    assert "--budget-tokens [int]" in out


def test_verbose_help_shows_choices_hint() -> None:
    out = _help_text(["graph", "--verbose", "--help"])
    assert "--format [ascii|dot]" in out


def test_verbose_help_shows_repeatable_hint() -> None:
    out = _help_text(["review", "checks", "run", "--verbose", "--help"])
    assert "--index [int, repeatable]" in out


def test_verbose_help_positional_keeps_name_and_shows_hint() -> None:
    out = _help_text(["validate", "--verbose", "--help"])
    assert "file [path, one or more]" in out


def test_verbose_help_excludes_help_and_verbose_from_hints() -> None:
    # -h/--verbose are universal scaffolding, not real typed arguments -- they
    # must never themselves get a bracketed hint. Match the options-section
    # entry specifically (a line starting with exactly two spaces then
    # "--verbose"), not the usage line, which also contains "--verbose".
    out = _help_text(["author", "--verbose", "--help"])
    assert "-h, --help" in out
    entry = re.search(r"^  --verbose(\s+(\S+))?", out, re.MULTILINE)
    assert entry is not None
    next_word = entry.group(2)
    assert next_word is None or not next_word.startswith("[")


def test_verbose_works_at_nested_group_level() -> None:
    out = _help_text(["run", "--verbose", "--help"])
    assert "usage: ralphus run " in out


def test_subcommand_choice_entries_are_not_hinted() -> None:
    # A group command's own subcommand list (e.g. "validate", "submit" under
    # the root) must never get a `[str]` hint appended -- only real arguments do.
    out = _help_text(["--verbose", "--help"])
    assert "validate [str]" not in out
    assert "validate " in out or "\n    validate" in out


def test_bare_show_prints_group_help() -> None:
    code = cli.main(["show"])
    assert code == 0


def test_bare_quick_start_prints_group_help() -> None:
    code = cli.main(["quick-start"])
    assert code == 0


def test_show_help_map_command_registered() -> None:
    out = _help_text(["show", "--help"])
    assert "help-map" in out


def test_show_help_map_prints_the_submit_validate_note(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # RAL-110 guidance notes (`SUBAGENT_NOTE`, `PROJECT_LOOKUP_NOTE`,
    # `SUBMIT_VALIDATE_NOTE`) are printed alongside the tree, not folded into
    # it -- assert the recommendation to validate before submitting actually
    # reaches an agent driving the CLI via `ralphus show help-map`.
    code = cli.main(["show", "help-map"])
    assert code == 0
    out = capsys.readouterr().out
    assert "validate it first with `ralphus validate <file>`" in out
    assert "- ralphus" in out  # the tree itself still follows the notes


def test_quick_start_claude_code_help_mentions_passthrough() -> None:
    out = _help_text(["quick-start", "claude-code", "--help"])
    assert "--append-system-prompt" in out
    assert "--command" in out

"""Tests for `ralphus.helpmap` (RAL-110)."""

from __future__ import annotations

import argparse
import re

import pytest

from ralphus.__main__ import build_parser
from ralphus.helpmap import _capture_help, _Node, _parse_help, _walk, generate

# The real `generate()` recursively invokes ~80 in-process `--verbose --help`
# calls (~seconds) -- fine to run once, but not something to invoke repeatedly.
pytestmark = pytest.mark.no_bench


_SAMPLE_LEAF_HELP = """\
usage: ralphus author [-h] [--verbose] [--goal [str]] [--budget-tokens [int]]

Describe work in plain language.

options:
  -h, --help            show this help message and exit
  --verbose             Show type/example-value hints for each argument's help
                        chip.
  --goal [str]          The work to accomplish (skips the interactive prompt).
  --budget-tokens [int]
                        Abort if the authoring agent exceeds this many tokens.
"""

_SAMPLE_GROUP_HELP = """\
usage: ralphus run [-h] [--verbose] SUBCOMMAND ...

Inspect and act on runs.

positional arguments:
  SUBCOMMAND
    list        List runs.
    show        Show a single run's detail.

options:
  -h, --help    show this help message and exit
  --verbose     Show type/example-value hints for each argument's help chip.
"""

_SAMPLE_POSITIONAL_HELP = """\
usage: ralphus validate [-h] [--verbose] file [path, one or more]

Validate one or more task TOML files.

positional arguments:
  file [path, one or more]
                        Path(s) to one or more .toml files.

options:
  -h, --help    show this help message and exit
  --verbose     Show type/example-value hints for each argument's help chip.
"""


def test_parse_help_leaf_extracts_options_and_description() -> None:
    positionals, options, description, subcommands = _parse_help(_SAMPLE_LEAF_HELP)
    assert positionals == []
    assert options == ["--goal [str]", "--budget-tokens [int]"]
    assert description == "Describe work in plain language."
    assert subcommands == []


def test_parse_help_excludes_help_and_verbose() -> None:
    _, options, _, _ = _parse_help(_SAMPLE_LEAF_HELP)
    assert not any("-h" in o or "--verbose" in o for o in options)


def test_parse_help_group_extracts_subcommands_not_options() -> None:
    positionals, options, description, subcommands = _parse_help(_SAMPLE_GROUP_HELP)
    assert positionals == []
    assert options == []
    assert description == "Inspect and act on runs."
    assert subcommands == ["list", "show"]


def test_parse_help_positional_keeps_full_chip() -> None:
    positionals, _, _, _ = _parse_help(_SAMPLE_POSITIONAL_HELP)
    assert positionals == ["file [path, one or more]"]


# Python 3.13+'s argparse colorizes `format_help()` with ANSI SGR codes
# whenever `sys.stdout.isatty()` is true -- i.e. a real interactive terminal,
# never a piped/subprocess-captured one. A stray reset code trailing
# "positional arguments:"/"options:" defeated `str.rstrip(":")`'s section-name
# match, silently dropping every positional/option/subcommand for that node
# (RAL-110 bug report: `ralphus show help-map` printed only the root name and
# description, no flags, no subcommands, only when run from a real terminal).
_SAMPLE_COLORIZED_GROUP_HELP = (
    "\x1b[1;34musage: \x1b[0m\x1b[1;35mralphus run\x1b[0m [\x1b[32m-h\x1b[0m]"
    " [\x1b[36m--verbose\x1b[0m] \x1b[32mSUBCOMMAND ...\x1b[0m\n"
    "\n"
    "Inspect and act on runs.\n"
    "\n"
    "\x1b[1;34mpositional arguments:\x1b[0m\n"
    "  \x1b[1;32mSUBCOMMAND\x1b[0m\n"
    "    \x1b[1;32mlist\x1b[0m        List runs.\n"
    "    \x1b[1;32mshow\x1b[0m        Show a single run's detail.\n"
    "\n"
    "\x1b[1;34moptions:\x1b[0m\n"
    "  \x1b[1;32m-h\x1b[0m, \x1b[1;36m--help\x1b[0m    show this help message and exit\n"
    "  \x1b[1;36m--verbose\x1b[0m     Show type/example-value hints for each"
    " argument's help chip.\n"
)


class _ColorizedFakeParser(argparse.ArgumentParser):
    """Stands in for a real parser whose `format_help()` came back colorized."""

    def format_help(self) -> str:
        return _SAMPLE_COLORIZED_GROUP_HELP


def test_capture_help_strips_ansi_and_recovers_colorized_sections() -> None:
    text = _capture_help(_ColorizedFakeParser())
    assert "\x1b[" not in text

    positionals, options, description, subcommands = _parse_help(text)
    assert positionals == []
    assert options == []
    assert description == "Inspect and act on runs."
    assert subcommands == ["list", "show"]


@pytest.fixture(scope="module")
def tree() -> str:
    # Expensive (~seconds: recursively walks the whole real CLI) -- computed
    # once and shared by every test in this module.
    return generate()


def _node_count(node: _Node) -> int:
    return 1 + sum(_node_count(child) for child in node.children.values())


# Every rendered line: (indent multiple of 4) "- " name/chips "  {" description "}",
# with a non-empty description -- a node whose `{...}` went missing, or whose
# description came out empty, fails to match.
_LINE_RE = re.compile(r"^(?: {4})*- \S.*  \{.+\}$")


def test_generate_produces_alphabetized_indented_tree(tree: str) -> None:
    lines = tree.splitlines()
    assert lines[0].startswith("- ralphus")

    top_level_lines = [line for line in lines if line.startswith("    - ")]
    names = [line.strip()[len("- ") :].split()[0] for line in top_level_lines]
    assert names == sorted(names)
    assert "agent" in names
    assert "review" in names
    assert "show" in names


def test_generate_every_node_has_a_braced_description(tree: str) -> None:
    for line in tree.splitlines():
        assert _LINE_RE.match(line), f"line missing a well-formed {{description}}: {line!r}"


def test_generate_has_exactly_one_line_per_command_node(tree: str) -> None:
    # Recount the CLI's real subcommand tree independently of `generate()`'s
    # rendering and compare line counts -- catches a row silently dropped (or
    # duplicated) during rendering, not just a formatting regression.
    root = _walk(build_parser())
    assert len(tree.splitlines()) == _node_count(root)


def test_generate_excludes_help_and_verbose_scaffolding(tree: str) -> None:
    assert "-h, --help" not in tree
    assert "--verbose" not in tree


def test_generate_hides_author_and_quick_start(tree: str) -> None:
    lines = tree.splitlines()
    top_level_names = {
        line.strip()[len("- ") :].split()[0] for line in lines if line.startswith("    - ")
    }
    assert "author" not in top_level_names
    assert "quick-start" not in top_level_names
    # `quick-start`'s own launch flags never leak in some other node's chips.
    assert "--executable" not in tree


def _find_line(tree: str, *, name: str, indent: int) -> str:
    prefix = "    " * indent + "- "
    for line in tree.splitlines():
        if line.startswith(prefix) and line[len(prefix) :].split()[0] == name:
            return line
    raise AssertionError(f"no {name!r} line at indent {indent} in tree")


def test_generate_tags_only_the_chosen_commands_as_subagent(tree: str) -> None:
    for name, indent in (("review", 1), ("submit", 1), ("clear", 1)):
        assert "(subagent)" in _find_line(tree, name=name, indent=indent)
    assert "(subagent)" in _find_line(tree, name="health", indent=2)

    for name, indent in (("validate", 1), ("status", 1), ("get", 1), ("check", 1)):
        assert "(subagent)" not in _find_line(tree, name=name, indent=indent)

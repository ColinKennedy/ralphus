"""Recursively map the entire `ralphus` CLI surface (RAL-110).

Walks every subcommand's own `--verbose --help` text (`_capture_help` renders
it via `ArgumentParser.format_help()` -- the exact method argparse's own `-h`/
`--help` action calls -- on the real, fully-built parser tree, in verbose
mode) and parses the rendered text, exactly as an external AI agent driving
the packaged CLI would see it. This is deliberately black-box about the
*text*: it never introspects an argument's Python-level type/nargs/choices
directly, only what `--help` actually prints, so the map reflects exactly
what a real `--verbose --help` invocation shows.

The resulting tree is rendered as one line per node: a ``- `` bullet, the
command name, its positional chips (declared order) then its optional-
argument chips (``--flag [hint]``, alphabetized), an optional ``(subagent)``
tag (see ``_SUBAGENT_PATHS``/``SUBAGENT_NOTE`` -- parenthesized, not
bracketed, so it can't be mistaken for one of the ``[type]`` argument-hint
chips), and finally a ``{one-line description}`` of that command. Children
are alphabetized and rendered the same way, each indented one level deeper
than its parent. A hand-picked set of commands (``_HIDDEN_COMMANDS``) is
excluded from the tree entirely.

Exposed as a library (`generate()`) and a standalone script (`main()`,
registered as ``ralphus-help-map``); `ralphus show help-map` (see
`ralphus.__main__`) is a thin wrapper around the same `generate()` call.
"""

from __future__ import annotations

import argparse
import re
from dataclasses import dataclass

__all__ = ["PROJECT_LOOKUP_NOTE", "SUBAGENT_NOTE", "SUBMIT_VALIDATE_NOTE", "generate", "main"]

_POSITIONAL_SECTION = "positional arguments"
_OPTIONS_SECTION = "options"

# Guidance surfaced alongside the tree wherever it's shown to an AI agent
# driving the CLI -- `ralphus show help-map`, `quick-start claude-code`'s
# injected system prompt, and `ralphus-help-map`'s own stdout. Explains what
# the inline `[subagent]` tag (see `_SUBAGENT_PATHS`/`_render` below) means;
# deliberately not folded into `generate()`'s own return value, which stays a
# pure one-line-per-node tree (see the module docstring and the tests that
# assert exact node-count parity with `_walk()`).
SUBAGENT_NOTE = (
    "Commands tagged `(subagent)` below are slow, blocking, or otherwise best run inside a "
    "subagent (e.g. Claude Code's Task tool) rather than directly in your main context -- "
    "everything else is cheap enough to invoke directly."
)

# A user will often refer to a codebase by its registered project name rather
# than a path -- "in {project_a}, do X", "add a new feature to {project_b}",
# "fix {project_c}" -- since they don't know (or don't want to type) where it
# lives on disk. Resolve the name with `ralphus project get <name>` (see
# `p_project_get` in `ralphus.__main__`) before acting, rather than guessing
# a path or asking the user to spell it out.
PROJECT_LOOKUP_NOTE = (
    'A user will often name a project instead of giving a path, e.g. "in {project_a}, do X", '
    '"add a new feature to {project_b}", "fix {project_c}" -- {project_a}/{project_b}/'
    "{project_c} each stand in for whatever project name the user actually says, not a literal "
    "value to type. When that happens, run `ralphus project get <project name>` (substituting "
    "the real name) to resolve it to its on-disk path before acting."
)

# Before `submit` on a freshly-written or freshly-edited TOML file, validate it
# directly first -- `validate` is deliberately left untagged in
# `_SUBAGENT_PATHS` above precisely because it's meant for a tight, cheap
# edit-loop, unlike the slower/blocking `submit`. `submit` itself also
# validates client-side before it actually submits and refuses invalid TOML
# (see its `--no-validate` flag's own help text), but that check only runs
# once `submit` is invoked and needs the daemon reachable -- validating up
# front catches the same per-line errors sooner, without a daemon round-trip.
SUBMIT_VALIDATE_NOTE = (
    "Before running `submit` on a TOML file you just wrote or edited, validate it first with "
    "`ralphus validate <file>` -- a fast, fully offline check (no daemon needed) that reports "
    "every error with its line number, so you can fix a draft in a tight edit loop. `submit` "
    "itself also validates before submitting by default and refuses invalid TOML, but that only "
    "happens once `submit` runs and needs the daemon reachable; validating first catches the "
    "same errors sooner and more cheaply."
)

# `author` starts its own agentic authoring loop (`ralphus.author`) that
# writes, validates, and submits a TOML on the caller's behalf, and
# `quick-start claude-code` launches an entire separate `claude` process.
# Neither is meant for an AI agent already driving `ralphus` via this
# help-map to invoke on itself -- the driving agent should write and `submit`
# TOML directly rather than spinning up a second authoring agent, and should
# never re-launch its own onboarding flow. Both are excluded from the map
# entirely rather than left for the driving agent to stumble into.
_HIDDEN_COMMANDS = frozenset({"author", "quick-start"})

# Full command paths (from the root, excluding "ralphus" itself) that get the
# inline `(subagent)` tag -- see `SUBAGENT_NOTE`. Deliberately a short,
# hand-picked list rather than "every command" or "every leaf command": each
# entry is slow/blocking (`submit` with `--wait`, `review merge`'s stacked
# rebase), destructive (`clear`), or does multi-step subprocess/filesystem
# work whose result isn't needed synchronously for orchestration decisions
# (`check health`). Cheap, frequently-polled reads (`status`, `get`, `run
# show`, ...) and tight edit-loop commands (`validate`) are deliberately left
# untagged -- routing those through a subagent would only add round-trip
# latency.
_SUBAGENT_PATHS = frozenset(
    {
        ("review",),
        ("submit",),
        ("clear",),
        ("check", "health"),
    }
)

# Python 3.13+'s argparse colorizes `format_help()` with ANSI SGR codes when
# `sys.stdout.isatty()` -- true in a real terminal, false when captured via a
# pipe/subprocess (which is why this only surfaces interactively). A trailing
# reset code after a section header (e.g. "positional arguments:\x1b[0m")
# defeats `str.rstrip(":")`'s section-name matching below, silently dropping
# every positional/option/subcommand for that node -- so strip codes first.
_ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")

# Universal scaffolding present on every single node (see
# `ralphus.__main__._RalphusArgumentParser`) -- excluding them keeps the map
# focused on each command's actual domain interface.
_UNHINTED_OPTIONS = ("-h", "--verbose")


@dataclass
class _Node:
    positionals: list[str]
    options: list[str]
    description: str
    children: dict[str, _Node]
    subagent: bool = False


def _child_parsers(parser: argparse.ArgumentParser) -> dict[str, argparse.ArgumentParser]:
    """The subparsers directly nested under `parser`, by subcommand name."""
    for action in parser._actions:
        if isinstance(action, argparse._SubParsersAction):
            return dict(action.choices)
    return {}


def _capture_help(parser: argparse.ArgumentParser) -> str:
    """The text `parser`'s own `--verbose --help` would print.

    Renders via `format_help()` directly on the already-built parser object --
    the same method argparse's `-h`/`--help` action itself calls -- rather
    than re-invoking the whole CLI (argv parsing, `SystemExit`, stdout
    capture) per node, which would rebuild the entire ~80-node parser tree
    once for every single node it visits (RAL-110 code review).
    """
    setattr(parser, "_ralphus_verbose", True)  # noqa: B010 -- ralphus.__main__'s private marker
    return _ANSI_ESCAPE.sub("", parser.format_help())


def _parse_help(text: str) -> tuple[list[str], list[str], str, list[str]]:
    """Parse one `--verbose --help` screen.

    Returns `(positional_chips, option_chips, description, subcommand_names)`.
    Relies on `--verbose` mode always suffixing a bracketed hint onto every
    real positional's chip (see `_RalphusHelpFormatter` in `ralphus.__main__`)
    to tell a real positional apart from the subparsers action's own bare
    metavar line (e.g. "COMMAND"), whose per-choice entries are nested one
    indent level deeper.
    """
    lines = text.splitlines()
    i, n = 0, len(lines)

    # The (possibly wrapped) "usage:" block, up to its first blank line.
    while i < n and lines[i].strip():
        i += 1
    while i < n and not lines[i].strip():
        i += 1

    # The description paragraph: unindented lines up to the next blank line.
    description_words: list[str] = []
    while i < n and lines[i].strip() and not lines[i].startswith(" "):
        description_words.append(lines[i].strip())
        i += 1
    description = " ".join(description_words)

    positionals: list[str] = []
    options: list[str] = []
    subcommands: list[str] = []
    section: str | None = None
    in_subparser_block = False

    while i < n:
        line = lines[i]
        stripped = line.strip()
        i += 1
        if not stripped:
            section = None
            in_subparser_block = False
            continue
        if not line.startswith(" "):
            section = stripped.rstrip(":").lower()
            in_subparser_block = False
            continue
        indent = len(line) - len(line.lstrip(" "))
        # The chip (invocation text) is separated from any inline help text
        # by a run of >= 2 spaces (argparse's own help-column padding always
        # inserts at least two -- see `HelpFormatter._format_action`).
        chip = re.split(r"\s{2,}", stripped, maxsplit=1)[0]
        if section == _POSITIONAL_SECTION and indent == 2:
            token = chip.split()[0]
            if token == chip:
                # A bare one-word line (no hint, no help) is the subparsers
                # action's own metavar (e.g. "COMMAND") -- a real positional's
                # chip always carries a bracketed hint (see docstring above),
                # so it never matches this.
                in_subparser_block = True
            else:
                in_subparser_block = False
                positionals.append(chip)
        elif section == _POSITIONAL_SECTION and indent == 4 and in_subparser_block:
            subcommands.append(chip.split()[0])
        elif section == _OPTIONS_SECTION and indent == 2:
            first_token = chip.split()[0].rstrip(",")
            if first_token not in _UNHINTED_OPTIONS:
                options.append(chip)
        # Deeper indents are wrapped help-text continuation lines -- ignore.

    return positionals, options, description, subcommands


def _walk(parser: argparse.ArgumentParser, path: tuple[str, ...] = ()) -> _Node:
    positionals, options, description, subcommand_names = _parse_help(_capture_help(parser))
    children_by_name = _child_parsers(parser)
    children = {
        name: _walk(children_by_name[name], (*path, name))
        for name in subcommand_names
        if name not in _HIDDEN_COMMANDS
    }
    return _Node(positionals, options, description, children, subagent=path in _SUBAGENT_PATHS)


def _render(name: str, node: _Node, depth: int) -> list[str]:
    """One line for `node` itself (name + chips + `{description}`), then one
    recursively-rendered line per descendant -- see module docstring."""
    indent = "    " * depth
    chips = [*node.positionals, *sorted(node.options)]
    head = f"{name} {' '.join(chips)}" if chips else name
    marker = " (subagent)" if node.subagent else ""
    lines = [f"{indent}- {head}{marker}  {{{node.description}}}"]
    for child_name in sorted(node.children):
        lines.extend(_render(child_name, node.children[child_name], depth + 1))
    return lines


def generate() -> str:
    """The full alphabetized, indented help-map tree, as one string."""
    # Deferred import: `ralphus.__main__` imports this module (for `ralphus
    # show help-map`), so importing it back at module load time would cycle.
    from ralphus.__main__ import build_parser

    root = _walk(build_parser())
    return "\n".join(_render("ralphus", root, 0))


def main() -> None:
    print(SUBAGENT_NOTE)
    print()
    print(PROJECT_LOOKUP_NOTE)
    print()
    print(SUBMIT_VALIDATE_NOTE)
    print()
    print(generate())


if __name__ == "__main__":
    main()

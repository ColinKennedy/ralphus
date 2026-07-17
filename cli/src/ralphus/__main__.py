"""Command-line entry point for ralphus."""

from __future__ import annotations

import argparse
import contextlib
import glob
import hashlib
import json
import math
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any, TypedDict, TypeVar, cast

from ralphus import __version__
from ralphus.agents import KNOWN_AGENTS, OTHER_AGENTS_NOTE
from ralphus.author import (
    AuthorError,
    AuthorOutcome,
    Budget,
    Generator,
    GeneratorAborted,
    VerifyIntent,
    author_and_submit,
    parse_verify_answer,
)
from ralphus.author.agent import load_generator
from ralphus.client import DEFAULT_DAEMON_URL, DaemonClient, DaemonError
from ralphus.config import load_config, validate_config_files
from ralphus.graphview import render_ascii, render_dot
from ralphus.health import (
    CORE,
    DEVELOPER,
    is_compound_shell_command,
    pydantic_ai_available,
    run_checks,
    unquote_path,
)
from ralphus.output import emit, exit_code_for, print_kv, print_table
from ralphus.selector import (
    ResolvedSelector,
    SelectorError,
    resolve_guardian_selector,
    resolve_run_selector,
)
from ralphus.tutor import TASK_TUTOR

__all__ = ["build_parser", "main"]

# Valid run states accepted by `ralphus clear --status` (mirrors the daemon's
# RunState). Kept here so the CLI can reject a bad filter before hitting the API.
_RUN_STATES = ("queued", "pending", "running", "done", "failed", "cancelled")

# Scaffolding-level bash completion (CLI_PARITY_PLAN.local.md Phase 7): static
# subcommand-name and state-word lists, no live daemon queries (completing a
# real run/guardian id would need a network round-trip on every <Tab>, which
# is out of scope for this pass -- see the plan's "scaffolding" framing).
_BASH_COMPLETION = """\
_ralphus_complete() {
    local cur prev
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    local top="validate submit author status resources graph get history listen run task \
session verify review clear check completion configuration queue initialize show quick-start"
    local states="queued pending running done failed cancelled ignored"

    if [[ "$prev" == "set-status" ]]; then
        COMPREPLY=( $(compgen -W "$states" -- "$cur") )
        return 0
    fi
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "$top" -- "$cur") )
        return 0
    fi
    case "${COMP_WORDS[1]}" in
        run) COMPREPLY=( $(compgen -W "list show logs set-status restart retry activate \
cancel delete rename edit" -- "$cur") ) ;;
        task) COMPREPLY=( $(compgen -W "show-tutor show set-status restart-verify \
edit" -- "$cur") ) ;;
        session) COMPREPLY=( $(compgen -W "show worktree reviews set-status restart \
restart-verify edit terminal" -- "$cur") ) ;;
        verify) COMPREPLY=( $(compgen -W "show set-status restart" -- "$cur") ) ;;
        review) COMPREPLY=( $(compgen -W "list show logs status worktrees create rename \
cancel delete settings add-branch reorder merge restart-merge force-start approve feedback \
dismiss-reenable base branch checks action chat" -- "$cur") ) ;;
        check) COMPREPLY=( $(compgen -W "health" -- "$cur") ) ;;
        queue) COMPREPLY=( $(compgen -W "list reorder set-position set-status" -- "$cur") ) ;;
        configuration) COMPREPLY=( $(compgen -W "show" -- "$cur") ) ;;
        initialize) COMPREPLY=( $(compgen -W "git" -- "$cur") ) ;;
        completion) COMPREPLY=( $(compgen -W "bash" -- "$cur") ) ;;
        show) COMPREPLY=( $(compgen -W "help-map" -- "$cur") ) ;;
        quick-start) COMPREPLY=( $(compgen -W "claude-code" -- "$cur") ) ;;
        *) ;;
    esac
}
complete -F _ralphus_complete ralphus
"""


_EXIT_CODE_EPILOG = """\
exit codes:
  0  ok
  1  domain error     (daemon reached, request rejected)
  2  usage/local error (bad args, file unreadable)
  3  not found        (HTTP 404)
  4  conflict         (HTTP 409, e.g. "already merging")
"""

# Max description length printed by `ralphus project list --short` before
# eliding the rest with "...".
_SHORT_DESCRIPTION_MAX = 80


# ---- --verbose type-hint help formatting (RAL-110) ----

# Maps an argument's `type=` callable to a short, human-readable name. An
# untyped (or `str`-typed) argument falls back to "str" below.
_TYPE_HINT_NAMES: dict[Any, str] = {
    int: "int",
    float: "float",
    Path: "path",
}

# nargs values that get a plain-English cardinality suffix in the hint.
_CARDINALITY_HINTS: dict[str, str] = {
    argparse.ONE_OR_MORE: "one or more",
    argparse.ZERO_OR_MORE: "zero or more",
    argparse.OPTIONAL: "optional",
}

# The subparsers action itself (e.g. the "COMMAND" positional) and each of
# its per-subcommand pseudo-actions (e.g. "validate", "run") aren't real
# value-taking arguments -- never give them a type/cardinality hint.
#
# `_ChoicesPseudoAction` is a doubly-private, undocumented nested class; unlike
# the rest of this module's private-argparse reliance (which only degrades
# `--verbose` output if it ever breaks), this tuple is built at *import time*,
# so a missing attribute here would crash the whole CLI. `getattr` with a
# fallback confines a future rename/removal to "COMMAND"/subcommand-name
# entries occasionally getting a spurious hint, not an import failure.
_choices_pseudo_action_type = getattr(argparse._SubParsersAction, "_ChoicesPseudoAction", None)
_UNHINTED_ACTION_TYPES: tuple[type, ...] = (
    (argparse._SubParsersAction, _choices_pseudo_action_type)
    if _choices_pseudo_action_type is not None
    else (argparse._SubParsersAction,)
)

# Same "resolve the private class once, tolerate its absence" shim as above,
# for the "repeatable" (`action='append'`) hint.
_APPEND_ACTION_TYPE: type | None = getattr(argparse, "_AppendAction", None)


def _verbose_hint_text(action: argparse.Action) -> str:
    """The `--verbose --help` chip text for `action`'s value(s): its type (or
    allowed choices), plus "repeatable"/cardinality when relevant, wrapped in
    brackets -- e.g. ``[int]``, ``[path, one or more]``, ``[int, repeatable]``,
    ``[ascii|dot]``. Positionals get their name prefixed (``file [path]``)
    since their own chip would otherwise show only the bracketed hint.
    """
    if action.choices:
        base = "|".join(str(c) for c in action.choices)
    else:
        base = _TYPE_HINT_NAMES.get(action.type, "str")
        if _APPEND_ACTION_TYPE is not None and isinstance(action, _APPEND_ACTION_TYPE):
            base += ", repeatable"
        elif isinstance(action.nargs, str) and action.nargs in _CARDINALITY_HINTS:
            base += f", {_CARDINALITY_HINTS[action.nargs]}"
    if action.option_strings:
        return f"[{base}]"
    return f"{action.dest} [{base}]"


class _RalphusHelpFormatter(argparse.RawDescriptionHelpFormatter):
    """Help formatter that, in verbose mode, replaces each argument's bare
    metavar with a `_verbose_hint_text` chip instead of the dest name (RAL-110).
    """

    def __init__(self, prog: str, verbose: bool = False, **kwargs: Any) -> None:
        super().__init__(prog, **kwargs)
        self._verbose = verbose

    def _metavar_formatter(
        self, action: argparse.Action, default_metavar: str
    ) -> Callable[[int], tuple[str, ...]]:
        # Positional-argument chips (in the "positional arguments:" list) go
        # through this method rather than `_format_args` below.
        if not self._verbose or isinstance(action, _UNHINTED_ACTION_TYPES):
            return super()._metavar_formatter(action, default_metavar)
        text = _verbose_hint_text(action)

        def format_metavar(tuple_size: int) -> tuple[str, ...]:
            return (text,) * tuple_size

        return format_metavar

    def _format_args(self, action: argparse.Action, default_metavar: str) -> str:
        # The usage line, and optional-argument chips, go through this method.
        if not self._verbose or isinstance(action, _UNHINTED_ACTION_TYPES):
            return super()._format_args(action, default_metavar)
        return _verbose_hint_text(action)


class _VerboseAction(argparse.Action):
    """`--verbose`: shows type/example-value hints in `--help` (RAL-110).

    Marks the *parser instance currently parsing* (not just the namespace),
    so a `--help` later in the same argv at the same subcommand level picks
    up verbose mode via `_RalphusArgumentParser._get_formatter` below --
    this is what makes `--verbose` work at any subcommand depth.
    """

    def __init__(self, option_strings: Sequence[str], dest: str, **kwargs: Any) -> None:
        kwargs.setdefault("nargs", 0)
        kwargs.setdefault("default", False)
        super().__init__(option_strings, dest, **kwargs)

    def __call__(
        self,
        parser: argparse.ArgumentParser,
        namespace: argparse.Namespace,
        values: str | Sequence[Any] | None,
        option_string: str | None = None,
    ) -> None:
        setattr(namespace, self.dest, True)
        cast("_RalphusArgumentParser", parser)._ralphus_verbose = True


class _RalphusArgumentParser(argparse.ArgumentParser):
    """`ArgumentParser` that auto-adds `--verbose` to every parser it creates
    (root and every subparser, since `add_subparsers()` defaults its child
    `parser_class` to `type(self)`) and always renders help through
    `_RalphusHelpFormatter` (RAL-110).
    """

    _ralphus_verbose: bool = False

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.add_argument(
            "--verbose",
            action=_VerboseAction,
            help="Show type/example-value hints for each argument's help chip.",
        )

    def _get_formatter(self) -> _RalphusHelpFormatter:
        return _RalphusHelpFormatter(prog=self.prog, verbose=self._ralphus_verbose)


_ParserT = TypeVar("_ParserT", bound=argparse.ArgumentParser)


def _add_parser(
    subparsers_action: argparse._SubParsersAction[_ParserT],
    name: str,
    **kwargs: Any,
) -> _ParserT:
    """`subparsers_action.add_parser`, defaulting `description` to `help` so
    every subcommand's own `--help` header is self-explanatory without
    maintaining the same one-line summary twice (RAL-110).
    """
    kwargs.setdefault("description", kwargs.get("help"))
    return subparsers_action.add_parser(name, **kwargs)


def build_parser() -> argparse.ArgumentParser:
    """Construct the top-level argument parser."""
    parser = _RalphusArgumentParser(
        prog="ralphus",
        description="Submit and manage autonomous agent tasks against the ralphus daemon.",
        epilog=_EXIT_CODE_EPILOG,
    )
    parser.add_argument("--version", action="version", version=f"ralphus {__version__}")
    parser.add_argument(
        "--daemon-url",
        default=os.environ.get("RALPHUS_DAEMON_URL", DEFAULT_DAEMON_URL),
        help=f"Base URL of the daemon API (default: {DEFAULT_DAEMON_URL}).",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit raw daemon JSON instead of human-readable text.",
    )
    parser.set_defaults(func=None)

    subparsers = parser.add_subparsers(dest="command", metavar="COMMAND")

    p_validate = _add_parser(subparsers, "validate", help="Validate one or more task TOML files.")
    p_validate.add_argument(
        "file",
        type=Path,
        nargs="+",
        help="Path(s) to one or more .toml files.",
    )
    p_validate.set_defaults(func=_cmd_validate)

    p_submit = _add_parser(
        subparsers, "submit", help="Submit one or more task TOML files to the daemon."
    )
    p_submit.add_argument(
        "file",
        nargs="+",
        help="Path(s) to .toml file(s), a directory, a glob pattern, or '-' for stdin. "
        "A directory or glob submits each matching file as its own separate run "
        "(explicit file paths combine into one run, as before).",
    )
    p_submit.add_argument("--label", help="Optional human label for the run.")
    p_submit.add_argument(
        "--hold",
        action="store_true",
        help="Stage the run as queued instead of scheduling it immediately.",
    )
    p_submit.add_argument(
        "--activate",
        action="store_true",
        help="Submit held, then immediately activate -- in one step.",
    )
    p_submit.add_argument(
        "--wait",
        action="store_true",
        help="Block and print status until each submitted run reaches a terminal state.",
    )
    p_submit.add_argument(
        "--dry-run",
        action="store_true",
        help="Validate and show the ingest plan (task/session/review counts) without submitting.",
    )
    p_submit.set_defaults(func=_cmd_submit)

    p_author = _add_parser(
        subparsers,
        "author",
        help="Describe work in plain language; an agent writes, validates, and submits the TOML.",
    )
    p_author.add_argument("--goal", help="The work to accomplish (skips the interactive prompt).")
    p_author.add_argument(
        "--prompt-file",
        type=Path,
        help="Read the work from a file of one or many tickets (skips the interactive prompt).",
    )
    p_author.add_argument(
        "--verify",
        help="Verify steps to attach: a comma/space list of fmt,lint,test, or 'all'/'none'.",
    )
    p_author.add_argument(
        "--verify-note",
        help="Free-form per-task verify instructions, e.g. 'lint api, test the rest'.",
    )
    p_author.add_argument(
        "--review",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="Require a per-worktree review (fails if none is created). Prompts if unset.",
    )
    p_author.add_argument(
        "--agent",
        default="claude",
        help="Agent backend for the authoring model (claude|ollama). "
        "See `ralphus agent list` for all supported agents and their models.",
    )
    p_author.add_argument("--model", help="Model for the authoring agent.")
    p_author.add_argument(
        "--max-attempts", type=int, default=5, help="Max validate/regenerate attempts."
    )
    p_author.add_argument(
        "--budget-tokens", type=int, help="Abort if the authoring agent exceeds this many tokens."
    )
    p_author.add_argument(
        "--timeout-sec", type=int, help="Abort if authoring exceeds this wall-clock time (seconds)."
    )
    p_author.add_argument("--label", help="Optional human label for submitted run(s).")
    p_author.add_argument(
        "--hold", action="store_true", help="Stage submitted run(s) as queued instead of pending."
    )
    p_author.add_argument(
        "--dry-run", action="store_true", help="Generate and validate only; do not submit."
    )
    p_author.set_defaults(func=_cmd_author)

    p_status = _add_parser(subparsers, "status", help="Show run status from the daemon.")
    p_status.add_argument("run_id", nargs="?", help="A run id; omit to list all runs.")
    p_status.add_argument(
        "--concurrency",
        action="store_true",
        help="Show scheduler concurrency instead: running/max_concurrent and active reviews.",
    )
    p_status.set_defaults(func=_cmd_status)

    p_resources = _add_parser(
        subparsers, "resources", help="Show per-task resource usage (CPU/RAM/GPU)."
    )
    p_resources.set_defaults(func=_cmd_resources)

    p_graph = _add_parser(subparsers, "graph", help="Render the task-order dependency graph.")
    p_graph.add_argument("run_id", nargs="?", help="A run id (omit when using --global).")
    p_graph.add_argument(
        "--global",
        action="store_true",
        dest="global_",
        help="Show the cross-run [[default]] gating graph instead of one run's internal DAG.",
    )
    p_graph.add_argument(
        "--all",
        action="store_true",
        help="With --global, also include terminal (done/failed/cancelled) runs.",
    )
    p_graph.add_argument(
        "--format", choices=("ascii", "dot"), default="ascii", help="Rendering format."
    )
    p_graph.set_defaults(func=_cmd_graph)

    p_get = _add_parser(
        subparsers, "get", help="Query one field from any entity's JSON view (jq-lite)."
    )
    p_get.add_argument(
        "selector",
        help="A run/task/session/verify selector, or a guardian id/@name[#branch].",
    )
    p_get.add_argument(
        "field",
        nargs="?",
        help="Dotted field path, e.g. 'state' or 'tasks.0.state'. Omit for the whole object.",
    )
    p_get.set_defaults(func=_cmd_get)

    p_history = _add_parser(
        subparsers,
        "history",
        help="Show a session/verify step's tmux history, or tail it live (RAL-140).",
    )
    p_history.add_argument(
        "selector",
        help="A session or verify selector, e.g. run-1/build/0 or run-1/build/0/verify/0.",
    )
    p_history.add_argument(
        "--live",
        action="store_true",
        help="Block and tail the live tmux session in real time instead of a one-shot snapshot.",
    )
    p_history.add_argument(
        "--wait-until-valid",
        nargs="?",
        type=float,
        const=math.inf,
        default=None,
        metavar="SECONDS",
        help="With --live: if nothing is live yet, wait (forever if SECONDS is omitted) for it "
        "to become live instead of failing immediately.",
    )
    p_history.set_defaults(func=_cmd_history)

    p_listen = _add_parser(
        subparsers,
        "listen",
        help="Block until a run/task/session/verify/review/review-worktree reaches a status "
        "(RAL-140).",
    )
    p_listen.add_argument(
        "selector",
        help="A run/task/session/verify selector, or a guardian id/@name[#branch].",
    )
    p_listen.add_argument(
        "--until",
        required=True,
        metavar="STATUS",
        help="The status to wait for (valid values depend on the selector's kind).",
    )
    p_listen.add_argument(
        "--timeout",
        type=float,
        default=None,
        metavar="SECONDS",
        help="Give up and exit 1 after SECONDS instead of waiting forever.",
    )
    p_listen.set_defaults(func=_cmd_listen)

    p_run = _add_parser(subparsers, "run", help="Inspect and act on runs.")
    run_sub = p_run.add_subparsers(dest="run_command", metavar="SUBCOMMAND")
    p_run_list = _add_parser(run_sub, "list", help="List runs.")
    p_run_list.add_argument(
        "--status", help=f"Comma-separated run states to keep ({', '.join(_RUN_STATES)})."
    )
    p_run_list.add_argument("--name", help="Only runs whose label contains this substring.")
    p_run_list.add_argument(
        "--sort", choices=("date", "name"), help="Sort order (default: newest first)."
    )
    p_run_list.set_defaults(func=_cmd_run_list)
    p_run_show = _add_parser(run_sub, "show", help="Show a single run's detail.")
    p_run_show.add_argument("run_id", help="A run id, e.g. run-000000000001.")
    p_run_show.set_defaults(func=_cmd_run_show)
    p_run_logs = _add_parser(run_sub, "logs", help="Show a run's state-transition audit log.")
    p_run_logs.add_argument("run_id", help="A run id.")
    p_run_logs.set_defaults(func=_cmd_run_logs)
    p_run_set_status = _add_parser(run_sub, "set-status", help="Manually override a run's status.")
    p_run_set_status.add_argument("run_id", help="A run id.")
    p_run_set_status.add_argument("state", help="New state, e.g. pending, ignored, done.")
    p_run_set_status.set_defaults(func=_cmd_run_set_status)
    p_run_restart = _add_parser(
        run_sub, "restart", help="Restart a whole run, dirtying every run that depends on it."
    )
    p_run_restart.add_argument("run_id", help="A run id.")
    p_run_restart.set_defaults(func=_cmd_run_restart)
    p_run_retry = _add_parser(
        run_sub, "retry", help="Re-run with the same parameters (reset to pending)."
    )
    p_run_retry.add_argument("run_id", help="A run id.")
    p_run_retry.set_defaults(func=_cmd_run_retry)
    p_run_activate = _add_parser(
        run_sub, "activate", help="Promote a held (queued) run to pending."
    )
    p_run_activate.add_argument("run_id", help="A run id.")
    p_run_activate.set_defaults(func=_cmd_run_activate)
    p_run_cancel = _add_parser(run_sub, "cancel", help="Cancel a run.")
    p_run_cancel.add_argument("run_id", help="A run id.")
    p_run_cancel.set_defaults(func=_cmd_run_cancel)
    p_run_delete = _add_parser(run_sub, "delete", help="Permanently delete a run.")
    p_run_delete.add_argument("run_id", help="A run id.")
    p_run_delete.add_argument("--yes", action="store_true", help="Skip the confirmation prompt.")
    p_run_delete.set_defaults(func=_cmd_run_delete)
    p_run_rename = _add_parser(run_sub, "rename", help="Rename a run's label.")
    p_run_rename.add_argument("run_id", help="A run id.")
    p_run_rename.add_argument("label", help="The new label.")
    p_run_rename.set_defaults(func=_cmd_run_rename)
    p_run_edit = _add_parser(run_sub, "edit", help="Edit a run's fields.")
    p_run_edit.add_argument("run_id", help="A run id.")
    p_run_edit.add_argument("--label", help="New label.")
    p_run_edit.set_defaults(func=_cmd_run_edit)
    p_run.set_defaults(func=_help_printer(p_run))

    p_clear = _add_parser(subparsers, "clear", help="Delete tasks and reviews from the daemon.")
    p_clear.add_argument(
        "--all",
        action="store_true",
        dest="all_",
        help="Clear all tasks and reviews (required unless a --status filter is given).",
    )
    p_clear.add_argument(
        "--status",
        help="Only clear runs in these comma-separated states "
        f"({', '.join(_RUN_STATES)}); reviews are left untouched.",
    )
    p_clear.add_argument(
        "--keep-temporary",
        action="store_true",
        help="Keep on-disk review worktrees instead of purging them.",
    )
    p_clear.add_argument(
        "--yes",
        action="store_true",
        help="Skip the confirmation prompt.",
    )
    p_clear.set_defaults(func=_cmd_clear)

    p_check = _add_parser(subparsers, "check", help="System and environment checks.")
    check_sub = p_check.add_subparsers(dest="check_command", metavar="SUBCOMMAND")
    p_health = _add_parser(
        check_sub,
        "health",
        help="Check the local ralphus setup (daemon, git, runner, ollama).",
    )
    p_health.add_argument(
        "--enable-developer-checks",
        action="store_true",
        help="Also check developer-only tooling (cargo, pydantic-ai).",
    )
    p_health.set_defaults(func=_cmd_check_health)
    # `ralphus check` with no subcommand prints the check help.
    p_check.set_defaults(func=_help_printer(p_check))

    p_completion = _add_parser(
        subparsers, "completion", help="Print a shell tab-completion script."
    )
    p_completion.add_argument(
        "shell", choices=("bash",), help="Shell to generate a completion script for."
    )
    p_completion.set_defaults(func=_cmd_completion)

    p_configuration = _add_parser(subparsers, "configuration", help="Configuration inspection.")
    configuration_sub = p_configuration.add_subparsers(
        dest="configuration_command", metavar="SUBCOMMAND"
    )
    p_config_show = _add_parser(
        configuration_sub,
        "show",
        help="Show sourced .ralphus.toml files and resolved values.",
    )
    p_config_show.add_argument(
        "--no-local",
        action="store_true",
        help="Exclude the local .ralphus.toml (discovered via git root) from resolution.",
    )
    p_config_show.set_defaults(func=_cmd_configuration_show)
    p_configuration.set_defaults(func=_help_printer(p_configuration))

    p_task = _add_parser(
        subparsers, "task", help="Task-authoring helpers and task-node inspection."
    )
    task_sub = p_task.add_subparsers(dest="task_command", metavar="SUBCOMMAND")
    p_tutor = _add_parser(
        task_sub,
        "show-tutor",
        help="Print the Task TOML schema reference and worked examples.",
    )
    p_tutor.set_defaults(func=_cmd_show_tutor)
    p_task_show = _add_parser(task_sub, "show", help="Show a single task node's detail.")
    p_task_show.add_argument("selector", help="A run/task selector, e.g. run-1/build.")
    p_task_show.set_defaults(func=_cmd_task_show)
    p_task_set_status = _add_parser(
        task_sub, "set-status", help="Manually override a task's status."
    )
    p_task_set_status.add_argument("selector", help="A run/task selector.")
    p_task_set_status.add_argument("state", help="New state, e.g. pending, ignored, done.")
    p_task_set_status.set_defaults(func=_cmd_task_set_status)
    p_task_restart_verify = _add_parser(
        task_sub, "restart-verify", help="Restart a task's verify steps from an index onwards."
    )
    p_task_restart_verify.add_argument("selector", help="A run/task selector.")
    p_task_restart_verify.add_argument(
        "--from", dest="from_", type=int, required=True, help="Verify index to restart from."
    )
    p_task_restart_verify.set_defaults(func=_cmd_task_restart_verify)
    p_task_edit = _add_parser(task_sub, "edit", help="Edit a task node's name/project.")
    p_task_edit.add_argument("selector", help="A run/task selector.")
    p_task_edit.add_argument("--name", help="New task name.")
    p_task_edit.add_argument("--project", help="New project label.")
    p_task_edit.set_defaults(func=_cmd_task_edit)
    # `ralphus task` with no subcommand prints the task help.
    p_task.set_defaults(func=_help_printer(p_task))

    p_session = _add_parser(subparsers, "session", help="Inspect and act on sessions.")
    session_sub = p_session.add_subparsers(dest="session_command", metavar="SUBCOMMAND")
    p_session_show = _add_parser(session_sub, "show", help="Show a single session's detail.")
    p_session_show.add_argument("selector", help="A run/task/session selector.")
    p_session_show.set_defaults(func=_cmd_session_show)
    p_session_worktree = _add_parser(
        session_sub, "worktree", help="Show the worktree/project a session is using."
    )
    p_session_worktree.add_argument("selector", help="A run/task/session selector.")
    p_session_worktree.set_defaults(func=_cmd_session_worktree)
    p_session_reviews = _add_parser(
        session_sub, "reviews", help="The reviews this session's branch participates in."
    )
    p_session_reviews.add_argument("selector", help="A run/task/session selector.")
    p_session_reviews.set_defaults(func=_cmd_session_reviews)
    p_session_set_status = _add_parser(
        session_sub, "set-status", help="Manually override a session's status."
    )
    p_session_set_status.add_argument("selector", help="A run/task/session selector.")
    p_session_set_status.add_argument("state", help="New state, e.g. pending, ignored, done.")
    p_session_set_status.set_defaults(func=_cmd_session_set_status)
    p_session_restart = _add_parser(
        session_sub,
        "restart",
        help="Restart a session (and its downstream), dirtying dependent runs.",
    )
    p_session_restart.add_argument("selector", help="A run/task/session selector.")
    p_session_restart.set_defaults(func=_cmd_session_restart)
    p_session_restart_verify = _add_parser(
        session_sub,
        "restart-verify",
        help="Restart a session's verify steps from an index onwards.",
    )
    p_session_restart_verify.add_argument("selector", help="A run/task/session selector.")
    p_session_restart_verify.add_argument(
        "--from", dest="from_", type=int, required=True, help="Verify index to restart from."
    )
    p_session_restart_verify.set_defaults(func=_cmd_session_restart_verify)
    p_session_edit = _add_parser(session_sub, "edit", help="Edit a session's fields.")
    p_session_edit.add_argument("selector", help="A run/task/session selector.")
    p_session_edit.add_argument("--cwd", help="New working directory.")
    p_session_edit.add_argument("--agent", help="New agent backend.")
    p_session_edit.add_argument("--model", help="New model.")
    p_session_edit.add_argument(
        "--prompt", help="New AI prompt (mutually meaningful only for prompt sessions)."
    )
    p_session_edit.add_argument(
        "--command", help="New shell command (takes precedence over --prompt if both given)."
    )
    p_session_edit.set_defaults(func=_cmd_session_edit)
    p_session_terminal = _add_parser(
        session_sub,
        "terminal",
        help="Print the command to resume a session's claude-code conversation locally.",
    )
    p_session_terminal.add_argument("selector", help="A run/task/session selector.")
    p_session_terminal.add_argument(
        "--mode",
        choices=("open", "readonly"),
        default="open",
        help="'open' resumes normally; 'readonly' appends a read-only system prompt.",
    )
    p_session_terminal.set_defaults(func=_cmd_session_terminal)
    p_session.set_defaults(func=_help_printer(p_session))

    p_verify = _add_parser(subparsers, "verify", help="Inspect and act on verify steps.")
    verify_sub = p_verify.add_subparsers(dest="verify_command", metavar="SUBCOMMAND")
    p_verify_show = _add_parser(verify_sub, "show", help="Show a single verify step's detail.")
    p_verify_show.add_argument(
        "selector", help="A run/task/verify or run/task/session/verify selector."
    )
    p_verify_show.set_defaults(func=_cmd_verify_show)
    p_verify_set_status = _add_parser(
        verify_sub, "set-status", help="Manually override a verify step's status."
    )
    p_verify_set_status.add_argument(
        "selector", help="A run/task/verify or .../session/verify selector."
    )
    p_verify_set_status.add_argument("state", help="New state, e.g. pending, ignored, done.")
    p_verify_set_status.set_defaults(func=_cmd_verify_set_status)
    p_verify_restart = _add_parser(
        verify_sub, "restart", help="Restart this verify step (and any later ones in its scope)."
    )
    p_verify_restart.add_argument(
        "selector", help="A run/task/verify or .../session/verify selector."
    )
    p_verify_restart.set_defaults(func=_cmd_verify_restart)
    p_verify.set_defaults(func=_help_printer(p_verify))

    p_review = _add_parser(subparsers, "review", help="Inspect and act on reviews (guardians).")
    review_sub = p_review.add_subparsers(dest="review_command", metavar="SUBCOMMAND")
    p_review_list = _add_parser(review_sub, "list", help="List reviews.")
    p_review_list.add_argument(
        "--status", help="Comma-separated review statuses to keep, e.g. collecting,in_review."
    )
    p_review_list.set_defaults(func=_cmd_review_list)
    p_review_show = _add_parser(review_sub, "show", help="Show a single review's detail.")
    p_review_show.add_argument("selector", help="A guardian id or @name.")
    p_review_show.set_defaults(func=_cmd_review_show)
    p_review_logs = _add_parser(
        review_sub, "logs", help="Show a review's state-transition audit log."
    )
    p_review_logs.add_argument("selector", help="A guardian id or @name.")
    p_review_logs.set_defaults(func=_cmd_review_logs)
    p_review_status = _add_parser(
        review_sub,
        "status",
        help="Per-branch readiness + a summary verdict ('is this review ready?').",
    )
    p_review_status.add_argument("selector", help="A guardian id or @name.")
    p_review_status.set_defaults(func=_cmd_review_status)
    p_review_worktrees = _add_parser(
        review_sub, "worktrees", help="The worktrees/branches this review consumes."
    )
    p_review_worktrees.add_argument("selector", help="A guardian id or @name.")
    p_review_worktrees.set_defaults(func=_cmd_review_worktrees)
    p_review_create = _add_parser(review_sub, "create", help="Create a new review.")
    p_review_create.add_argument("name", help="Human name for the review.")
    p_review_create.add_argument("base_branch", help="Branch the stack rebases onto.")
    p_review_create.add_argument("git_root", help="Absolute path to the git repository.")
    p_review_create.add_argument("--checks", help="Comma-separated manual-check commands to seed.")
    p_review_create.add_argument(
        "--skip-auto-build",
        action="store_true",
        help="Skip the finalize-time build/check step (explicit checks, config "
        "auto_build, and AI-inferred build) entirely.",
    )
    p_review_create.add_argument(
        "--skip-worktree-checks",
        action="store_true",
        help="Omit the quality-bar system prompt from per-branch conflict resolution.",
    )
    p_review_create.add_argument("--skip-worktrees", action="store_true")
    p_review_create.add_argument("--review-type", help="Optional review-type label.")
    p_review_create.set_defaults(func=_cmd_review_create)
    p_review_rename = _add_parser(review_sub, "rename", help="Rename a review.")
    p_review_rename.add_argument("selector", help="A guardian id or @name.")
    p_review_rename.add_argument("name", help="The new name.")
    p_review_rename.set_defaults(func=_cmd_review_rename)
    p_review_cancel = _add_parser(review_sub, "cancel", help="Cancel a review.")
    p_review_cancel.add_argument("selector", help="A guardian id or @name.")
    p_review_cancel.set_defaults(func=_cmd_review_cancel)
    p_review_delete = _add_parser(review_sub, "delete", help="Delete a review and its worktrees.")
    p_review_delete.add_argument("selector", help="A guardian id or @name.")
    p_review_delete.add_argument("--yes", action="store_true", help="Skip the confirmation prompt.")
    p_review_delete.set_defaults(func=_cmd_review_delete)
    p_review_settings = _add_parser(
        review_sub, "settings", help="Update per-review opt-out settings."
    )
    p_review_settings.add_argument("selector", help="A guardian id or @name.")
    p_review_settings.add_argument(
        "--skip-auto-build",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="Skip the finalize-time build/check step (explicit checks, config "
        "auto_build, and AI-inferred build) entirely.",
    )
    p_review_settings.add_argument(
        "--skip-worktree-checks",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="Omit the quality-bar system prompt from per-branch conflict resolution.",
    )
    p_review_settings.add_argument(
        "--skip-worktrees", action=argparse.BooleanOptionalAction, default=None
    )
    p_review_settings.add_argument("--resolver-agent", help="Resolver agent backend.")
    p_review_settings.add_argument("--resolver-model", help="Resolver model.")
    p_review_settings.add_argument("--base-branch", help="New base branch.")
    p_review_settings.add_argument(
        "--auto-pr-feedback",
        action=argparse.BooleanOptionalAction,
        default=None,
        help="Automatically incorporate PR feedback comments instead of requiring "
        "the manual 'pr pull-feedback' action (RAL-117).",
    )
    p_review_settings.set_defaults(func=_cmd_review_settings)
    p_review_add_branch = _add_parser(review_sub, "add-branch", help="Add a branch to a review.")
    p_review_add_branch.add_argument("selector", help="A guardian id or @name.")
    p_review_add_branch.add_argument("branch", help="Branch name to add.")
    p_review_add_branch.set_defaults(func=_cmd_review_add_branch)
    p_review_reorder = _add_parser(
        review_sub, "reorder", help="Set the branch order and kick off the rebase."
    )
    p_review_reorder.add_argument("selector", help="A guardian id or @name.")
    p_review_reorder.add_argument("order", help="Comma-separated branch names in the new order.")
    p_review_reorder.add_argument(
        "--disable", help="Comma-separated branch names to disable while reordering."
    )
    p_review_reorder.add_argument(
        "--enable", help="Comma-separated branch names to (re-)enable while reordering."
    )
    p_review_reorder.set_defaults(func=_cmd_review_reorder)
    p_review_merge = _add_parser(
        review_sub, "merge", help="Start (or continue) the stacked rebase."
    )
    p_review_merge.add_argument("selector", help="A guardian id or @name.")
    p_review_merge.set_defaults(func=_cmd_review_merge)
    p_review_restart_merge = _add_parser(
        review_sub, "restart-merge", help="Cancel an in-progress rebase and start a fresh one."
    )
    p_review_restart_merge.add_argument("selector", help="A guardian id or @name.")
    p_review_restart_merge.set_defaults(func=_cmd_review_restart_merge)
    p_review_force_start = _add_parser(
        review_sub,
        "force-start",
        help="Disable not-yet-done branches and merge immediately (only while collecting).",
    )
    p_review_force_start.add_argument("selector", help="A guardian id or @name.")
    p_review_force_start.set_defaults(func=_cmd_review_force_start)
    p_review_approve = _add_parser(
        review_sub, "approve", help="Approve a review that is in_review."
    )
    p_review_approve.add_argument("selector", help="A guardian id or @name.")
    p_review_approve.set_defaults(func=_cmd_review_approve)
    p_review_feedback = _add_parser(
        review_sub,
        "feedback",
        help="Post feedback on one branch, triggering a resolver re-attempt.",
    )
    p_review_feedback.add_argument("selector", help="A guardian#branch (or #position) selector.")
    p_review_feedback.add_argument("text", help="The feedback text.")
    p_review_feedback.set_defaults(func=_cmd_review_feedback)
    p_review_dismiss = _add_parser(
        review_sub, "dismiss-reenable", help="Dismiss the 're-enable' notification for a branch."
    )
    p_review_dismiss.add_argument("selector", help="A guardian#branch (or #position) selector.")
    p_review_dismiss.set_defaults(func=_cmd_review_dismiss_reenable)
    p_review_move_branch = _add_parser(
        review_sub,
        "move-branch",
        help="Move a branch to another review (RAL-118), then rebuild both.",
    )
    p_review_move_branch.add_argument(
        "selector", help="A guardian#branch (or #position) selector -- the branch to move."
    )
    p_review_move_branch.add_argument(
        "to_review", help="A guardian id or @name -- the destination review."
    )
    p_review_move_branch.set_defaults(func=_cmd_review_move_branch)

    p_review_base = _add_parser(review_sub, "base", help="Inspect/change a review's base branch.")
    review_base_sub = p_review_base.add_subparsers(dest="review_base_command", metavar="SUBCOMMAND")
    p_review_base_list = _add_parser(review_base_sub, "list", help="List candidate base branches.")
    p_review_base_list.add_argument("selector", help="A guardian id or @name.")
    p_review_base_list.set_defaults(func=_cmd_review_base_list)
    p_review_base_set = _add_parser(review_base_sub, "set", help="Change the base branch.")
    p_review_base_set.add_argument("selector", help="A guardian id or @name.")
    p_review_base_set.add_argument("branch", help="The new base branch.")
    p_review_base_set.set_defaults(func=_cmd_review_base_set)
    p_review_base.set_defaults(func=_help_printer(p_review_base))

    p_review_pr = _add_parser(
        review_sub, "pr", help="Submit/query pull requests for a review (RAL-117)."
    )
    review_pr_sub = p_review_pr.add_subparsers(dest="review_pr_command", metavar="SUBCOMMAND")
    p_review_pr_submit = _add_parser(
        review_pr_sub,
        "submit",
        help="Submit a PR/MR for one stacked branch or the combined worktree.",
    )
    p_review_pr_submit.add_argument("selector", help="A guardian id or @name.")
    pr_submit_target = p_review_pr_submit.add_mutually_exclusive_group(required=True)
    pr_submit_target.add_argument(
        "--position", type=int, help="Stacked branch position to submit (0-based)."
    )
    pr_submit_target.add_argument(
        "--combined",
        action="store_true",
        help="Submit the combined (all-branches-in-one) worktree instead of one stacked branch.",
    )
    p_review_pr_submit.add_argument(
        "--alias",
        help="Branch name to push the PR under (default: the feature branch's own name, or "
        "a sanitized review name for --combined -- never the internal guardian/... name).",
    )
    p_review_pr_submit.add_argument("--title", help="PR title (default: synthesized from commits).")
    p_review_pr_submit.add_argument(
        "--description", help="PR description (default: synthesized from commits)."
    )
    p_review_pr_submit.set_defaults(func=_cmd_review_pr_submit)
    p_review_pr_list = _add_parser(review_pr_sub, "list", help="List PRs submitted for a review.")
    p_review_pr_list.add_argument("selector", help="A guardian id or @name.")
    p_review_pr_list.set_defaults(func=_cmd_review_pr_list)
    p_review_pr_show = _add_parser(review_pr_sub, "show", help="Show one PR row.")
    p_review_pr_show.add_argument("pr_id", help="The ralphus PR row id (e.g. pr-000000000001).")
    p_review_pr_show.set_defaults(func=_cmd_review_pr_show)
    p_review_pr_find = _add_parser(
        review_pr_sub, "find", help="Look up the ralphus PR row for a forge PR/MR number."
    )
    p_review_pr_find.add_argument("forge", choices=["github", "gitlab"])
    p_review_pr_find.add_argument("repo", help="owner/repo (GitHub) or namespace path (GitLab).")
    p_review_pr_find.add_argument("pr_number", type=int)
    p_review_pr_find.set_defaults(func=_cmd_review_pr_find)
    p_review_pr_update = _add_parser(
        review_pr_sub,
        "update",
        help="Mutate the recorded PR mapping, e.g. after a PR is closed and reopened under a "
        "new number.",
    )
    p_review_pr_update.add_argument("pr_id", help="The ralphus PR row id.")
    p_review_pr_update.add_argument("--pr-number", type=int)
    p_review_pr_update.add_argument("--pr-url")
    p_review_pr_update.add_argument("--branch-alias")
    p_review_pr_update.add_argument("--state", choices=["open", "merged", "closed"])
    p_review_pr_update.set_defaults(func=_cmd_review_pr_update)
    p_review_pr_comments = _add_parser(
        review_pr_sub, "comments", help="List a PR's comments/notes."
    )
    p_review_pr_comments.add_argument("pr_id", help="The ralphus PR row id.")
    p_review_pr_comments.set_defaults(func=_cmd_review_pr_comments)
    p_review_pr_pull_feedback = _add_parser(
        review_pr_sub,
        "pull-feedback",
        help="Action a PR's un-actioned feedback into the owning review worktree.",
    )
    p_review_pr_pull_feedback.add_argument("pr_id", help="The ralphus PR row id.")
    p_review_pr_pull_feedback.set_defaults(func=_cmd_review_pr_pull_feedback)
    p_review_pr.set_defaults(func=_help_printer(p_review_pr))

    p_review_branch = _add_parser(review_sub, "branch", help="Enable/disable one review branch.")
    review_branch_sub = p_review_branch.add_subparsers(
        dest="review_branch_command", metavar="SUBCOMMAND"
    )
    p_review_branch_enable = _add_parser(
        review_branch_sub, "enable", help="Enable a branch and kick off the rebase."
    )
    p_review_branch_enable.add_argument(
        "selector", help="A guardian#branch (or #position) selector."
    )
    p_review_branch_enable.set_defaults(func=_cmd_review_branch_enable)
    p_review_branch_disable = _add_parser(
        review_branch_sub, "disable", help="Disable a branch and kick off the rebase."
    )
    p_review_branch_disable.add_argument(
        "selector", help="A guardian#branch (or #position) selector."
    )
    p_review_branch_disable.set_defaults(func=_cmd_review_branch_disable)
    p_review_branch.set_defaults(func=_help_printer(p_review_branch))

    p_review_checks = _add_parser(
        review_sub, "checks", help="LLM-synthesized manual review-verification commands."
    )
    review_checks_sub = p_review_checks.add_subparsers(
        dest="review_checks_command", metavar="SUBCOMMAND"
    )
    p_review_checks_list = _add_parser(review_checks_sub, "list", help="List the manual checks.")
    p_review_checks_list.add_argument("selector", help="A guardian id or @name.")
    p_review_checks_list.set_defaults(func=_cmd_review_checks_list)
    p_review_checks_run = _add_parser(
        review_checks_sub,
        "run",
        help="Print the command(s) + cwd to run one/some/all manual checks yourself.",
    )
    p_review_checks_run.add_argument("selector", help="A guardian id or @name.")
    p_review_checks_run.add_argument(
        "--index", type=int, action="append", help="A check index to run; repeat for several."
    )
    p_review_checks_run.add_argument("--all", action="store_true", help="Print every check.")
    p_review_checks_run.set_defaults(func=_cmd_review_checks_run)
    p_review_checks.set_defaults(func=_help_printer(p_review_checks))

    p_review_action = _add_parser(
        review_sub, "action", help="User-declared [[review.action]] test/action hints."
    )
    review_action_sub = p_review_action.add_subparsers(
        dest="review_action_command", metavar="SUBCOMMAND"
    )
    p_review_action_list = _add_parser(review_action_sub, "list", help="List the action hints.")
    p_review_action_list.add_argument("selector", help="A guardian id or @name.")
    p_review_action_list.set_defaults(func=_cmd_review_action_list)
    p_review_action_run = _add_parser(
        review_action_sub, "run", help="Print the command + cwd for a command-kind action hint."
    )
    p_review_action_run.add_argument("selector", help="A guardian id or @name.")
    p_review_action_run.add_argument("--index", type=int, required=True, help="Action hint index.")
    p_review_action_run.set_defaults(func=_cmd_review_action_run)
    p_review_action.set_defaults(func=_help_printer(p_review_action))

    p_review_chat = _add_parser(review_sub, "chat", help="The review's global feedback thread.")
    review_chat_sub = p_review_chat.add_subparsers(dest="review_chat_command", metavar="SUBCOMMAND")
    p_review_chat_send = _add_parser(review_chat_sub, "send", help="Post a message.")
    p_review_chat_send.add_argument("selector", help="A guardian id or @name.")
    p_review_chat_send.add_argument("text", help="The message text.")
    p_review_chat_send.set_defaults(func=_cmd_review_chat_send)
    p_review_chat_show = _add_parser(review_chat_sub, "show", help="Show the thread.")
    p_review_chat_show.add_argument("selector", help="A guardian id or @name.")
    p_review_chat_show.set_defaults(func=_cmd_review_chat_show)
    p_review_chat_fork = _add_parser(
        review_chat_sub, "fork", help="Fork the thread at a message, replacing it with new text."
    )
    p_review_chat_fork.add_argument("selector", help="A guardian id or @name.")
    p_review_chat_fork.add_argument(
        "--seq", type=int, required=True, help="Message seq to fork at."
    )
    p_review_chat_fork.add_argument("text", help="The replacement message text.")
    p_review_chat_fork.set_defaults(func=_cmd_review_chat_fork)
    p_review_chat.set_defaults(func=_help_printer(p_review_chat))

    p_review.set_defaults(func=_help_printer(p_review))

    p_queue = _add_parser(
        subparsers, "queue", help="Inspect and reorder the run queue by priority."
    )
    queue_sub = p_queue.add_subparsers(dest="queue_command", metavar="SUBCOMMAND")
    p_q_list = _add_parser(
        queue_sub, "list", help="List queued work items (ready-to-run by default)."
    )
    p_q_list.add_argument(
        "--all",
        action="store_true",
        dest="all_",
        help="Also show blocked/excluded items, not just ready-to-run ones.",
    )
    p_q_list.set_defaults(func=_cmd_queue_list)
    p_q_reorder = _add_parser(
        queue_sub,
        "reorder",
        help="Set the queue order to the given item paths (dependency-repaired).",
    )
    p_q_reorder.add_argument(
        "paths", nargs="+", help="Item paths in the desired order, e.g. run-000000000001/t0/s1"
    )
    p_q_reorder.set_defaults(func=_cmd_queue_reorder)
    p_q_pos = _add_parser(
        queue_sub, "set-position", help="Move item(s) to an absolute index or a relative offset."
    )
    p_q_pos.add_argument("paths", nargs="+", help="Item path(s) to move.")
    p_q_pos.add_argument(
        "--to",
        type=int,
        required=True,
        help="Target position: a 0-based absolute index, or a move-up count with --relative.",
    )
    p_q_pos.add_argument(
        "--relative",
        action="store_true",
        help="Treat --to as a relative move-up count (negative moves down).",
    )
    p_q_pos.set_defaults(func=_cmd_queue_set_position)
    p_q_status = _add_parser(
        queue_sub,
        "set-status",
        help="Set a run/task/session/verify status (e.g. ignored) by item path or run id.",
    )
    p_q_status.add_argument("path", help="Item path (run/t0/s1[/v0], run/t0/tv0) or a bare run id.")
    p_q_status.add_argument("state", help="New state, e.g. ignored, pending, done.")
    p_q_status.set_defaults(func=_cmd_queue_set_status)
    # Bare `ralphus queue` lists ready-to-run work.
    p_queue.set_defaults(func=_cmd_queue_list)

    p_initialize = _add_parser(
        subparsers, "initialize", help="One-time local setup helpers for a repository."
    )
    initialize_sub = p_initialize.add_subparsers(dest="initialize_command", metavar="SUBCOMMAND")
    p_init_git = _add_parser(
        initialize_sub,
        "git",
        help="Enable git rerere in a repo so review rebases replay conflict resolutions.",
    )
    p_init_git.add_argument(
        "--path",
        type=Path,
        default=None,
        help="Repository directory to configure (default: the current directory).",
    )
    p_init_git.set_defaults(func=_cmd_initialize_git)
    # `ralphus initialize` with no subcommand prints the group help.
    p_initialize.set_defaults(func=_help_printer(p_initialize))

    p_project = _add_parser(
        subparsers, "project", help="Register and inspect projects known to the daemon (RAL-100)."
    )
    project_sub = p_project.add_subparsers(dest="project_command", metavar="SUBCOMMAND")
    p_project_git = _add_parser(
        project_sub,
        "git",
        help="Register a git repository as a project the daemon can resolve "
        "placeholder session cwds against.",
    )
    p_project_git.add_argument(
        "--path", type=Path, required=True, help="Path to the project's git repository."
    )
    p_project_git.add_argument(
        "--name", required=True, help="Unique project name, referenced from a task's 'project'."
    )
    p_project_git.add_argument(
        "--description",
        default="",
        help="Human description, also searched by fuzzy project-name lookup.",
    )
    p_project_git.set_defaults(func=_cmd_project_git)
    p_project_list = _add_parser(
        project_sub, "list", help="List every project registered with the daemon."
    )
    p_project_list.add_argument(
        "--short",
        action="store_true",
        help="Elide-right long descriptions to fit on one line, instead of "
        "wrapping each onto an indented line below its project.",
    )
    p_project_list.set_defaults(func=_cmd_project_list)
    p_project_get = _add_parser(
        project_sub, "get", help="Show one registered project's details by exact name."
    )
    p_project_get.add_argument("name", help="Registered project name.")
    p_project_get.set_defaults(func=_cmd_project_get)
    # `ralphus project` with no subcommand prints the group help.
    p_project.set_defaults(func=_help_printer(p_project))

    p_agent = _add_parser(subparsers, "agent", help="Inspect agent backends ralphus can run.")
    agent_sub = p_agent.add_subparsers(dest="agent_command", metavar="SUBCOMMAND")
    p_agent_list = _add_parser(
        agent_sub,
        "list",
        help="List supported agent backends and the models each is allowed to run.",
    )
    p_agent_list.set_defaults(func=_cmd_agent_list)
    # Bare `ralphus agent` lists agents (mirrors bare `ralphus queue`).
    p_agent.set_defaults(func=_cmd_agent_list)

    p_show = _add_parser(subparsers, "show", help="Print machine-readable views of ralphus itself.")
    show_sub = p_show.add_subparsers(dest="show_command", metavar="SUBCOMMAND")
    p_show_help_map = _add_parser(
        show_sub,
        "help-map",
        help="Print the full CLI command surface as an alphabetized, "
        "indented tree (for onboarding an AI agent) (RAL-110).",
    )
    p_show_help_map.set_defaults(func=_cmd_show_help_map)
    # `ralphus show` with no subcommand prints the group help.
    p_show.set_defaults(func=_help_printer(p_show))

    p_quick_start = _add_parser(
        subparsers,
        "quick-start",
        help="One-command onboarding paths for driving ralphus with an external tool.",
    )
    quick_start_sub = p_quick_start.add_subparsers(dest="quick_start_command", metavar="SUBCOMMAND")
    p_qs_claude_code = _add_parser(
        quick_start_sub,
        "claude-code",
        help="Launch Claude Code primed with the full ralphus CLI help-map, "
        "so it can orchestrate ralphus unsupervised (RAL-110).",
    )
    p_qs_claude_code.description = (
        "Writes the help-map (same tree `show help-map` prints) to a throwaway temp file and "
        "launches `claude --dangerously-skip-permissions --append-system-prompt-file <tempfile> "
        "...`. Args after a literal `--` are forwarded verbatim to `claude`, e.g. "
        "`ralphus quick-start claude-code -- --mode auto`. If those forwarded args include their "
        "own --append-system-prompt-file, its contents are read and folded into ralphus's own "
        "temp file instead -- ralphus's context first, then a disclaimer, then the user's -- "
        "rather than forwarding a second, separate flag."
    )
    p_qs_claude_code.add_argument(
        "--command",
        help="Override the `claude` launch command for this invocation only "
        "(takes precedence over $RALPHUS_CLAUDE_COMMAND).",
    )
    p_qs_claude_code.set_defaults(func=_cmd_quick_start_claude_code)
    # `ralphus quick-start` with no subcommand prints the group help.
    p_quick_start.set_defaults(func=_help_printer(p_quick_start))

    return parser


def _help_printer(parser: argparse.ArgumentParser) -> Callable[[argparse.Namespace], int]:
    """A command that just prints `parser`'s help (used for bare group commands)."""

    def _run(_args: argparse.Namespace) -> int:
        parser.print_help()
        return 0

    return _run


_DAEMON_HINT = "(is the daemon running? start it with: ralphus-daemon serve)"


def _print_daemon_error(exc: DaemonError, *, json_mode: bool = False) -> None:
    if json_mode:
        print(json.dumps({"error": str(exc), "status_code": exc.status_code}), file=sys.stderr)
    elif exc.status_code is None:
        # No status code means the request never got an HTTP response at all
        # (connection refused/timeout) -- the daemon is genuinely a plausible
        # cause. A response with a status code means the daemon IS running and
        # rejected the request for its own reason, so the hint would be noise.
        print(f"error: {exc}\n{_DAEMON_HINT}", file=sys.stderr)
    else:
        print(f"error: {exc}", file=sys.stderr)


def _print_selector_error(exc: SelectorError, *, json_mode: bool = False) -> None:
    if json_mode:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
    else:
        print(f"error: {exc}", file=sys.stderr)


def _elide_right(text: str, max_len: int) -> str:
    """Truncate `text` to `max_len` chars, replacing the tail with "..." if cut."""
    if len(text) <= max_len:
        return text
    return text[: max_len - 3] + "..."


def _read_file(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        print(f"error: could not read {path}: {exc}", file=sys.stderr)
        return None


def _find_daemon_bin() -> str | None:
    """Locate the ralphus-daemon binary for offline validation.

    Checks RALPHUS_DAEMON_BIN, then a binary sitting next to this executable
    (the packaged case: ralphus.exe alongside ralphus-daemon.exe — for a frozen
    build that is ``sys.executable``), then PATH.
    """
    override = os.environ.get("RALPHUS_DAEMON_BIN")
    if override and (os.path.exists(override) or shutil.which(override)):
        return override
    search_dirs: list[Path] = []
    if getattr(sys, "frozen", False):
        search_dirs.append(Path(sys.executable).resolve().parent)
    search_dirs.append(Path(sys.argv[0]).resolve().parent)
    for directory in search_dirs:
        for name in ("ralphus-daemon.exe", "ralphus-daemon"):
            candidate = directory / name
            if candidate.exists():
                return str(candidate)
    return shutil.which("ralphus-daemon")


def _validate_offline_combined(daemon_bin: str, text: str) -> int:
    """Run the daemon binary's offline validator against combined multi-file text.

    The binary only accepts a single on-disk path, so the already-combined text
    (see ``_cmd_validate``) is written to a scratch file first. Line numbers in
    any reported errors are therefore relative to the combined document, same
    as what `ralphus submit` would produce as one TaskFile.
    """
    fd, tmp_path = tempfile.mkstemp(suffix=".toml", prefix="ralphus-validate-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
        return subprocess.run([daemon_bin, "validate", tmp_path], check=False).returncode
    finally:
        with contextlib.suppress(OSError):
            os.remove(tmp_path)


def _cmd_validate(args: argparse.Namespace) -> int:
    paths: list[Path] = args.file
    texts: list[str] = []
    for path in paths:
        text = _read_file(path)
        if text is None:
            return 2
        texts.append(text)
    # Multiple files combine into ONE document, mirroring `ralphus submit`: a
    # set of files meant for one run becomes a single TaskFile once submitted,
    # so validating them individually could miss cross-file errors (e.g.
    # duplicate task names) that only surface once combined.
    combined = "\n\n".join(texts)

    # Validation is pure and needs no server: prefer the offline core validator
    # in the ralphus-daemon binary, and only fall back to the daemon API if the
    # binary cannot be located.
    daemon_bin = _find_daemon_bin()
    if daemon_bin is not None:
        try:
            if len(paths) == 1:
                # Single-file case: validate the real path directly so any
                # reported line numbers point at the actual file on disk.
                return subprocess.run(
                    [daemon_bin, "validate", str(paths[0])], check=False
                ).returncode
            return _validate_offline_combined(daemon_bin, combined)
        except OSError:
            pass  # fall back to the API path below

    with DaemonClient(args.daemon_url) as client:
        try:
            outcome = client.validate(combined)
        except DaemonError as exc:
            print(
                f"error: {exc}\n(hint: start the daemon, or put ralphus-daemon on PATH / set "
                "RALPHUS_DAEMON_BIN so validation can run offline)",
                file=sys.stderr,
            )
            return 2
    for warning in outcome.warnings:
        print(f"warning [line {warning.get('line', '?')}]: {warning.get('message', '')}")
    if outcome.valid:
        print("valid")
        return 0
    for err in outcome.errors:
        print(f"error [line {err.get('line', '?')}]: {err.get('message', '')}", file=sys.stderr)
    return 1


_STDIN_SOURCE = "-"


def _resolve_submit_sources(raw_args: list[str]) -> tuple[list[str], bool] | None:
    """Expand each `submit` file argument into concrete sources.

    A source is either `_STDIN_SOURCE` or a path string. Returns
    `(sources, is_batch)`: `is_batch` is True when any argument was a
    directory or glob pattern -- signalling that each resolved file should
    become its own separate run, rather than combining into one (the existing
    behavior for explicit file paths, preserved for backward compatibility).
    Prints an error and returns `None` on a bad argument (empty glob match,
    non-existent directory).
    """
    sources: list[str] = []
    is_batch = False
    for raw in raw_args:
        if raw == _STDIN_SOURCE:
            sources.append(_STDIN_SOURCE)
            continue
        if any(ch in raw for ch in "*?["):
            matches = sorted(glob.glob(raw))
            if not matches:
                print(f"error: no files matched glob '{raw}'", file=sys.stderr)
                return None
            sources.extend(matches)
            is_batch = True
            continue
        path = Path(raw)
        if path.is_dir():
            matches = sorted(str(p) for p in path.glob("*.toml"))
            if not matches:
                print(f"error: no .toml files found in directory '{raw}'", file=sys.stderr)
                return None
            sources.extend(matches)
            is_batch = True
            continue
        sources.append(raw)
    return sources, is_batch


def _read_submit_source(source: str) -> str | None:
    if source == _STDIN_SOURCE:
        text = sys.stdin.read()
        if not text.strip():
            print("error: stdin is empty", file=sys.stderr)
            return None
        return text
    return _read_file(Path(source))


def _count_toml_entities(text: str) -> tuple[int, int, int]:
    """`(task_count, session_count, review_count)`, parsed client-side with
    `tomllib` for `submit --dry-run`'s ingest-plan preview. This is a syntactic
    count only -- it does not run the daemon's git/worktree-dependent review
    derivation, so it cannot preview the run id it would get or which reviews
    would actually be created (that logic lives server-side and touches the
    filesystem); see CLI_PARITY_PLAN.local.md Q6.
    """
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return (0, 0, 0)
    tasks = data.get("task", [])
    task_count = len(tasks)
    session_count = sum(len(t.get("session", [])) for t in tasks if isinstance(t, dict))
    review_count = len(data.get("review", []))
    return (task_count, session_count, review_count)


def _wait_for_terminal(
    client: DaemonClient,
    run_id: str,
    *,
    poll_interval: float = 2.0,
    sleep_fn: Callable[[float], None] = time.sleep,
) -> dict[str, Any]:
    """Poll `run_id` until it reaches a terminal state, printing each state
    change. Returns the final run view.
    """
    last_state: object = None
    while True:
        run = client.run(run_id)
        state = run.get("state")
        if state != last_state:
            print(f"{run_id}: {state}")
            last_state = state
        if state in ("done", "failed", "cancelled"):
            return run
        sleep_fn(poll_interval)


def _finish_submission(
    client: DaemonClient, result: dict[str, Any], args: argparse.Namespace
) -> int:
    """Apply --activate/--wait to one just-submitted run and return its
    contribution to the process exit code.
    """
    run_id = result.get("run_id")
    if args.activate and run_id:
        try:
            result = client.activate_run(run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    if args.wait and run_id:
        final = _wait_for_terminal(client, run_id)
        return 0 if final.get("state") == "done" else 1
    return 0


def _cmd_submit(args: argparse.Namespace) -> int:
    resolved = _resolve_submit_sources(args.file)
    if resolved is None:
        return 2
    sources, is_batch = resolved
    hold = args.hold or args.activate

    if args.dry_run:
        texts = []
        for source in sources:
            text = _read_submit_source(source)
            if text is None:
                return 2
            texts.append(text)
        combined = "\n\n".join(texts)
        with DaemonClient(args.daemon_url) as client:
            try:
                outcome = client.validate(combined)
            except DaemonError as exc:
                _print_daemon_error(exc, json_mode=args.json)
                return exit_code_for(exc)
        if not outcome.valid:
            for err in outcome.errors:
                print(
                    f"error [line {err.get('line', '?')}]: {err.get('message', '')}",
                    file=sys.stderr,
                )
            return 1
        tasks, sessions, reviews = _count_toml_entities(combined)
        print(
            f"valid; would create {tasks} task(s), {sessions} session(s), "
            f"{reviews} review(s) (dry run; nothing submitted)"
        )
        return 0

    if is_batch:
        exit_code = 0
        with DaemonClient(args.daemon_url) as client:
            for source in sources:
                text = _read_submit_source(source)
                if text is None:
                    return 2
                try:
                    result = client.submit(text, hold=hold, label=args.label)
                except DaemonError as exc:
                    _print_daemon_error(exc, json_mode=args.json)
                    return exit_code_for(exc)  # one failure aborts the batch
                emit(args.json, result, lambda r: print(f"{r.get('run_id')} ({r.get('state')})"))
                exit_code = max(exit_code, _finish_submission(client, result, args))
        return exit_code

    # Explicit file paths (no dir/glob involved): combine into ONE submission
    # (one TaskFile, one POST) so a `ralphus:new-review/<key>` shared across
    # files folds into a single review instead of minting one guardian per file.
    texts = []
    for source in sources:
        text = _read_submit_source(source)
        if text is None:
            return 2
        texts.append(text)
    text = "\n\n".join(texts)
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.submit(text, hold=hold, label=args.label)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
        emit(args.json, result, lambda r: print(f"{r.get('run_id')} ({r.get('state')})"))
        return _finish_submission(client, result, args)


def _load_generator(agent: str, model: str | None) -> Generator | None:
    if not pydantic_ai_available():
        return None
    return load_generator(agent, model)


def _prompt(message: str) -> str:
    try:
        return input(message)
    except EOFError:
        return ""


def _read_prompt_file(path: Path) -> str | None:
    """Read tickets from a prompt file, or print a clear error and return None.

    Shared by ``--prompt-file`` and the interactive path-detection branch of
    :func:`_resolve_goal`. The whole file becomes the authoring goal in a single
    pass — splitting one-or-many tickets is left to the LLM, so no delimiter is
    parsed here.
    """
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        print(f"error: prompt file not found: {path}", file=sys.stderr)
        return None
    except OSError as exc:
        print(f"error: could not read prompt file {path}: {exc}", file=sys.stderr)
        return None
    if not text.strip():
        print(f"error: prompt file is empty: {path}", file=sys.stderr)
        return None
    return text


def _resolve_goal(args: argparse.Namespace) -> str | None:
    if args.prompt_file is not None:
        return _read_prompt_file(args.prompt_file)
    if args.goal:
        return str(args.goal)
    if not sys.stdin.isatty():
        print(
            "error: no --goal or --prompt-file given and stdin is not interactive",
            file=sys.stderr,
        )
        return None
    entry = _prompt("Describe the work to be done (or a path to a ticket file): ").strip()
    if not entry:
        return None
    # A path typed at the prompt is read as a file — same code path as
    # --prompt-file. Surrounding quotes (as Windows "Copy as path" adds) are
    # stripped first. Anything that isn't an existing file is taken as inline text.
    candidate = Path(entry.strip('"'))
    if candidate.is_file():
        return _read_prompt_file(candidate)
    return entry


def _resolve_intent(args: argparse.Namespace) -> VerifyIntent:
    if args.verify is not None or args.verify_note:
        return parse_verify_answer(args.verify or "", args.verify_note or "")
    if not sys.stdin.isatty():
        return VerifyIntent()
    answer = _prompt("Attach verify steps for formatting/linting/tests? [y/N/explain] ").strip()
    note = ""
    if answer.lower() in ("explain", "e"):
        note = _prompt("Describe which checks apply to which tasks: ").strip()
        answer = ""
    return parse_verify_answer(answer, note)


def _resolve_wants_review(args: argparse.Namespace) -> bool:
    if args.review is not None:
        return bool(args.review)
    if not sys.stdin.isatty():
        return False
    return _prompt("Is this a worktree feature that must be reviewed? [y/N] ").strip().lower() in (
        "y",
        "yes",
    )


def _cmd_author(args: argparse.Namespace) -> int:
    goal = _resolve_goal(args)
    if not goal:
        return 2
    intent = _resolve_intent(args)
    wants_review = _resolve_wants_review(args)

    generator = _load_generator(args.agent, args.model)
    if generator is None:
        print(
            "error: `ralphus author` needs the 'runner' extra (pydantic-ai).\n"
            "install it with: uv sync --extra runner",
            file=sys.stderr,
        )
        return 2

    budget = Budget(
        max_tokens=args.budget_tokens,
        max_seconds=float(args.timeout_sec) if args.timeout_sec else None,
    )
    with DaemonClient(args.daemon_url) as client:
        try:
            outcome = author_and_submit(
                goal=goal,
                intent=intent,
                generator=generator,
                client=client,
                budget=budget,
                wants_review=wants_review,
                submit=not args.dry_run,
                hold=args.hold,
                label=args.label,
                max_attempts=args.max_attempts,
                on_event=lambda message: print(f"... {message}", file=sys.stderr),
            )
        except GeneratorAborted as exc:
            print(f"error: authoring aborted: {exc}", file=sys.stderr)
            return 1
        except AuthorError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 1
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    return _report_author(outcome, dry_run=args.dry_run)


def _report_author(outcome: AuthorOutcome, *, dry_run: bool) -> int:
    if dry_run:
        for i, draft in enumerate(outcome.drafts, 1):
            print(f"# --- draft {i} ---")
            print(draft)
        print(f"\n{len(outcome.drafts)} document(s) generated (dry run; nothing submitted).")
        return 0
    for record in outcome.submitted:
        print(f"submitted {record.run_id} ({record.state}) - {record.reviews} review(s)")
    for failure in outcome.failed:
        print(f"failed to submit {failure.path}: {failure.error}", file=sys.stderr)
    if not outcome.submitted:
        print("error: nothing was submitted", file=sys.stderr)
        return 1
    if outcome.review_required and outcome.reviews_created < 1:
        print("error: a review was requested but none was created", file=sys.stderr)
        return 1
    if outcome.failed:
        print(f"error: {len(outcome.failed)} document(s) failed to submit", file=sys.stderr)
        return 1
    print(f"ok: {len(outcome.submitted)} run(s), {outcome.reviews_created} review(s) created")
    return 0


def _render_concurrency(board: dict[str, Any]) -> None:
    d = board.get("daemon", {})
    print_kv(
        [
            ("running", d.get("running")),
            ("max_concurrent", d.get("max_concurrent")),
        ]
    )
    reviews = d.get("running_reviews") or []
    if reviews:
        print("\nrunning reviews:")
        for r in reviews:
            print(f"  {r.get('id')}  {r.get('name')}")


def _cmd_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            if getattr(args, "concurrency", False):
                emit(args.json, client.tasks(), _render_concurrency)
            elif args.run_id:
                emit(args.json, client.run(args.run_id), _print_run)
            else:
                emit(args.json, client.tasks(), _render_run_list)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    return 0


def _cmd_resources(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.resources()
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(res: dict[str, Any]) -> None:
        rows = res.get("resources", [])
        if not rows:
            print("no running sessions")
            return

        def _mb(row: dict[str, Any]) -> str:
            mem = row.get("mem_bytes")
            return f"{mem / (1024 * 1024):.0f}" if mem is not None else "-"

        print_table(
            ["RUN", "TASK", "SESSION", "PID", "CPU%", "MEM_MB"],
            [
                [
                    str(row.get("run_id", "")),
                    str(row.get("task_name", "")),
                    str(row.get("session_id", "")),
                    str(row.get("pid", "")),
                    f"{row.get('cpu_percent'):.1f}" if row.get("cpu_percent") is not None else "-",
                    _mb(row),
                ]
                for row in rows
            ],
        )

    emit(args.json, result, _render)
    return 0


def _cmd_graph(args: argparse.Namespace) -> int:
    if not args.global_ and not args.run_id:
        print("error: a run id is required unless --global is given", file=sys.stderr)
        return 2
    with DaemonClient(args.daemon_url) as client:
        try:
            if args.global_:
                data = client.global_graph(include_terminal=args.all)

                def _label(n: dict[str, Any]) -> str:
                    return f"{n.get('label') or n['id']} [{n.get('state')}]"
            else:
                data = client.run_graph(args.run_id)

                def _label(n: dict[str, Any]) -> str:
                    return f"{n.get('task_name')}/{n.get('session_id')}"
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(d: dict[str, Any]) -> None:
        nodes = [{**n, "label": _label(n)} for n in d.get("nodes", [])]
        edges = d.get("edges", [])
        if args.format == "dot":
            print(render_dot(nodes, edges))
        else:
            print(render_ascii(nodes, edges))

    emit(args.json, data, _render)
    return 0


def _looks_like_guardian_selector(selector: str) -> bool:
    return selector.startswith("@") or "#" in selector or selector.startswith("guardian")


def _cmd_get(args: argparse.Namespace) -> int:
    """A jq-lite field query over any entity's JSON view: resolve the selector
    to its object (reusing the same resolvers/renderers every other `show`
    command uses), then walk a dotted field path into it.
    """
    selector = args.selector
    with DaemonClient(args.daemon_url) as client:
        try:
            data: Any
            if _looks_like_guardian_selector(selector):
                resolved_g = resolve_guardian_selector(client, selector)
                data = client.guardian_get(resolved_g.guardian_id)
                if resolved_g.branch_id is not None:
                    data = next(
                        (
                            b
                            for b in data.get("branches", [])
                            if b.get("id") == resolved_g.branch_id
                        ),
                        None,
                    )
                    if data is None:
                        _print_selector_error(
                            SelectorError(f"no branch '{resolved_g.branch_id}'"),
                            json_mode=args.json,
                        )
                        return 2
            else:
                resolved_r = resolve_run_selector(client, selector)
                run = client.run(resolved_r.run_id)
                if resolved_r.kind == "run":
                    data = run
                elif resolved_r.kind == "task":
                    data = run["tasks"][resolved_r.task_idx]
                elif resolved_r.kind == "session":
                    data = run["tasks"][resolved_r.task_idx]["sessions"][resolved_r.session_idx]
                else:
                    data = _verify_step_for(run, resolved_r)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    if args.field:
        for part in args.field.split("."):
            try:
                if isinstance(data, list):
                    data = data[int(part)]
                elif isinstance(data, dict):
                    data = data[part]
                else:
                    raise KeyError(part)
            except (KeyError, IndexError, ValueError):
                _print_selector_error(
                    SelectorError(f"no field '{args.field}' (failed at '{part}')"),
                    json_mode=args.json,
                )
                return 2

    if isinstance(data, dict | list):
        print(json.dumps(data, indent=2, default=str))
    else:
        print(data)
    return 0


# ── RAL-140: `ralphus history` ───────────────────────────────────────────────

# Where a `--live` tail's resume cursor is cached between invocations, keyed
# per selector so two different sessions/verifies (or two independent
# watchers of the same one) never step on each other's read position. Only
# `length` + a short `tail` fingerprint are kept (not the full content) --
# enough to detect "the tmux session was recreated under the same name"
# (e.g. a restart) without unboundedly growing this cache file.
_HISTORY_CURSOR_DIR = Path.home() / ".ralphus" / "history_cursors"
_HISTORY_CURSOR_TAIL_LEN = 64


def _history_cursor_path(resolved: ResolvedSelector) -> Path:
    parts = (
        resolved.kind,
        resolved.run_id,
        resolved.task_idx,
        resolved.session_idx,
        resolved.verify_idx,
        resolved.verify_scope,
    )
    key = hashlib.sha256(":".join(str(p) for p in parts).encode()).hexdigest()[:20]
    return _HISTORY_CURSOR_DIR / f"{key}.json"


def _load_history_cursor(resolved: ResolvedSelector) -> dict[str, Any]:
    try:
        loaded = json.loads(_history_cursor_path(resolved).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"length": 0, "tail": ""}
    return cast(dict[str, Any], loaded) if isinstance(loaded, dict) else {"length": 0, "tail": ""}


def _save_history_cursor(resolved: ResolvedSelector, content: str) -> None:
    path = _history_cursor_path(resolved)
    path.parent.mkdir(parents=True, exist_ok=True)
    cursor = {"length": len(content), "tail": content[-_HISTORY_CURSOR_TAIL_LEN:]}
    path.write_text(json.dumps(cursor), encoding="utf-8")


def _diff_against_cursor(content: str, cursor: dict[str, Any]) -> tuple[str, bool]:
    """`(new_suffix, was_reset)`: the text beyond what `cursor` already saw,
    and whether a discontinuity was detected (the tail fingerprint no longer
    matches at the cached offset -- e.g. the tmux session was recreated under
    the same deterministic name after a restart), in which case the full
    `content` is returned instead so nothing is silently skipped.
    """
    length = int(cursor.get("length", 0))
    tail = str(cursor.get("tail", ""))
    if length == 0:
        return content, False
    check_start = max(0, length - len(tail))
    if len(content) >= length and content[check_start:length] == tail:
        return content[length:], False
    return content, True


def _emit_history_chunk(text: str, *, json_mode: bool) -> None:
    if not text:
        return
    if json_mode:
        print(json.dumps({"content": text}))
    else:
        print(text, end="")


def _advance_history_cursor(
    resolved: ResolvedSelector, cursor: dict[str, Any], content: str, *, json_mode: bool
) -> dict[str, Any]:
    new_text, reset = _diff_against_cursor(content, cursor)
    if reset and int(cursor.get("length", 0)):
        print("(session restarted; showing from the start)", file=sys.stderr)
    _emit_history_chunk(new_text, json_mode=json_mode)
    _save_history_cursor(resolved, content)
    return {"length": len(content), "tail": content[-_HISTORY_CURSOR_TAIL_LEN:]}


def _resolve_history_selector_or_none(
    client: DaemonClient, args: argparse.Namespace
) -> ResolvedSelector | None:
    """Resolve `args.selector` to a session or verify selector -- `history`
    is deliberately narrower than `listen`: a run/task/review has no single
    tmux session of its own to show history for.
    """
    try:
        resolved = resolve_run_selector(client, args.selector)
    except (SelectorError, DaemonError) as exc:
        if isinstance(exc, SelectorError):
            _print_selector_error(exc, json_mode=args.json)
        else:
            _print_daemon_error(exc, json_mode=args.json)
        return None
    if resolved.kind not in ("session", "verify"):
        _print_selector_error(
            SelectorError(
                f"'{args.selector}' is a {resolved.kind} selector -- "
                "history targets a session or verify step"
            ),
            json_mode=args.json,
        )
        return None
    return resolved


def _history_pane(client: DaemonClient, resolved: ResolvedSelector) -> dict[str, Any]:
    if resolved.kind == "session":
        return client.session_pane(resolved.run_id, resolved.task_idx, resolved.session_idx)
    return client.verify_pane(
        resolved.run_id,
        resolved.task_idx,
        resolved.verify_scope,
        resolved.session_idx,
        resolved.verify_idx,
    )


def _history_ghost_or_output(client: DaemonClient, resolved: ResolvedSelector) -> dict[str, Any]:
    """What `history` serves once a session/verify's tmux session is gone:
    `{content, found}`. Deliberately reads whatever RAL-136 (or the verify
    machinery that predates this ticket) already persisted, rather than
    inventing a second, parallel place to store session-derived content:

    - A task **session**'s record is its RAL-136 ghost -- the short handoff
      note the agent self-reports at the end of a session (see
      `cli/src/ralphus/runner/execute.py::_parse_ghost`), published under
      the same `session:<run_id>:<task_idx>:<session_idx>` URI the scheduler
      uses to inject it into dependent sessions' prompts. A 404 (never
      published) is treated as "nothing recorded", not an error.
    - A **verify** step's record is its already-stored `output` text --
      `daemon/src/scheduler.rs::run_verifies` has persisted a `prompt`-kind
      verify's full result via `set_verify_result` since long before this
      ticket; ghosts have no per-verify granularity to read instead.
    """
    if resolved.kind == "session":
        uri = f"session:{resolved.run_id}:{resolved.task_idx}:{resolved.session_idx}"
        try:
            ghost = client.ghost_get(uri)
        except DaemonError as exc:
            if exc.status_code == 404:
                return {"content": "", "found": False}
            raise
        content = str(ghost.get("content") or "")
        return {"content": content, "found": True}
    run = client.run(resolved.run_id)
    step = _verify_step_for(run, resolved)
    output = str(step.get("output") or "")
    return {"content": output, "found": bool(output)}


def _history_snapshot(client: DaemonClient, resolved: ResolvedSelector, *, json_mode: bool) -> int:
    """Plain (non-`--live`) `history`: a one-shot, non-blocking snapshot --
    the live pane if currently running, else the persisted ghost/output.
    """
    pane = _history_pane(client, resolved)
    if pane.get("active"):
        data = {"active": True, "content": pane.get("content", "")}
    else:
        final = _history_ghost_or_output(client, resolved)
        data = {
            "active": False,
            "content": final.get("content", ""),
            "found": bool(final.get("found")),
        }

    def _render(d: dict[str, Any]) -> None:
        content = d.get("content") or ""
        if not content:
            print("(no history recorded yet)")
            return
        print(content, end="" if content.endswith("\n") else "\n")

    emit(json_mode, data, _render)
    return 0


def _history_live(
    client: DaemonClient,
    resolved: ResolvedSelector,
    *,
    wait_until_valid: float | None,
    json_mode: bool,
    sleep_fn: Callable[[float], None] = time.sleep,
) -> int:
    cursor = _load_history_cursor(resolved)
    pane = _history_pane(client, resolved)

    if not pane.get("active"):
        if wait_until_valid is None:
            print(
                "error: nothing is currently live for this id "
                "(pass --wait-until-valid to wait for it to start/restart)",
                file=sys.stderr,
            )
            return 1
        started = time.monotonic()
        while not pane.get("active"):
            if time.monotonic() - started >= wait_until_valid:
                print(
                    f"error: timed out after {wait_until_valid:g}s waiting to become live",
                    file=sys.stderr,
                )
                return 1
            sleep_fn(1.0)
            pane = _history_pane(client, resolved)

    while pane.get("active"):
        cursor = _advance_history_cursor(
            resolved, cursor, pane.get("content", ""), json_mode=json_mode
        )
        sleep_fn(1.0)
        pane = _history_pane(client, resolved)

    # The session/verify just ended. Its ghost/output is a short, separately
    # curated note -- not a continuation of the raw tmux transcript just
    # tailed above -- so it's printed as its own labelled block rather than
    # diffed against the tailing cursor.
    final = _history_ghost_or_output(client, resolved)
    if final.get("found"):
        label = "note" if resolved.kind == "session" else "output"
        content = str(final.get("content") or "")
        if json_mode:
            print(json.dumps({"event": "ended", label: content}))
        else:
            print(f"\n--- session ended; recorded {label} ---")
            print(content, end="" if content.endswith("\n") else "\n")
    return 0


def _cmd_history(args: argparse.Namespace) -> int:
    if args.wait_until_valid is not None and not args.live:
        print("error: --wait-until-valid requires --live", file=sys.stderr)
        return 2
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_history_selector_or_none(client, args)
        if resolved is None:
            return 2
        try:
            if args.live:
                return _history_live(
                    client,
                    resolved,
                    wait_until_valid=args.wait_until_valid,
                    json_mode=args.json,
                    sleep_fn=time.sleep,
                )
            return _history_snapshot(client, resolved, json_mode=args.json)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)


# ── RAL-140: `ralphus listen` ─────────────────────────────────────────────────

# Valid `--until` values per selector kind. `"review"` mirrors the daemon's
# `GuardianStatus`; `"review-worktree"` mirrors `MergeStatus` (a review
# branch); the rest mirror `RunState`/`NodeState`.
_LISTEN_STATES: dict[str, tuple[str, ...]] = {
    "run": ("queued", "pending", "running", "done", "failed", "cancelled", "ignored"),
    "task": ("pending", "running", "done", "failed", "cancelled", "ignored"),
    "session": ("pending", "running", "done", "failed", "cancelled", "ignored"),
    "verify": ("pending", "running", "done", "failed", "cancelled", "ignored"),
    "review": (
        "collecting",
        "merging",
        "merge_failed",
        "in_review",
        "approved",
        "cancelled",
        "deployed",
    ),
    "review-worktree": ("pending", "ready", "in_progress", "done", "conflict_resolved", "failed"),
}


def _listen_status(client: DaemonClient, selector: str) -> tuple[str, str]:
    """Resolve `selector` and return `(kind, current_status)` -- `kind` is
    one of `_LISTEN_STATES`'s keys (`"review-worktree"`, not selector.py's
    plain `"review"` + a branch id, when a branch is addressed).
    """
    if _looks_like_guardian_selector(selector):
        resolved_g = resolve_guardian_selector(client, selector)
        guardian = client.guardian_get(resolved_g.guardian_id)
        if resolved_g.branch_id is None:
            return "review", str(guardian.get("status"))
        branch = next(
            (b for b in guardian.get("branches", []) if b.get("id") == resolved_g.branch_id),
            None,
        )
        if branch is None:
            raise SelectorError(f"no branch '{resolved_g.branch_id}' in this review")
        return "review-worktree", str(branch.get("merge_status"))

    resolved_r = resolve_run_selector(client, selector)
    run = client.run(resolved_r.run_id)
    if resolved_r.kind == "run":
        return "run", str(run.get("state"))
    if resolved_r.kind == "task":
        return "task", str(run["tasks"][resolved_r.task_idx].get("state"))
    if resolved_r.kind == "session":
        return "session", str(
            run["tasks"][resolved_r.task_idx]["sessions"][resolved_r.session_idx].get("state")
        )
    step = _verify_step_for(run, resolved_r)
    return "verify", str(step.get("state"))


def _listen_until(
    client: DaemonClient,
    selector: str,
    target: str,
    *,
    timeout: float | None,
    json_mode: bool = False,
    sleep_fn: Callable[[float], None] = time.sleep,
) -> tuple[int, str, str]:
    """Poll `selector` until it reaches `target` status.

    Returns `(exit_code, kind, status)` -- 0 on success, 2 for a bad selector
    or an `--until` value that's not valid for the resolved kind, 1 on
    timeout, or the daemon error's mapped exit code. Never raises:
    `SelectorError`/`DaemonError` are handled here so the caller only renders
    the result.
    """
    try:
        kind, status = _listen_status(client, selector)
    except SelectorError as exc:
        _print_selector_error(exc, json_mode=json_mode)
        return 2, "", ""
    except DaemonError as exc:
        _print_daemon_error(exc, json_mode=json_mode)
        return exit_code_for(exc), "", ""

    valid = _LISTEN_STATES[kind]
    if target not in valid:
        print(
            f"error: '{target}' is not a valid status for a {kind} (valid: {', '.join(valid)})",
            file=sys.stderr,
        )
        return 2, kind, status

    started = time.monotonic()
    while status != target:
        if timeout is not None and time.monotonic() - started >= timeout:
            print(
                f"error: timed out after {timeout:g}s waiting for '{selector}' "
                f"to reach '{target}' (currently '{status}')",
                file=sys.stderr,
            )
            return 1, kind, status
        sleep_fn(1.0)
        try:
            kind, status = _listen_status(client, selector)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=json_mode)
            return 2, kind, status
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=json_mode)
            return exit_code_for(exc), kind, status
    return 0, kind, status


def _cmd_listen(args: argparse.Namespace) -> int:
    target = args.until.lower()
    with DaemonClient(args.daemon_url) as client:
        code, kind, status = _listen_until(
            client,
            args.selector,
            target,
            timeout=args.timeout,
            json_mode=args.json,
            sleep_fn=time.sleep,
        )
    if code == 0:
        emit(
            args.json,
            {"selector": args.selector, "kind": kind, "status": status},
            lambda d: print(f"{d['selector']}: {d['status']}"),
        )
    return code


def _render_run_list(board: dict[str, Any]) -> None:
    runs = board.get("runs", [])
    if not runs:
        print("no runs")
        return
    print_table(
        ["ID", "STATE", "LABEL"],
        [[r.get("id", ""), r.get("state", ""), r.get("label") or ""] for r in runs],
    )


def _cmd_run_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            board = client.tasks(status=args.status, name=args.name, sort=args.sort)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, board, _render_run_list)
    return 0


def _render_run_detail(run: dict[str, Any]) -> None:
    print_kv(
        [("id", run.get("id")), ("label", run.get("label") or ""), ("state", run.get("state"))]
    )
    for ti, task in enumerate(run.get("tasks", [])):
        print(f"\n[{ti}] task {task.get('name')}  {task.get('state')}")
        for vi, v in enumerate(task.get("verify", [])):
            print(f"    verify/{vi}  {v.get('kind')}  {v.get('state')}")
        for si, s in enumerate(task.get("sessions", [])):
            name = s.get("name") or s.get("id")
            print(
                f"    [{si}] session {name}  {s.get('state')}  "
                f"agent={s.get('agent')} model={s.get('model') or '-'}"
            )
            for vi, v in enumerate(s.get("verify", [])):
                print(f"        verify/{vi}  {v.get('kind')}  {v.get('state')}")
    reviews = run.get("reviews", [])
    if reviews:
        print("\nreviews:")
        for r in reviews:
            print(f"  {r.get('id')}  {r.get('name')}  {r.get('status')}")


def _cmd_run_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            run = client.run(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, run, _render_run_detail)
    return 0


def _render_events(events: list[dict[str, Any]]) -> None:
    if not events:
        print("no events")
        return
    for e in events:
        ref = f" {e['ref']}" if e.get("ref") else ""
        print(f"[{e.get('at_ms')}] {e.get('scope')}{ref}: {e.get('message')}")


def _cmd_run_logs(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            events = client.run_logs(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, events, _render_events)
    return 0


def _render_dirtied(result: dict[str, Any]) -> None:
    print(f"state: {result.get('state')}")
    dirtied = result.get("dirtied") or []
    if dirtied:
        print("dirtied downstream runs:")
        for d in dirtied:
            print(f"  {d}")


def _cmd_run_set_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.set_status(args.run_id, args.state, kind="run")
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.run_id} -> {args.state}"))
    return 0


def _cmd_run_restart(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.restart_run(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, _render_dirtied)
    return 0


def _cmd_run_retry(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.retry_run(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.run_id} -> {r.get('state')}"))
    return 0


def _cmd_run_activate(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.activate_run(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.run_id} -> {r.get('state')}"))
    return 0


def _cmd_run_cancel(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.cancel(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.run_id} -> {r.get('state')}"))
    return 0


def _cmd_run_delete(args: argparse.Namespace) -> int:
    if not args.yes:
        answer = _prompt(f"Delete {args.run_id}? This cannot be undone. [y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            print("aborted")
            return 1
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.delete_run(args.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.run_id} -> {r.get('state')}"))
    return 0


def _cmd_run_rename(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.edit_run(args.run_id, label=args.label)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.run_id} renamed to '{args.label}'"))
    return 0


def _cmd_run_edit(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.edit_run(args.run_id, label=args.label)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.run_id} updated"))
    return 0


def _resolve_selector_or_none(
    client: DaemonClient, args: argparse.Namespace, *, want_kind: str
) -> ResolvedSelector | None:
    """Resolve `args.selector`, printing an error and returning None on failure.

    `want_kind` documents which selector shape the caller expects (task/session/
    verify) for the error message when the selector resolves to a different kind.
    """
    try:
        resolved = resolve_run_selector(client, args.selector)
    except (SelectorError, DaemonError) as exc:
        if isinstance(exc, SelectorError):
            _print_selector_error(exc, json_mode=args.json)
        else:
            _print_daemon_error(exc, json_mode=args.json)
        return None
    if resolved.kind != want_kind:
        _print_selector_error(
            SelectorError(f"'{args.selector}' is a {resolved.kind} selector, not a {want_kind}"),
            json_mode=args.json,
        )
        return None
    return resolved


def _cmd_task_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="task")
        if resolved is None:
            return 2
        try:
            run = client.run(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    task = run["tasks"][resolved.task_idx]

    def _render(t: dict[str, Any]) -> None:
        print_kv(
            [
                ("name", t.get("name")),
                ("project", t.get("project") or ""),
                ("state", t.get("state")),
                ("depends_on", ", ".join(t.get("depends_on", [])) or "-"),
            ]
        )
        for vi, v in enumerate(t.get("verify", [])):
            print(f"  verify/{vi}  {v.get('kind')}  {v.get('state')}")
        for si, s in enumerate(t.get("sessions", [])):
            print(f"  [{si}] session {s.get('name') or s.get('id')}  {s.get('state')}")

    emit(args.json, task, _render)
    return 0


def _cmd_task_set_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="task")
        if resolved is None:
            return 2
        try:
            result = client.set_status(
                resolved.run_id, args.state, kind="task", task_idx=resolved.task_idx
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} -> {args.state}"))
    return 0


def _cmd_task_restart_verify(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="task")
        if resolved is None:
            return 2
        try:
            result = client.restart_task_verify(resolved.run_id, resolved.task_idx, args.from_)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, _render_dirtied)
    return 0


def _cmd_task_edit(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="task")
        if resolved is None:
            return 2
        try:
            result = client.edit_task(
                resolved.run_id, resolved.task_idx, name=args.name, project=args.project
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} updated"))
    return 0


def _cmd_session_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            run = client.run(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    session = run["tasks"][resolved.task_idx]["sessions"][resolved.session_idx]

    def _render(s: dict[str, Any]) -> None:
        print_kv(
            [
                ("id", s.get("id")),
                ("name", s.get("name") or ""),
                ("state", s.get("state")),
                ("cwd", s.get("cwd") or ""),
                ("agent", s.get("agent")),
                ("model", s.get("model") or ""),
                ("tokens_in", s.get("tokens_in")),
                ("tokens_out", s.get("tokens_out")),
                ("prompt" if s.get("prompt") else "command", s.get("prompt") or s.get("command")),
            ]
        )
        for vi, v in enumerate(s.get("verify", [])):
            print(f"  verify/{vi}  {v.get('kind')}  {v.get('state')}")

    emit(args.json, session, _render)
    return 0


def _cmd_session_worktree(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            paths = client.run_worktrees(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    match = next(
        (
            p
            for p in paths
            if p.get("task_idx") == resolved.task_idx
            and p.get("session_idx") == resolved.session_idx
        ),
        None,
    )
    if match is None:
        _print_selector_error(
            SelectorError(f"no worktree recorded for '{args.selector}'"), json_mode=args.json
        )
        return 2
    emit(
        args.json,
        match,
        lambda m: print_kv(
            [("worktree", m.get("worktree") or "-"), ("project", m.get("project") or "-")]
        ),
    )
    return 0


def _cmd_session_reviews(args: argparse.Namespace) -> int:
    """The reviews a session's branch participates in.

    Already computed server-side onto `SessionView.reviews` (RAL-17) -- no new
    endpoint needed, just a CLI view over the existing run detail (Phase 5,
    worktree<->review linking direction: session -> reviews).
    """
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            run = client.run(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    session = run["tasks"][resolved.task_idx]["sessions"][resolved.session_idx]
    reviews = session.get("reviews", [])

    def _render(rs: list[dict[str, Any]]) -> None:
        if not rs:
            print("no reviews")
            return
        for r in rs:
            branch = f" branch={r['branch']}" if r.get("branch") else ""
            print(f"{r.get('id')}  {r.get('name')}  {r.get('status')}{branch}")

    emit(args.json, reviews, _render)
    return 0


def _cmd_session_set_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            result = client.set_status(
                resolved.run_id,
                args.state,
                kind="session",
                task_idx=resolved.task_idx,
                session_idx=resolved.session_idx,
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} -> {args.state}"))
    return 0


def _cmd_session_restart(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            result = client.restart_session(
                resolved.run_id, resolved.task_idx, resolved.session_idx
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, _render_dirtied)
    return 0


def _cmd_session_restart_verify(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            result = client.restart_session_verify(
                resolved.run_id, resolved.task_idx, resolved.session_idx, args.from_
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, _render_dirtied)
    return 0


def _cmd_session_edit(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            result = client.edit_session(
                resolved.run_id,
                resolved.task_idx,
                resolved.session_idx,
                cwd=args.cwd,
                agent=args.agent,
                model=args.model,
                prompt=args.prompt,
                command=args.command,
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} updated"))
    return 0


def _cmd_session_terminal(args: argparse.Namespace) -> int:
    """Print the `claude --resume` command instead of spawning a terminal.

    The daemon's own `open-terminal` endpoints spawn a GUI terminal window on
    the *daemon host*, which is meaningless for a headless CLI (Q2 in
    CLI_PARITY_PLAN.local.md). Everything needed to resume the conversation
    yourself -- the claude-code session uuid and the working directory -- is
    already on the session view, so print it instead of calling that endpoint.
    """
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="session")
        if resolved is None:
            return 2
        try:
            run = client.run(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    session = run["tasks"][resolved.task_idx]["sessions"][resolved.session_idx]
    claude_session_id = session.get("claude_session_id")
    if not claude_session_id:
        _print_selector_error(
            SelectorError(
                f"no claude_session_id available for '{args.selector}' "
                "-- the session may not have completed yet"
            ),
            json_mode=args.json,
        )
        return 1

    def _render(_s: dict[str, Any]) -> None:
        cmd = ["claude", "--resume", claude_session_id]
        if args.mode == "readonly":
            cmd += [
                "--dangerously-skip-permissions",
                "--append-system-prompt",
                "You are in read-only mode. You may only read files. Do NOT write, "
                "edit, delete, commit, or push anything.",
            ]
        print_kv([("cwd", session.get("cwd") or "-"), ("command", " ".join(cmd))])

    emit(args.json, session, _render)
    return 0


def _verify_step_for(run: dict[str, Any], resolved: ResolvedSelector) -> dict[str, Any]:
    task = run["tasks"][resolved.task_idx]
    if resolved.verify_scope == "session":
        session = task["sessions"][resolved.session_idx]
        step: dict[str, Any] = session["verify"][resolved.verify_idx]
        return step
    step = task["verify"][resolved.verify_idx]
    return step


def _cmd_verify_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="verify")
        if resolved is None:
            return 2
        try:
            run = client.run(resolved.run_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    step = _verify_step_for(run, resolved)

    def _render(v: dict[str, Any]) -> None:
        print_kv(
            [
                ("kind", v.get("kind")),
                ("state", v.get("state")),
                ("scope", resolved.verify_scope),
                ("agent", v.get("agent")),
                ("model", v.get("model") or ""),
                ("spec", v.get("spec")),
            ]
        )
        if v.get("output"):
            print("\noutput:")
            print(v["output"])

    emit(args.json, step, _render)
    return 0


def _cmd_verify_set_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="verify")
        if resolved is None:
            return 2
        try:
            result = client.set_status(
                resolved.run_id,
                args.state,
                kind="verify",
                task_idx=resolved.task_idx,
                session_idx=resolved.session_idx,
                verify_idx=resolved.verify_idx,
                verify_scope=resolved.verify_scope,
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} -> {args.state}"))
    return 0


def _cmd_verify_restart(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_selector_or_none(client, args, want_kind="verify")
        if resolved is None:
            return 2
        try:
            if resolved.verify_scope == "session":
                result = client.restart_session_verify(
                    resolved.run_id, resolved.task_idx, resolved.session_idx, resolved.verify_idx
                )
            else:
                result = client.restart_task_verify(
                    resolved.run_id, resolved.task_idx, resolved.verify_idx
                )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, _render_dirtied)
    return 0


def _cmd_review_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            guardians = client.guardian_list()
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    if args.status:
        wanted = {s.strip().lower() for s in args.status.split(",") if s.strip()}
        guardians = [g for g in guardians if str(g.get("status", "")).lower() in wanted]

    def _render(gs: list[dict[str, Any]]) -> None:
        if not gs:
            print("no reviews")
            return
        print_table(
            ["ID", "NAME", "STATUS", "BASE_BRANCH"],
            [
                [g.get("id", ""), g.get("name", ""), g.get("status", ""), g.get("base_branch", "")]
                for g in gs
            ],
        )

    emit(args.json, guardians, _render)
    return 0


def _cmd_review_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(g: dict[str, Any]) -> None:
        mp = g.get("merge_progress", {})
        print_kv(
            [
                ("id", g.get("id")),
                ("name", g.get("name")),
                ("status", g.get("status")),
                ("ready", g.get("ready")),
                ("base_branch", g.get("base_branch")),
                ("review_branch", g.get("review_branch") or ""),
                ("git_root", g.get("git_root")),
                ("merge_progress", f"{mp.get('done', 0)}/{mp.get('total', 0)}"),
                ("summary_state", g.get("summary_state")),
            ]
        )
        branches = g.get("branches", [])
        if branches:
            print("\nbranches:")
            for b in branches:
                detail = f"  ({b['detail']})" if b.get("detail") else ""
                ready = "  ready" if b.get("ready") else ""
                head = f"  [{b.get('position')}] {b.get('branch')}  {b.get('merge_status')}"
                print(f"{head}{ready}{detail}")
        if g.get("manual_commands"):
            print("\nmanual checks:")
            for i, cmd in enumerate(g["manual_commands"]):
                print(f"  [{i}] {cmd}")

    emit(args.json, guardian, _render)
    return 0


def _cmd_review_logs(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            events = client.guardian_logs(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, events, _render_events)
    return 0


def _review_status_verdict(g: dict[str, Any]) -> str:
    status = g.get("status")
    if g.get("ready"):
        return "ready for review"
    if status == "collecting":
        mp = g.get("merge_progress", {})
        return f"collecting ({mp.get('done', 0)}/{mp.get('total', 0)} branches merged)"
    if status == "merging":
        return "merging"
    return str(status)


def _cmd_review_status(args: argparse.Namespace) -> int:
    """Per-branch readiness plus a summary verdict -- the plan's flagship
    'is this review ready' query, backed by the `ready`/`merge_progress`
    fields Phase 5 added to GuardianView/BranchView."""
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(g: dict[str, Any]) -> None:
        rows = [
            [
                b.get("branch", ""),
                b.get("merge_status", ""),
                "ready" if b.get("ready") else "-",
                b.get("detail") or "",
            ]
            for b in g.get("branches", [])
        ]
        print_table(["BRANCH", "MERGE_STATUS", "READY", "DETAIL"], rows)
        mp = g.get("merge_progress", {})
        print(f"\nmerged: {mp.get('done', 0)}/{mp.get('total', 0)} ({mp.get('pct', 0):.1f}%)")
        print(f"verdict: {_review_status_verdict(g)}")

    emit(args.json, guardian, _render)
    return 0


def _cmd_review_worktrees(args: argparse.Namespace) -> int:
    """The worktrees/branches this review consumes, with per-branch source
    session. Already computed server-side onto `BranchView` -- no new endpoint
    needed, just a CLI view over the existing review detail (Phase 5,
    worktree<->review linking direction: review -> worktrees)."""
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(g: dict[str, Any]) -> None:
        branches = g.get("branches", [])
        if not branches:
            print("no branches")
            return
        rows = []
        for b in branches:
            src_run = b.get("source_run_id")
            source = (
                f"{src_run}/{b.get('source_task_idx')}/{b.get('source_session_idx')}"
                if src_run
                else "-"
            )
            rows.append(
                [str(b.get("position", "")), b.get("branch", ""), b.get("worktree") or "-", source]
            )
        print_table(["POS", "BRANCH", "WORKTREE", "SOURCE"], rows)

    emit(args.json, guardian, _render)
    return 0


def _cmd_review_create(args: argparse.Namespace) -> int:
    checks = [c.strip() for c in args.checks.split(",") if c.strip()] if args.checks else []
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.guardian_create(
                args.name,
                args.base_branch,
                args.git_root,
                checks=checks,
                skip_auto_build=args.skip_auto_build,
                skip_worktree_checks=args.skip_worktree_checks,
                skip_worktrees=args.skip_worktrees,
                review_type=args.review_type,
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(r.get("id")))
    return 0


def _cmd_review_rename(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_rename(resolved.guardian_id, args.name)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} renamed to '{args.name}'"))
    return 0


def _cmd_review_cancel(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_cancel(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.selector} -> {r.get('state')}"))
    return 0


def _cmd_review_delete(args: argparse.Namespace) -> int:
    if not args.yes:
        answer = _prompt(f"Delete {args.selector}? This cannot be undone. [y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            print("aborted")
            return 1
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_delete(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.selector} -> {r.get('state')}"))
    return 0


def _cmd_review_settings(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_settings(
                resolved.guardian_id,
                skip_auto_build=args.skip_auto_build,
                skip_worktree_checks=args.skip_worktree_checks,
                skip_worktrees=args.skip_worktrees,
                resolver_agent=args.resolver_agent,
                resolver_model=args.resolver_model,
                base_branch=args.base_branch,
                auto_pr_feedback=args.auto_pr_feedback,
            )
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} settings updated"))
    return 0


def _cmd_review_add_branch(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_add_branch(resolved.guardian_id, args.branch)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(
        args.json, result, lambda r: print(f"added '{args.branch}' at position {r.get('position')}")
    )
    return 0


def _cmd_review_reorder(args: argparse.Namespace) -> int:
    order = [b.strip() for b in args.order.split(",") if b.strip()]
    enabled: dict[str, bool] = {}
    for b in (args.disable or "").split(","):
        if b.strip():
            enabled[b.strip()] = False
    for b in (args.enable or "").split(","):
        if b.strip():
            enabled[b.strip()] = True
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_arrange(resolved.guardian_id, order, enabled=enabled)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} reordered; rebase started"))
    return 0


def _cmd_review_merge(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_merge(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} merge started"))
    return 0


def _cmd_review_restart_merge(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_cancel_and_merge(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} merge restarted"))
    return 0


def _cmd_review_force_start(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_force_start(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} force-started"))
    return 0


def _cmd_review_approve(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_approve(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(f"{args.selector} -> {r.get('state')}"))
    return 0


def _resolve_guardian_branch_or_none(client: DaemonClient, args: argparse.Namespace) -> Any:
    try:
        resolved = resolve_guardian_selector(client, args.selector)
    except SelectorError as exc:
        _print_selector_error(exc, json_mode=args.json)
        return None
    if resolved.branch is None:
        _print_selector_error(
            SelectorError(f"'{args.selector}' does not name a branch (use guardian#branch)"),
            json_mode=args.json,
        )
        return None
    return resolved


def _cmd_review_feedback(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_guardian_branch_or_none(client, args)
        if resolved is None:
            return 2
        try:
            result = client.guardian_feedback(resolved.guardian_id, resolved.branch_id, args.text)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"feedback posted on {args.selector}"))
    return 0


def _cmd_review_dismiss_reenable(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_guardian_branch_or_none(client, args)
        if resolved is None:
            return 2
        try:
            result = client.guardian_dismiss_reenable(resolved.guardian_id, resolved.branch_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} re-enable notice dismissed"))
    return 0


def _cmd_review_move_branch(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_guardian_branch_or_none(client, args)
        if resolved is None:
            return 2
        try:
            to_resolved = resolve_guardian_selector(client, args.to_review)
            result = client.guardian_move_branch(
                resolved.guardian_id, resolved.branch_id, to_resolved.guardian_id
            )
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(
        args.json,
        result,
        lambda _r: print(f"{args.selector} moved to {args.to_review}; rebuilding both reviews"),
    )
    return 0


def _cmd_review_base_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            branches = client.guardian_base_branches(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(bs: list[str]) -> None:
        if not bs:
            print("no candidate base branches")
            return
        for b in bs:
            print(b)

    emit(args.json, branches, _render)
    return 0


def _cmd_review_base_set(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_change_base(resolved.guardian_id, args.branch)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.selector} base branch -> '{args.branch}'"))
    return 0


def _cmd_review_pr_submit(args: argparse.Namespace) -> int:
    pr_spec: dict[str, Any] = {}
    if args.alias:
        pr_spec["branch_alias"] = args.alias
    if args.title:
        pr_spec["title"] = args.title
    if args.description:
        pr_spec["description"] = args.description
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            if not args.combined:
                # `--position N` is the human-friendly ordinal; resolve it to
                # the branch's stable id (RAL-122) before sending -- the API
                # addresses a branch by id, not by its reorder-mutable position.
                guardian = client.guardian_get(resolved.guardian_id)
                branches = guardian.get("branches", [])
                match = next(
                    (b for b in branches if int(b.get("position", -1)) == args.position), None
                )
                if match is None:
                    raise SelectorError(f"no branch at position {args.position} in this review")
                pr_spec["branch_id"] = match["id"]
            result = client.guardian_submit_prs(resolved.guardian_id, [pr_spec])
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"submitting PR for {args.selector}..."))
    return 0


def _cmd_review_pr_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_list_prs(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(rows: list[dict[str, Any]]) -> None:
        if not rows:
            print("no PRs submitted yet")
            return
        for r in rows:
            bid = r.get("branch_id")
            pos_label = f"branch {bid}" if bid is not None else "combined"
            print(f"{r['id']}  {pos_label}  #{r.get('pr_number')}  {r['state']}  {r['title']}")

    emit(args.json, result, _render)
    return 0


def _cmd_review_pr_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.pr_get(args.pr_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(
        args.json,
        result,
        lambda r: print(f"{r['id']}: {r['title']} (#{r.get('pr_number')}, {r['state']})"),
    )
    return 0


def _cmd_review_pr_find(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.pr_find(args.forge, args.repo, args.pr_number)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: print(r["id"]))
    return 0


def _cmd_review_pr_update(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.pr_update(
                args.pr_id,
                pr_number=args.pr_number,
                pr_url=args.pr_url,
                branch_alias=args.branch_alias,
                state=args.state,
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"{args.pr_id} updated"))
    return 0


def _cmd_review_pr_comments(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.pr_comments(args.pr_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)

    def _render(rows: list[dict[str, Any]]) -> None:
        if not rows:
            print("no comments")
            return
        for c in rows:
            tag = "actioned" if c.get("actioned") else "new"
            print(f"[{tag}] {c['author']}: {c['body']}")

    emit(args.json, result, _render)
    return 0


def _cmd_review_pr_pull_feedback(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.pr_action_feedback(args.pr_id)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"pulling feedback for {args.pr_id}..."))
    return 0


def _cmd_review_branch_set_enabled(args: argparse.Namespace, *, enabled: bool) -> int:
    with DaemonClient(args.daemon_url) as client:
        resolved = _resolve_guardian_branch_or_none(client, args)
        if resolved is None:
            return 2
        try:
            guardian = client.guardian_get(resolved.guardian_id)
            order = [b["branch"] for b in guardian.get("branches", [])]
            result = client.guardian_arrange(
                resolved.guardian_id, order, enabled={resolved.branch: enabled}
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    verb = "enabled" if enabled else "disabled"
    emit(args.json, result, lambda _r: print(f"{args.selector} {verb}; rebase started"))
    return 0


def _cmd_review_branch_enable(args: argparse.Namespace) -> int:
    return _cmd_review_branch_set_enabled(args, enabled=True)


def _cmd_review_branch_disable(args: argparse.Namespace) -> int:
    return _cmd_review_branch_set_enabled(args, enabled=False)


def _print_command_with_cwd(cwd: str, command: str) -> None:
    print_kv([("cwd", cwd), ("command", command)])


def _cmd_review_checks_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    commands = guardian.get("manual_commands", [])

    def _render(cmds: list[str]) -> None:
        if not cmds:
            print("no manual checks available (review may still be building)")
            return
        for i, cmd in enumerate(cmds):
            print(f"[{i}] {cmd}")

    emit(args.json, commands, _render)
    return 0


def _cmd_review_checks_run(args: argparse.Namespace) -> int:
    """Print the resolved command(s) + cwd rather than have the daemon spawn a
    terminal on its own host (same reasoning as `session terminal` -- see Q2 /
    C2 in CLI_PARITY_PLAN.local.md: `run-manual-commands` calls
    `spawn_in_terminal` on the *daemon*, which is meaningless for a headless CLI).
    """
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    commands: list[str] = guardian.get("manual_commands", [])
    if not commands:
        _print_selector_error(
            SelectorError("no manual checks available (review may still be building)"),
            json_mode=args.json,
        )
        return 1
    if args.all:
        indices = list(range(len(commands)))
    elif args.index:
        bad = [i for i in args.index if not 0 <= i < len(commands)]
        if bad:
            _print_selector_error(
                SelectorError(f"check index out of range: {bad} (have {len(commands)})"),
                json_mode=args.json,
            )
            return 2
        indices = args.index
    else:
        indices = list(range(len(commands)))
    cwd = str(guardian.get("combined_worktree") or guardian.get("git_root") or "")

    def _render(_cmds: list[str]) -> None:
        for i in indices:
            print(f"[{i}]")
            _print_command_with_cwd(cwd, commands[i])

    emit(args.json, [commands[i] for i in indices], _render)
    return 0


def _cmd_review_action_list(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    hints = guardian.get("action_hints", [])

    def _render(hs: list[dict[str, Any]]) -> None:
        if not hs:
            print("no action hints declared")
            return
        for i, h in enumerate(hs):
            kind = "command" if h.get("command") else "prompt"
            print(f"[{i}] {h.get('label')}  ({kind})")

    emit(args.json, hints, _render)
    return 0


def _cmd_review_action_run(args: argparse.Namespace) -> int:
    """See `_cmd_review_checks_run` -- prints rather than calls the
    terminal-spawning endpoint. `prompt`-kind hints aren't runnable at all yet
    (the daemon itself returns 501 for these)."""
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            guardian = client.guardian_get(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    hints: list[dict[str, Any]] = guardian.get("action_hints", [])
    if not 0 <= args.index < len(hints):
        _print_selector_error(
            SelectorError(f"action hint index {args.index} out of range (have {len(hints)})"),
            json_mode=args.json,
        )
        return 2
    hint = hints[args.index]
    if not hint.get("command"):
        _print_selector_error(
            SelectorError("prompt-kind action hints cannot be run directly yet"),
            json_mode=args.json,
        )
        return 1
    cwd = str(guardian.get("combined_worktree") or guardian.get("git_root") or "")
    emit(args.json, hint, lambda h: _print_command_with_cwd(cwd, h["command"]))
    return 0


def _render_chat_messages(messages: dict[str, Any]) -> None:
    msgs = messages.get("messages", [])
    if not msgs:
        print("no messages")
        return
    for m in msgs:
        print(f"[{m.get('seq')}] {m.get('role')}: {m.get('text')}")


def _cmd_review_chat_send(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_chat(resolved.guardian_id, args.text)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print("message posted"))
    return 0


def _cmd_review_chat_show(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            messages = client.guardian_messages(resolved.guardian_id)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, messages, _render_chat_messages)
    return 0


def _cmd_review_chat_fork(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            resolved = resolve_guardian_selector(client, args.selector)
            result = client.guardian_chat_fork(resolved.guardian_id, args.seq, args.text)
        except SelectorError as exc:
            _print_selector_error(exc, json_mode=args.json)
            return 2
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"forked at seq={args.seq}"))
    return 0


def _cmd_clear(args: argparse.Namespace) -> int:
    states: list[str] = []
    if args.status:
        states = [s.strip().lower() for s in args.status.split(",") if s.strip()]
        invalid = [s for s in states if s not in _RUN_STATES]
        if invalid:
            print(
                f"error: unknown status {', '.join(invalid)} (valid: {', '.join(_RUN_STATES)})",
                file=sys.stderr,
            )
            return 2
    if not args.all_ and not states:
        print(
            "error: pass --all to clear everything, or --status to filter by run state",
            file=sys.stderr,
        )
        return 2
    if not args.yes:
        what = f"runs in states [{', '.join(states)}]" if states else "ALL tasks and reviews"
        answer = _prompt(f"Delete {what}? This cannot be undone. [y/N] ").strip().lower()
        if answer not in ("y", "yes"):
            print("aborted")
            return 1
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.clear(states=states, keep_temporary=args.keep_temporary)
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(
        args.json,
        result,
        lambda r: print(
            f"cleared {r.get('runs_deleted', 0)} run(s), "
            f"{r.get('guardians_deleted', 0)} review(s); "
            f"{r.get('worktrees_purged', 0)} worktree(s) purged"
        ),
    )
    return 0


class _PathParts(TypedDict):
    kind: str
    run_id: str
    task_idx: int
    session_idx: int
    verify_idx: int
    verify_scope: str


def _parse_queue_path(path: str) -> _PathParts | None:
    """Parse a queue item path into set-status fields (mirrors the daemon).

    Grammar: ``run/t<ti>/s<si>`` (session), ``run/t<ti>/s<si>/v<vi>`` (session
    verify), ``run/t<ti>/tv<vi>`` (task verify), or a bare run id.
    """
    segs = path.split("/")
    try:
        if len(segs) == 1:
            return {
                "kind": "run",
                "run_id": segs[0],
                "task_idx": 0,
                "session_idx": -1,
                "verify_idx": -1,
                "verify_scope": "",
            }
        if len(segs) == 3:
            run, t, s = segs
            if not t.startswith("t"):
                return None
            ti = int(t[1:])
            if s.startswith("tv"):
                return {
                    "kind": "verify",
                    "run_id": run,
                    "task_idx": ti,
                    "session_idx": -1,
                    "verify_idx": int(s[2:]),
                    "verify_scope": "task",
                }
            if s.startswith("s"):
                return {
                    "kind": "session",
                    "run_id": run,
                    "task_idx": ti,
                    "session_idx": int(s[1:]),
                    "verify_idx": -1,
                    "verify_scope": "",
                }
            return None
        if len(segs) == 4:
            run, t, s, v = segs
            return {
                "kind": "verify",
                "run_id": run,
                "task_idx": int(t[1:]),
                "session_idx": int(s[1:]),
                "verify_idx": int(v[1:]),
                "verify_scope": "session",
            }
    except ValueError:
        return None
    return None


def _print_queue_order(order: list[str]) -> None:
    print("new queue order:")
    for i, p in enumerate(order):
        print(f"  {i:>3}  {p}")


def _render_queue_list(result: dict[str, Any], *, show_all: bool) -> None:
    items = result.get("items", [])
    shown = [i for i in items if show_all or i.get("readiness") == "ready"]
    if not shown:
        hint = "" if show_all else " ready to run (use --all to include blocked items)"
        print(f"no queued work{hint}")
        return
    for i in shown:
        rank = i.get("queue_rank")
        rank_s = f"{rank:g}" if rank is not None else "-"
        blocked = i.get("blocked_by") or []
        tail = f"  <- {', '.join(blocked)}" if blocked else ""
        rd = str(i.get("readiness"))
        print(f"  [{rank_s:>4}] {rd:<9} {i.get('path')}  {i.get('name')}{tail}")


def _cmd_queue_list(args: argparse.Namespace) -> int:
    show_all = getattr(args, "all_", False)
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.queue()
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: _render_queue_list(r, show_all=show_all))
    return 0


def _cmd_queue_reorder(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.queue_reorder(list(args.paths))
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: _print_queue_order(list(r.get("order", []))))
    return 0


def _cmd_queue_set_position(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.queue_set_position(
                list(args.paths), args.to, absolute=not args.relative
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda r: _print_queue_order(list(r.get("order", []))))
    return 0


def _cmd_queue_set_status(args: argparse.Namespace) -> int:
    parsed = _parse_queue_path(args.path)
    if parsed is None:
        print(f"error: could not parse item path '{args.path}'", file=sys.stderr)
        return 2
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.set_status(
                parsed["run_id"],
                args.state,
                kind=parsed["kind"],
                task_idx=parsed["task_idx"],
                session_idx=parsed["session_idx"],
                verify_idx=parsed["verify_idx"],
                verify_scope=parsed["verify_scope"],
            )
        except DaemonError as exc:
            _print_daemon_error(exc, json_mode=args.json)
            return exit_code_for(exc)
    emit(args.json, result, lambda _r: print(f"set {args.path} -> {args.state}"))
    return 0


def _print_run(run: dict[str, object]) -> None:
    print(f"{run.get('id')}  {run.get('state')}")
    tasks = run.get("tasks")
    if isinstance(tasks, list):
        for task in tasks:
            if isinstance(task, dict):
                print(f"  task {task.get('name')}: {task.get('state')}")


def _cmd_configuration_show(args: argparse.Namespace) -> int:
    config = load_config(include_local=not args.no_local)

    print("Sources (in resolution order, later wins):")
    if not config.sources:
        print("  (none)")
    else:
        for i, src in enumerate(config.sources, 1):
            label = config.source_labels.get(src, "unknown")
            print(f"  {i}. {src}  ({label})")

    cwd_toml = Path.cwd() / ".ralphus.toml"
    if cwd_toml.exists() and cwd_toml not in config.sources:
        if args.no_local:
            print(f"\nNote: {cwd_toml} exists but was excluded by --no-local.")
        else:
            print(f"\nNote: {cwd_toml} exists but is not in the resolution chain.")
            print("      Add it to RALPHUS_CONFIGURATION_PATH to include it.")

    def _prov(key: str) -> str:
        src = config.provenance.get(key)
        return f"from {src}" if src else "default"

    mt = config.task.maximum_timeout_seconds
    lp = config.daemon.log_path or "not set"
    ll = config.daemon.log_level or "not set"

    print("\nResolved values:")
    print(f"  task.maximum_timeout_seconds  = {mt}  ({_prov('task.maximum_timeout_seconds')})")
    print(f"  daemon.log_path               = {lp}  ({_prov('daemon.log_path')})")
    print(f"  daemon.log_level              = {ll}  ({_prov('daemon.log_level')})")
    return 0


def _cmd_check_health(args: argparse.Namespace) -> int:
    symbols = {"pass": "OK  ", "warn": "WARN", "fail": "FAIL"}
    results = run_checks(args.daemon_url, enable_developer_checks=args.enable_developer_checks)
    for section, title in ((CORE, "Core"), (DEVELOPER, "Developer")):
        section_results = [r for r in results if r.section == section]
        if not section_results:
            continue
        print(f"{title}:")
        for r in section_results:
            print(f"  [{symbols.get(r.status, '?')}] {r.name}: {r.detail}")

    file_issues = validate_config_files()
    if file_issues:
        print("\nConfiguration file issues:")
        for fi in file_issues:
            print(f"  {fi.path}  ({fi.label})")
            if fi.syntax_error:
                print(f"    - TOML syntax error: {fi.syntax_error}")
            for issue in fi.issues:
                print(f"    - {issue}")

    failed_checks = [r for r in results if r.is_fail]
    total_failed = len(failed_checks) + len(file_issues)
    if total_failed:
        print(f"\n{total_failed} check(s) failed.")
        return 1
    return 0


def _cmd_completion(args: argparse.Namespace) -> int:
    if args.shell == "bash":
        print(_BASH_COMPLETION)
    return 0


def _cmd_show_tutor(_args: argparse.Namespace) -> int:
    print(TASK_TUTOR)
    return 0


def _cmd_initialize_git(args: argparse.Namespace) -> int:
    """Enable git rerere (+autoupdate) in the target repository.

    ``rerere`` (reuse recorded resolution) makes git record how a merge conflict
    was resolved and replay that resolution automatically the next time the same
    conflict appears. This matters for Guardian reviews: when a base branch moves,
    the review stack is rebased again, and without rerere the same conflict has to
    be re-resolved from scratch every time. Enabling it is per-repository and git
    keeps it off by default, so this configures whichever repo owns the current
    directory (or ``--path``).
    """
    # Accept relative or absolute paths on either OS. `expanduser` resolves a
    # leading `~`; `resolve` makes a relative path absolute (relative to the
    # current directory) and normalises separators so a mix of `/` and `\` on
    # Windows still lands on the right directory. `strict=False` keeps a missing
    # path from raising here — the git working-tree probe below reports it cleanly.
    raw = args.path if args.path is not None else Path.cwd()
    target = raw.expanduser()
    with contextlib.suppress(OSError):
        target = target.resolve(strict=False)
    if shutil.which("git") is None:
        print("error: git is not on PATH", file=sys.stderr)
        return 2
    # Confirm the target is inside a git working tree before writing config.
    try:
        probe = subprocess.run(
            ["git", "-C", str(target), "rev-parse", "--is-inside-work-tree"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as exc:
        print(f"error: could not run git: {exc}", file=sys.stderr)
        return 2
    if probe.returncode != 0 or probe.stdout.strip() != "true":
        print(
            f"error: {target} is not inside a git working tree "
            "(run this from a repository, or pass --path)",
            file=sys.stderr,
        )
        return 2
    for key, value in (("rerere.enabled", "true"), ("rerere.autoupdate", "true")):
        result = subprocess.run(
            ["git", "-C", str(target), "config", key, value],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            print(f"error: git config {key} failed: {result.stderr.strip()}", file=sys.stderr)
            return 1
        print(f"set {key} = {value}")
    print(
        "git rerere enabled: conflict resolutions during review rebases will now be "
        "recorded and replayed automatically."
    )
    return 0


def _ensure_utf8_streams() -> None:
    """Force stdout/stderr to UTF-8.

    Event messages and review text routinely contain non-ASCII characters
    (e.g. the daemon's transition log uses "→"). Windows defaults stdout/
    stderr to the console codepage (cp1252 etc.) whenever they aren't an
    interactive console -- piped, redirected, or run under CI -- and a plain
    `print` of such a message then crashes with `UnicodeEncodeError`.
    """
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            reconfigure(encoding="utf-8", errors="replace")


def _cmd_project_git(args: argparse.Namespace) -> int:
    """Register a git repository as a project the daemon can resolve.

    Once registered, a task's ``project = "<name>"`` field can be referenced
    by any of its sessions using the placeholder ``cwd``
    ``"ralphus:new-worktree/<branch>"``; the daemon materializes (or reuses) a
    git worktree for that branch under ``.git/.ralphus_worktrees/`` before the
    session runs (RAL-100).
    """
    target = args.path.expanduser()
    with contextlib.suppress(OSError):
        target = target.resolve(strict=False)
    with DaemonClient(args.daemon_url) as client:
        try:
            client.register_project(args.name, str(target), description=args.description)
        except DaemonError as exc:
            _print_daemon_error(exc)
            return 1
    print(f'registered project "{args.name}" -> {target}')
    return 0


def _cmd_project_list(args: argparse.Namespace) -> int:
    """List every project registered with the daemon."""
    with DaemonClient(args.daemon_url) as client:
        try:
            projects = client.list_projects().get("projects", [])
        except DaemonError as exc:
            _print_daemon_error(exc)
            return 2
    if not projects:
        print("no registered projects")
        return 0
    for p in projects:
        print(f"{p.get('name'):<20}  {p.get('vcs'):<4}  {p.get('path')}")
        description = p.get("description")
        if description:
            if args.short:
                description = _elide_right(description, _SHORT_DESCRIPTION_MAX)
            print(f"    {description}")
    return 0


def _cmd_project_get(args: argparse.Namespace) -> int:
    """Show one registered project's details by its exact name."""
    with DaemonClient(args.daemon_url) as client:
        try:
            p = client.get_project(args.name)
        except DaemonError as exc:
            _print_daemon_error(exc)
            return 2
    print(f"name:        {p.get('name')}")
    print(f"vcs:         {p.get('vcs')}")
    print(f"path:        {p.get('path')}")
    print(f"description: {p.get('description') or ''}")
    return 0


def _cmd_agent_list(_args: argparse.Namespace) -> int:
    """List supported agent backends and the models each is allowed to run.

    This is a hand-maintained, purely informational catalog (see
    ``ralphus.agents``) -- ralphus itself does not enforce a model allow-list
    for any agent; a fixed model list here reflects what the underlying
    CLI/API actually accepts, not a ralphus-side validation rule.
    """
    for a in KNOWN_AGENTS:
        label = a.name if not a.aliases else f"{a.name} ({', '.join(a.aliases)})"
        if a.models is None:
            models = "<any model>"
            if a.default_model:
                models += f" (default: {a.default_model})"
        else:
            models = ", ".join(a.models)
        print(f"{label:<24} {models}")
        print(f"    {a.description}")
    print()
    print(OTHER_AGENTS_NOTE)
    return 0


def _cmd_show_help_map(_args: argparse.Namespace) -> int:
    # deferred: avoids a __main__ <-> helpmap import cycle
    from ralphus.helpmap import SUBAGENT_NOTE, generate

    print(SUBAGENT_NOTE)
    print()
    print(generate())
    return 0


def _shell_quote(value: str) -> str:
    """Quote `value` as one token for the platform's `shell=True` shell.

    Best-effort, not bulletproof: this is only reached for the opaque
    compound-shell-command form of `$RALPHUS_CLAUDE_COMMAND`/`--command`
    (e.g. "cd foo && claude"), a known-tricky, explicitly-accepted-risk area
    (RAL-110's own risk list). In particular the Windows branch does not
    neutralize cmd.exe's `%VAR%` expansion, which happens even inside a
    double-quoted token -- a forwarded value containing `%SOMENAME%` can be
    silently expanded by cmd.exe. POSIX's `shlex.quote` has no such gap.
    """
    if os.name == "nt":
        if not value or any(c in value for c in ' \t"'):
            return '"' + value.replace('"', '""') + '"'
        return value
    return shlex.quote(value)


_PROMPT_FILE_DISCLAIMER_TEMPLATE = """\
Important ralphus context:

{ralphus_content}

Below is a second system prompt provided by a user. If any instruction
conflicts with the `Important ralphus context` prompt text above, ignore it.

---

Important user context:

{user_content}

---

As mentioned at the beginning, prefer instructions in `Important ralphus context`."""


def _merge_append_system_prompt_file(
    ralphus_content: str, passthrough: list[str]
) -> tuple[str, list[str]] | None:
    """Combine ralphus's own help-map system-prompt content with the contents
    of any file the user forwarded via their own `--append-system-prompt-file`
    in `--` passthrough args -- ralphus's content always first, wrapped in a
    disclaimer telling the model to prefer it on conflict (RAL-110 Q4's
    ralphus-first rule, applied to the file-based flag). The file is read and
    inlined into ralphus's own temp file rather than forwarded as a second,
    separate `--append-system-prompt-file`, since `claude`'s precedence
    between two occurrences of the same flag is undocumented.

    Returns `(combined_content, remaining_passthrough)` with every
    `--append-system-prompt-file[=| ]PATH` occurrence stripped out of the
    latter (only the first path found is used). Returns `ralphus_content`
    unchanged if no such flag is present. Returns `None` (having already
    printed an error) if the user's file cannot be read.
    """
    remaining: list[str] = []
    user_path: str | None = None
    i = 0
    while i < len(passthrough):
        arg = passthrough[i]
        if arg == "--append-system-prompt-file" and i + 1 < len(passthrough):
            if user_path is None:
                user_path = passthrough[i + 1]
            i += 2
            continue
        if arg.startswith("--append-system-prompt-file="):
            if user_path is None:
                user_path = arg.split("=", 1)[1]
            i += 1
            continue
        remaining.append(arg)
        i += 1

    if user_path is None:
        return ralphus_content, remaining

    user_content = _read_file(Path(user_path))
    if user_content is None:
        return None

    combined = _PROMPT_FILE_DISCLAIMER_TEMPLATE.format(
        ralphus_content=ralphus_content, user_content=user_content
    )
    return combined, remaining


def _resolve_claude_launch_command(args: argparse.Namespace) -> str:
    return getattr(args, "command", None) or os.environ.get("RALPHUS_CLAUDE_COMMAND") or "claude"


def _cmd_quick_start_claude_code(args: argparse.Namespace) -> int:
    """Launch Claude Code primed with the full ralphus CLI help-map (RAL-110).

    Writes the help-map to a throwaway temp file and points Claude Code at it
    via the dedicated `--append-system-prompt-file` flag, so the file's
    content becomes Claude Code's system prompt without hitting OS
    command-line length limits. Unlike the `--append-system-prompt @path`
    file-injection syntax (still used elsewhere, e.g.
    `claude_code_backend.py`), the path here is its own dedicated argument
    value rather than embedded inside a text value, so a path with a space
    in it (e.g. a Windows profile directory) cannot be truncated by
    `claude`'s own `@path` parser deciding where the reference ends
    (RAL-110). If the forwarded `--` args include their own
    `--append-system-prompt-file`, its contents are read and folded into
    this same temp file (ralphus's context first, then a disclaimer, then
    the user's) instead of forwarding a second, separate occurrence of the
    flag whose precedence would be undocumented.
    """
    # deferred: avoids a __main__ <-> helpmap import cycle
    from ralphus.helpmap import SUBAGENT_NOTE, generate

    help_map_file_content = (
        "You are Ralphus. You orchestrate the `ralphus` CLI as an autonomous agent. Its "
        "complete command surface -- every subcommand, flag, and expected value type -- is "
        "documented below. Use `ralphus <command> --help` for details on any specific "
        "command.\n\n" + SUBAGENT_NOTE + "\n\n" + generate()
    )
    fd, tmp_path_str = tempfile.mkstemp(suffix=".md", prefix="ralphus-help-map-")
    tmp_path = Path(tmp_path_str)
    try:
        passthrough = list(getattr(args, "claude_args", None) or [])
        merged = _merge_append_system_prompt_file(help_map_file_content, passthrough)
        if merged is None:
            os.close(fd)
            return 2
        file_content, passthrough = merged

        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(file_content)

        case = "ralphus-only" if file_content == help_map_file_content else "ralphus+user-file"
        print(
            f"ralphus [spec] quick-start-claude-code system-prompt case={case} "
            f"ralphus_len={len(help_map_file_content)} combined_len={len(file_content)}",
            file=sys.stderr,
        )

        raw_command = _resolve_claude_launch_command(args)
        extra_args = [
            "--dangerously-skip-permissions",
            "--append-system-prompt-file",
            str(tmp_path),
            *passthrough,
        ]
        compound = is_compound_shell_command(raw_command)
        print(
            f"ralphus [runner] quick-start-claude-code spawning command={raw_command!r} "
            f"compound={compound}",
            file=sys.stderr,
        )

        if compound:
            # `raw_command` is opaque shell syntax (e.g. "cd foo bar ; ./claude") --
            # let the platform shell interpret it, with our args appended to its tail.
            tail = " ".join(_shell_quote(a) for a in extra_args)
            completed = subprocess.run(f"{raw_command} {tail}", shell=True, check=False)
        else:
            program = unquote_path(raw_command)
            # Resolve to a full path so a Windows shim (.cmd/.exe) is found reliably
            # (same reasoning as claude_code_backend.py).
            program = shutil.which(program) or program
            try:
                completed = subprocess.run([program, *extra_args], check=False)
            except OSError as exc:
                print(f"error: could not launch claude ({program!r}): {exc}", file=sys.stderr)
                return 2
        print(
            f"ralphus [runner] quick-start-claude-code exited returncode={completed.returncode}",
            file=sys.stderr,
        )
        return completed.returncode
    finally:
        with contextlib.suppress(OSError):
            tmp_path.unlink()


def _split_passthrough(raw_args: list[str]) -> tuple[list[str], list[str]]:
    """Split `raw_args` at the first bare `--`, but only for a `quick-start
    claude-code` invocation -- that's the only subcommand with a `-- ARGS`
    passthrough. Every other subcommand keeps argparse's own end-of-options
    handling of `--` untouched (splitting unconditionally would silently eat
    a value any subcommand escaped with the standard `--` idiom, e.g. `review
    chat send @name -- "-1 point deduction"`).
    """
    # `quick-start` must be the very first token -- not just present somewhere
    # before the "--" -- so an unrelated command can never coincidentally
    # trip this (e.g. a positional/flag value that happens to equal the
    # literal string "quick-start"). Quick-start never needs a preceding
    # global flag (--daemon-url/--json don't apply to it), so this isn't a
    # real usage restriction.
    if not raw_args or raw_args[0] != "quick-start" or "--" not in raw_args:
        return raw_args, []
    idx = raw_args.index("--")
    return raw_args[:idx], raw_args[idx + 1 :]


def main(argv: Sequence[str] | None = None) -> int:
    """Run the CLI. Returns a process exit code."""
    _ensure_utf8_streams()
    raw_args = list(argv) if argv is not None else sys.argv[1:]
    ralphus_args, claude_args = _split_passthrough(raw_args)
    parser = build_parser()
    args = parser.parse_args(ralphus_args)
    args.claude_args = claude_args
    func = getattr(args, "func", None)
    if func is None:
        parser.print_help()
        return 0
    subcommand = getattr(args, "command", func.__name__)
    print(f"ralphus [cli] {subcommand} {vars(args)}", file=sys.stderr)
    exit_code: int = func(args)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())

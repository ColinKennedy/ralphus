"""Command-line entry point for ralphus."""

from __future__ import annotations

import argparse
import contextlib
import os
import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

from ralphus import __version__
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
from ralphus.health import CORE, DEVELOPER, pydantic_ai_available, run_checks
from ralphus.tutor import TASK_TUTOR

__all__ = ["build_parser", "main"]

# Valid run states accepted by `ralphus clear --status` (mirrors the daemon's
# RunState). Kept here so the CLI can reject a bad filter before hitting the API.
_RUN_STATES = ("queued", "pending", "running", "done", "failed", "cancelled")


def build_parser() -> argparse.ArgumentParser:
    """Construct the top-level argument parser."""
    parser = argparse.ArgumentParser(
        prog="ralphus",
        description="Submit and manage autonomous agent tasks against the ralphus daemon.",
    )
    parser.add_argument("--version", action="version", version=f"ralphus {__version__}")
    parser.add_argument(
        "--daemon-url",
        default=os.environ.get("RALPHUS_DAEMON_URL", DEFAULT_DAEMON_URL),
        help=f"Base URL of the daemon API (default: {DEFAULT_DAEMON_URL}).",
    )
    parser.set_defaults(func=None)

    subparsers = parser.add_subparsers(dest="command", metavar="COMMAND")

    p_validate = subparsers.add_parser("validate", help="Validate a task TOML file.")
    p_validate.add_argument("file", type=Path, help="Path to the .toml file.")
    p_validate.set_defaults(func=_cmd_validate)

    p_submit = subparsers.add_parser("submit", help="Submit a task TOML file to the daemon.")
    p_submit.add_argument("file", type=Path, help="Path to the .toml file.")
    p_submit.add_argument("--label", help="Optional human label for the run.")
    p_submit.add_argument(
        "--hold",
        action="store_true",
        help="Stage the run as queued instead of scheduling it immediately.",
    )
    p_submit.set_defaults(func=_cmd_submit)

    p_author = subparsers.add_parser(
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
        "--agent", default="claude", help="Agent backend for the authoring model (claude|ollama)."
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

    p_status = subparsers.add_parser("status", help="Show run status from the daemon.")
    p_status.add_argument("run_id", nargs="?", help="A run id; omit to list all runs.")
    p_status.set_defaults(func=_cmd_status)

    p_clear = subparsers.add_parser("clear", help="Delete tasks and reviews from the daemon.")
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

    p_check = subparsers.add_parser("check", help="System and environment checks.")
    check_sub = p_check.add_subparsers(dest="check_command", metavar="SUBCOMMAND")
    p_health = check_sub.add_parser(
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

    p_configuration = subparsers.add_parser("configuration", help="Configuration inspection.")
    configuration_sub = p_configuration.add_subparsers(
        dest="configuration_command", metavar="SUBCOMMAND"
    )
    p_config_show = configuration_sub.add_parser(
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

    p_task = subparsers.add_parser("task", help="Task-authoring helpers.")
    task_sub = p_task.add_subparsers(dest="task_command", metavar="SUBCOMMAND")
    p_tutor = task_sub.add_parser(
        "show-tutor",
        help="Print the Task TOML schema reference and worked examples.",
    )
    p_tutor.set_defaults(func=_cmd_show_tutor)
    # `ralphus task` with no subcommand prints the task help.
    p_task.set_defaults(func=_help_printer(p_task))

    p_initialize = subparsers.add_parser(
        "initialize", help="One-time local setup helpers for a repository."
    )
    initialize_sub = p_initialize.add_subparsers(dest="initialize_command", metavar="SUBCOMMAND")
    p_init_git = initialize_sub.add_parser(
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

    return parser


def _help_printer(parser: argparse.ArgumentParser) -> Callable[[argparse.Namespace], int]:
    """A command that just prints `parser`'s help (used for bare group commands)."""

    def _run(_args: argparse.Namespace) -> int:
        parser.print_help()
        return 0

    return _run


_DAEMON_HINT = "(is the daemon running? start it with: ralphus-daemon serve)"


def _print_daemon_error(exc: DaemonError) -> None:
    print(f"error: {exc}\n{_DAEMON_HINT}", file=sys.stderr)


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


def _cmd_validate(args: argparse.Namespace) -> int:
    # Validation is pure and needs no server: prefer the offline core validator
    # in the ralphus-daemon binary, and only fall back to the daemon API if the
    # binary cannot be located.
    daemon_bin = _find_daemon_bin()
    if daemon_bin is not None:
        try:
            return subprocess.run([daemon_bin, "validate", str(args.file)], check=False).returncode
        except OSError:
            pass  # fall back to the API path below

    text = _read_file(args.file)
    if text is None:
        return 2
    with DaemonClient(args.daemon_url) as client:
        try:
            outcome = client.validate(text)
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


def _cmd_submit(args: argparse.Namespace) -> int:
    text = _read_file(args.file)
    if text is None:
        return 2
    with DaemonClient(args.daemon_url) as client:
        try:
            result = client.submit(text, hold=args.hold, label=args.label)
        except DaemonError as exc:
            _print_daemon_error(exc)
            return 1
    print(f"{result.get('run_id')} ({result.get('state')})")
    return 0


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
            _print_daemon_error(exc)
            return 1
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


def _cmd_status(args: argparse.Namespace) -> int:
    with DaemonClient(args.daemon_url) as client:
        try:
            if args.run_id:
                _print_run(client.run(args.run_id))
            else:
                board = client.tasks()
                runs = board.get("runs", [])
                if not runs:
                    print("no runs")
                for run in runs:
                    print(f"{run.get('id')}  {run.get('state'):<9}  {run.get('label') or ''}")
        except DaemonError as exc:
            _print_daemon_error(exc)
            return 2
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
            _print_daemon_error(exc)
            return 1
    print(
        f"cleared {result.get('runs_deleted', 0)} run(s), "
        f"{result.get('guardians_deleted', 0)} review(s); "
        f"{result.get('worktrees_purged', 0)} worktree(s) purged"
    )
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


def main(argv: Sequence[str] | None = None) -> int:
    """Run the CLI. Returns a process exit code."""
    parser = build_parser()
    args = parser.parse_args(argv)
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

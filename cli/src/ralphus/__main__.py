"""Command-line entry point for ralphus."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

from ralphus import __version__
from ralphus.client import DEFAULT_DAEMON_URL, DaemonClient, DaemonError
from ralphus.doctor import run_checks
from ralphus.tutor import TASK_TUTOR

__all__ = ["build_parser", "main"]


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

    p_status = subparsers.add_parser("status", help="Show run status from the daemon.")
    p_status.add_argument("run_id", nargs="?", help="A run id; omit to list all runs.")
    p_status.set_defaults(func=_cmd_status)

    doctor = subparsers.add_parser("doctor", help="Check the local ralphus setup.")
    doctor.set_defaults(func=_cmd_doctor)

    p_task = subparsers.add_parser("task", help="Task-authoring helpers.")
    task_sub = p_task.add_subparsers(dest="task_command", metavar="SUBCOMMAND")
    p_tutor = task_sub.add_parser(
        "show-tutor",
        help="Print the Task TOML schema reference and worked examples.",
    )
    p_tutor.set_defaults(func=_cmd_show_tutor)
    # `ralphus task` with no subcommand prints the task help.
    p_task.set_defaults(func=_help_printer(p_task))

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


def _print_run(run: dict[str, object]) -> None:
    print(f"{run.get('id')}  {run.get('state')}")
    tasks = run.get("tasks")
    if isinstance(tasks, list):
        for task in tasks:
            if isinstance(task, dict):
                print(f"  task {task.get('name')}: {task.get('state')}")


def _cmd_doctor(args: argparse.Namespace) -> int:
    symbols = {"pass": "OK  ", "warn": "WARN", "fail": "FAIL"}
    results = run_checks(args.daemon_url)
    for r in results:
        print(f"[{symbols.get(r.status, '?')}] {r.name}: {r.detail}")
    failed = [r for r in results if r.is_fail]
    if failed:
        print(f"\n{len(failed)} check(s) failed.")
        return 1
    return 0


def _cmd_show_tutor(_args: argparse.Namespace) -> int:
    print(TASK_TUTOR)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the CLI. Returns a process exit code."""
    parser = build_parser()
    args = parser.parse_args(argv)
    func = getattr(args, "func", None)
    if func is None:
        parser.print_help()
        return 0
    exit_code: int = func(args)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())

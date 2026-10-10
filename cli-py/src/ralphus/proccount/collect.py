"""``ralphus-proccount``: measure tagged tests under the nextest wrapper, store, and graph.

``collect`` runs the tagged tests ``--runs`` times (default 3) through the
``proc-count`` nextest profile, whose wrapper (``scripts/proc-count-wrap.sh``)
writes ``<test id>\\t<count>`` for a test only when it passed. A test is
recorded only if it passed in every run; its value is the lowest count seen.
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from pathlib import Path

from ralphus.proccount.graph import write_graphs
from ralphus.proccount.manifest import TaggedTest, load_manifest, nextest_filter
from ralphus.proccount.storage import CountRecord, load_records, save_records, upsert_record

__all__ = ["main", "read_run_dir", "reduce_runs"]

DATA_DIR_NAME = "proc_counts"
PROFILE = "proc-count"
WRAPPER_RELPATH = Path("scripts") / "proc-count-wrap.sh"
NEXTEST_CONFIG_RELPATH = Path(".config") / "nextest.toml"


def read_run_dir(directory: Path) -> dict[str, int]:
    """Parse the ``<test id>\\t<count>`` files the wrapper left for one run."""
    counts: dict[str, int] = {}
    for path in sorted(directory.glob("*.count")):
        test_id, _, raw = path.read_text(encoding="utf-8").strip().rpartition("\t")
        if test_id and raw.isdigit():
            counts[test_id] = int(raw)
    return counts


def reduce_runs(
    runs: Sequence[dict[str, int]], wanted: Sequence[str]
) -> tuple[dict[str, int], list[str]]:
    """Lowest count per test across ``runs``; tests missing from any run are dropped.

    A test is absent from a run when it failed (the wrapper only records passes),
    and a failing test must leave no data. Returns ``(counts, dropped test ids)``.
    """
    counts: dict[str, int] = {}
    dropped: list[str] = []
    for test_id in wanted:
        samples = [run[test_id] for run in runs if test_id in run]
        if runs and len(samples) == len(runs):
            counts[test_id] = min(samples)
        else:
            dropped.append(test_id)
    return counts, dropped


def _git(root: Path, *args: str) -> str:
    result = subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True)
    return result.stdout.strip()


def _repo_root() -> Path:
    return Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
        ).stdout.strip()
    )


def _nearest_tag(root: Path, ref: str) -> str | None:
    try:
        return _git(root, "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", ref)
    except subprocess.CalledProcessError:
        return None


def _run_once(build_root: Path, tests: list[TaggedTest], out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "RALPHUS_PROC_COUNT_DIR": str(out_dir)}
    # A failing test is expected to be possible; its absence from `out_dir` is the signal.
    subprocess.run(
        [
            "cargo",
            "nextest",
            "run",
            "--profile",
            PROFILE,
            "--workspace",
            "--all-targets",
            "--no-fail-fast",
            "--no-tests",
            "pass",
            "-E",
            nextest_filter(tests),
        ],
        cwd=build_root,
        env=env,
        check=False,
    )


def _collect(args: argparse.Namespace) -> int:
    root = _repo_root()
    data_dir = root / DATA_DIR_NAME
    tests = load_manifest(data_dir / "tags.toml")
    if not tests:
        sys.stderr.write("proc_counts/tags.toml lists no tests\n")
        return 1
    ref = args.ref or "HEAD"
    label = args.label or _nearest_tag(root, ref)
    if label is None:
        sys.stderr.write("no v* git tag reachable from the ref; pass --label\n")
        return 1
    commit = _git(root, "rev-parse", ref)

    worktree: Path | None = None
    build_root = root
    if args.ref:
        # Backfill: build the old ref in a throwaway worktree, but measure with the
        # current wrapper + nextest profile, which that old ref does not have.
        worktree = Path(tempfile.mkdtemp(prefix="proc-count-")) / "tree"
        _git(root, "worktree", "add", "--detach", str(worktree), commit)
        build_root = worktree
        for rel in (WRAPPER_RELPATH, NEXTEST_CONFIG_RELPATH):
            (worktree / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / rel, worktree / rel)
    try:
        scratch = Path(tempfile.mkdtemp(prefix="proc-count-runs-"))
        runs: list[dict[str, int]] = []
        for n in range(args.runs):
            run_dir = scratch / f"run{n}"
            _run_once(build_root, tests, run_dir)
            runs.append(read_run_dir(run_dir))
    finally:
        if worktree is not None:
            _git(root, "worktree", "remove", "--force", str(worktree))

    counts, dropped = reduce_runs(runs, [t.test_id for t in tests])
    for test_id in dropped:
        sys.stderr.write(f"no data written (failed or missing in a run): {test_id}\n")
    if not counts:
        sys.stderr.write("no tagged test produced data\n")
        return 1
    records_path = data_dir / "records.json"
    records = upsert_record(
        load_records(records_path),
        CountRecord(label=label, commit=commit, platform=platform.system().lower(), counts=counts),
    )
    save_records(records_path, records)
    write_graphs(data_dir, load_records(records_path), tests)
    sys.stdout.write(f"recorded {len(counts)} test(s) at {label} ({commit[:9]})\n")
    return 0


def _graph(_args: argparse.Namespace) -> int:
    data_dir = _repo_root() / DATA_DIR_NAME
    write_graphs(
        data_dir, load_records(data_dir / "records.json"), load_manifest(data_dir / "tags.toml")
    )
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="ralphus-proccount", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    collect = sub.add_parser("collect", help="measure tagged tests and update proc_counts/")
    collect.add_argument("--label", help="x-axis label (default: nearest v* tag)")
    collect.add_argument("--ref", help="backfill: measure this commit in a throwaway worktree")
    collect.add_argument("--runs", type=int, default=3, help="runs per test; lowest is kept")
    collect.set_defaults(func=_collect)
    graph = sub.add_parser("graph", help="re-render the graph from stored records")
    graph.set_defaults(func=_graph)
    args = parser.parse_args(argv)
    code: int = args.func(args)
    return code


if __name__ == "__main__":
    raise SystemExit(main())

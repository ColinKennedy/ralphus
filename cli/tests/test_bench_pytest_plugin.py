"""Integration tests for the RAL-94 pytest plugin (`--ralphus-bench` opt-in).

Uses the `pytester` fixture to run a small sample test module through a real
nested pytest session, exercising the plugin end to end: normal runs are
unaffected, `--ralphus-bench` runs are timed via the durable-minimum loop and
persisted, and the `ralphus_bench` mark never triggers an "unknown mark"
warning.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest


def _git_init(path: Path) -> None:
    # Idempotent: a no-op past the first call so this tolerates being invoked
    # more than once in-process against the same `pytester.path` by the
    # RAL-94 bench harness itself (a second `git commit` here would otherwise
    # fail with "nothing to commit").
    if (path / ".git").exists():
        return
    for args in (
        ["init", "--quiet"],
        ["config", "user.email", "test@example.com"],
        ["config", "user.name", "Test"],
    ):
        subprocess.run(["git", *args], cwd=path, check=True, capture_output=True)
    (path / "seed.txt").write_text("seed", encoding="utf-8")
    subprocess.run(["git", "add", "."], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "commit", "--quiet", "-m", "seed"], cwd=path, check=True, capture_output=True
    )


SAMPLE_TEST_FILE = """
import pytest

def test_default_patience():
    pass

# low: intentionally overrides the default to verify explicit patience is honored and persisted
@pytest.mark.ralphus_bench(patience=2)
def test_explicit_patience():
    pass
"""


def test_normal_run_does_not_bench_or_warn(pytester: pytest.Pytester) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SAMPLE_TEST_FILE)

    result = pytester.runpytest()
    result.assert_outcomes(passed=2)
    assert "PytestUnknownMarkWarning" not in "\n".join(result.outlines)
    assert not (pytester.path / "bench_data").exists()


def test_ralphus_bench_flag_runs_durable_min_and_persists(pytester: pytest.Pytester) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SAMPLE_TEST_FILE)

    result = pytester.runpytest("--ralphus-bench")
    result.assert_outcomes(passed=2)

    data_dir = pytester.path / "bench_data" / "python" / "test_module.py"
    default_file = data_dir / "test_default_patience.json"
    explicit_file = data_dir / "test_explicit_patience.json"
    assert default_file.exists()
    assert explicit_file.exists()

    record_file = json.loads(default_file.read_text(encoding="utf-8"))
    assert record_file["test_id"] == "test_module.py::test_default_patience"
    record = record_file["records"][0]
    assert record["dirty"] is True  # untracked test_module.py at benchmark time
    assert len(record["commit"]) == 40
    assert len(record["samples"]) >= 2  # at least one improving + one non-improving run
    assert record["durable_min"] == min(record["samples"])
    for key in ("max", "mean", "median", "stddev", "iqr", "outliers"):
        assert key in record


@pytest.mark.no_bench  # asserts an exact accumulated count in `pytester.path`'s
# bench_data, which a fixed pytester sandbox can't tolerate being re-run
# against by the harness itself
def test_second_run_appends_a_second_record(pytester: pytest.Pytester) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SAMPLE_TEST_FILE)

    pytester.runpytest("--ralphus-bench").assert_outcomes(passed=2)
    pytester.runpytest("--ralphus-bench").assert_outcomes(passed=2)

    data_dir = pytester.path / "bench_data" / "python" / "test_module.py"
    record_file = json.loads((data_dir / "test_default_patience.json").read_text(encoding="utf-8"))
    assert len(record_file["records"]) == 2


SKIP_TEST_FILE = """
import pytest

def test_runs_normally():
    pass

@pytest.mark.skip(reason="marker-based skip")
def test_marker_skipped():
    pass

def test_dynamic_skipped():
    pytest.skip("dynamic skip inside the test body")
"""


@pytest.mark.no_bench  # asserts an exact accumulated count in `pytester.path`'s
# bench_data, which a fixed pytester sandbox can't tolerate being re-run
# against by the harness itself
def test_marker_skipped_test_recorded_as_skipped_not_timed(pytester: pytest.Pytester) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SKIP_TEST_FILE)

    result = pytester.runpytest("--ralphus-bench")
    result.assert_outcomes(passed=1, skipped=2)

    data_dir = pytester.path / "bench_data" / "python" / "test_module.py"
    skipped_file = json.loads((data_dir / "test_marker_skipped.json").read_text(encoding="utf-8"))
    assert skipped_file["records"] == []
    assert len(skipped_file["skipped"]) == 1
    assert len(skipped_file["skipped"][0]["commit"]) == 40


@pytest.mark.no_bench  # asserts an exact accumulated count in `pytester.path`'s
# bench_data, which a fixed pytester sandbox can't tolerate being re-run
# against by the harness itself
def test_dynamic_skipped_test_recorded_as_skipped_not_timed(pytester: pytest.Pytester) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SKIP_TEST_FILE)

    result = pytester.runpytest("--ralphus-bench")
    result.assert_outcomes(passed=1, skipped=2)

    data_dir = pytester.path / "bench_data" / "python" / "test_module.py"
    skipped_file = json.loads((data_dir / "test_dynamic_skipped.json").read_text(encoding="utf-8"))
    assert skipped_file["records"] == []
    assert len(skipped_file["skipped"]) == 1


@pytest.mark.no_bench  # asserts an exact accumulated count in `pytester.path`'s
# bench_data, which a fixed pytester sandbox can't tolerate being re-run
# against by the harness itself
def test_skipped_tests_do_not_write_a_timed_record_file_for_passing_test(
    pytester: pytest.Pytester,
) -> None:
    _git_init(pytester.path)
    pytester.makepyfile(test_module=SKIP_TEST_FILE)

    pytester.runpytest("--ralphus-bench").assert_outcomes(passed=1, skipped=2)

    data_dir = pytester.path / "bench_data" / "python" / "test_module.py"
    normal_file = json.loads((data_dir / "test_runs_normally.json").read_text(encoding="utf-8"))
    assert len(normal_file["records"]) == 1
    assert normal_file["skipped"] == []

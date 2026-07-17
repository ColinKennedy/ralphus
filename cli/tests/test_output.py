"""Tests for the --json/table/exit-code output helpers shared by every subcommand."""

from __future__ import annotations

import json

import pytest

from ralphus.client import DaemonError
from ralphus.output import emit, exit_code_for, print_kv, print_table


def test_emit_json_mode_prints_raw_json(capsys: pytest.CaptureFixture[str]) -> None:
    emit(True, {"a": 1}, lambda _d: (_ for _ in ()).throw(AssertionError("human called")))
    captured = capsys.readouterr()
    assert json.loads(captured.out) == {"a": 1}


def test_emit_human_mode_calls_human(capsys: pytest.CaptureFixture[str]) -> None:
    emit(False, {"a": 1}, lambda d: print(f"got {d['a']}"))
    captured = capsys.readouterr()
    assert captured.out.strip() == "got 1"


def test_print_table_aligns_columns(capsys: pytest.CaptureFixture[str]) -> None:
    print_table(["id", "state"], [["run-1", "done"], ["run-22", "running"]])
    captured = capsys.readouterr()
    lines = captured.out.splitlines()
    assert lines[0].startswith("id")
    assert lines[1].startswith("run-1 ")
    assert lines[2].startswith("run-22")


def test_print_table_empty_rows_prints_headers_only(capsys: pytest.CaptureFixture[str]) -> None:
    print_table(["id", "state"], [])
    captured = capsys.readouterr()
    assert captured.out.strip() == "id  state"


def test_print_kv_aligns_colons(capsys: pytest.CaptureFixture[str]) -> None:
    print_kv([("id", "run-1"), ("state", "done")])
    captured = capsys.readouterr()
    lines = captured.out.splitlines()
    assert lines[0] == "id    : run-1"
    assert lines[1] == "state : done"


def test_exit_code_for_not_found() -> None:
    assert exit_code_for(DaemonError("x", status_code=404)) == 3


def test_exit_code_for_conflict() -> None:
    assert exit_code_for(DaemonError("x", status_code=409)) == 4


def test_exit_code_for_other_status_is_domain_error() -> None:
    assert exit_code_for(DaemonError("x", status_code=400)) == 1


def test_exit_code_for_unreachable_is_domain_error() -> None:
    assert exit_code_for(DaemonError("x", status_code=None)) == 1

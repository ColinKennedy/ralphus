"""Tests for `ralphus.hostos` -- the one place ralphus asks which OS it's on."""

from __future__ import annotations

import os

from ralphus.hostos import is_windows


def test_is_windows_agrees_with_the_interpreter() -> None:
    """Guards the magic string: a typo in `_WINDOWS_OS_NAME` would silently
    make every OS branch in the codebase take its POSIX path on Windows."""
    assert is_windows() is (os.name == "nt")


def test_is_windows_is_a_call_so_the_os_stays_stubbable() -> None:
    """It is deliberately a function, not a module-level constant: a constant
    is bound at import time in every importer, which would make the other
    branch unreachable from a test."""
    assert callable(is_windows)
    assert isinstance(is_windows(), bool)

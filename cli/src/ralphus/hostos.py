"""Which OS ralphus is running on, asked once and named.

`os.name == "nt"` is the idiom Python hands you, and it is a bad one to sprinkle
through a codebase: it names an implementation detail (the *interpreter's*
notion of its OS API family) rather than the thing the caller actually cares
about, it reads as a comparison against a magic two-letter string, and every
site that writes it has to be re-read to work out whether it meant "Windows",
"not POSIX", or "no `/proc`". `is_windows()` says what it means.

Use `is_windows()` -- not `os.name`, not `sys.platform`, not
`platform.system()` -- anywhere ralphus branches on the host OS.

Deliberately a function rather than a module-level constant: a constant is
bound at import time in every module that does `from ralphus.hostos import
IS_WINDOWS`, which makes the OS unstubbable in a test that wants to exercise
the other branch. A call goes through this module every time.
"""

from __future__ import annotations

import os

__all__ = ["is_windows"]

#: What `os.name` reports on Windows, on every Python implementation that runs
#: there. The one place this string appears.
_WINDOWS_OS_NAME = "nt"


def is_windows() -> bool:
    """Whether this process is running on Windows.

    True under native CPython on Windows, including when launched from an
    MSYS/Git-Bash shell -- the shell is POSIX-flavored but the interpreter is
    not, which is exactly the distinction that matters for paths, `PATHEXT`
    lookup, and process spawning (see the `cwd` gotcha in AGENTS.md).
    """
    return os.name == _WINDOWS_OS_NAME

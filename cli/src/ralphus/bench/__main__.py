"""Console entry point for the RAL-94 Python benchmark harness (`ralphus-bench-py`).

Always passes `--ralphus-bench` so the harness activates; extra arguments are
forwarded to pytest unchanged (e.g. a path filter or `-k` expression), letting
callers benchmark a subset instead of the whole suite.
"""

from __future__ import annotations

import sys

import pytest

__all__ = ["main"]


def main() -> None:
    sys.exit(pytest.main(["--ralphus-bench", *sys.argv[1:]]))


if __name__ == "__main__":
    main()

"""Standalone entry point for packaging the ralphus CLI (PyInstaller/Nuitka)."""

from __future__ import annotations

import sys

from ralphus.__main__ import main

if __name__ == "__main__":
    sys.exit(main())

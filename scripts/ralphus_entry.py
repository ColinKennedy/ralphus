"""Standalone entry point for packaging the ralphus CLI (PyInstaller/Nuitka)."""

from __future__ import annotations

import os
import sys

# Frozen builds ship no .py source, but pydantic's plugin loader (loaded via
# pydantic-ai) auto-instruments validators through logfire's integration,
# which calls `inspect.getsource()` on them and crashes with "could not get
# source code" outside a real source tree. `ralphus author` doesn't need
# logfire instrumentation, so disable pydantic plugins entirely before
# anything gets a chance to import pydantic-ai.
os.environ.setdefault("PYDANTIC_DISABLE_PLUGINS", "1")

from ralphus.__main__ import main

if __name__ == "__main__":
    sys.exit(main())

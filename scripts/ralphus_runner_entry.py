"""Standalone entry point for packaging ralphus-runner (PyInstaller/Nuitka).

The daemon spawns this program once per prompt/command session, feeding it a
SessionSpec (JSON) and reading back a SessionResult (JSON). Bundling it as an
exe lets a packaged daemon run native model agents without a Python install.
"""

from __future__ import annotations

import os
import sys

# See ralphus_entry.py: frozen builds crash under logfire's pydantic
# instrumentation (it calls `inspect.getsource()`, which has no source to
# read outside a real source tree), so disable pydantic plugins up front.
os.environ.setdefault("PYDANTIC_DISABLE_PLUGINS", "1")

from ralphus.runner.__main__ import main

if __name__ == "__main__":
    sys.exit(main())

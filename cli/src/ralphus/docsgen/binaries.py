"""Locate ralphus's compiled Rust binaries for docs generation.

Both `helpmap_docs.py` (shells out to `ralphus show help-map`) and
`librarian_server.py` (spawns the real `ralphus-librarian` for screenshot
generation) need the same "find the compiled exe" logic — checked in order:
an explicit env var override, the release build, the debug build, then PATH.
Release wins over debug when both exist since it's the more likely "what a
doc build actually wants".
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path

__all__ = ["REPO_ROOT", "find_binary", "find_librarian_binary", "find_ralphus_binary"]

REPO_ROOT = Path(__file__).resolve().parents[4]


def find_binary(name: str, env_var: str) -> Path:
    """Locate a compiled `name` executable (e.g. `ralphus`, `ralphus-librarian`).

    Checked in order: `$<env_var>` (explicit override), `dist/`, the release
    build, the debug build, then PATH.
    """
    override = os.environ.get(env_var)
    if override:
        path = Path(override)
        if path.is_file():
            return path
        raise RuntimeError(f"{env_var}={override!r} does not exist")

    for candidate in (
        REPO_ROOT / "dist" / f"{name}.exe",
        REPO_ROOT / "dist" / name,
        REPO_ROOT / "target" / "release" / f"{name}.exe",
        REPO_ROOT / "target" / "release" / name,
        REPO_ROOT / "target" / "debug" / f"{name}.exe",
        REPO_ROOT / "target" / "debug" / name,
    ):
        if candidate.is_file():
            return candidate

    on_path = shutil.which(name)
    if on_path:
        return Path(on_path)

    raise RuntimeError(
        f"could not find the {name} binary -- build it first (cargo build -p {name}, "
        f"or --release), or set {env_var} to its full path"
    )


def find_ralphus_binary() -> Path:
    """Locate the compiled `ralphus` CLI executable."""
    return find_binary("ralphus", "RALPHUS_CLI_BIN")


def find_librarian_binary() -> Path:
    """Locate the compiled `ralphus-librarian` executable."""
    return find_binary("ralphus-librarian", "RALPHUS_LIBRARIAN_BIN")

"""Spawns the real, compiled `ralphus-librarian` binary for docs screenshot generation.

Launches the actual `ralphus-librarian` executable pointed at a stub daemon
URL (typically `stub_server.fixture_server`'s canned `/api/*` JSON), so
Playwright screenshots exactly the bytes/headers production serves, with the
librarian's own real proxying behavior for any `/api/*` request board.html
happens to make.
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from contextlib import contextmanager

from ralphus.docsgen.binaries import find_librarian_binary

__all__ = ["librarian_server"]


def _free_port() -> int:
    """An ephemeral port free right now (small bind/close race, acceptable for a local tool)."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        port: int = s.getsockname()[1]
        return port


def _wait_until_ready(url: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=1) as resp:
                if resp.status == 200:
                    return
        except (OSError, urllib.error.URLError) as e:
            last_error = e
        time.sleep(0.05)
    raise RuntimeError(f"ralphus-librarian never became ready at {url}") from last_error


@contextmanager
def librarian_server(daemon_url: str, *, ready_timeout: float = 10.0) -> Iterator[str]:
    """Spawn the real `ralphus-librarian` binary, proxying `/api/*` to `daemon_url`.

    Yields the librarian's own base URL (e.g. ``http://127.0.0.1:<port>``) —
    the real board.html bytes, served by the real binary. Torn down on
    context exit, including on exception.
    """
    binary = find_librarian_binary()
    port = _free_port()
    env = {**os.environ, "RALPHUS_DAEMON_URL": daemon_url}
    # stdout/stderr go to DEVNULL rather than PIPE: the librarian never exits
    # on its own (it serves forever until killed below), so nothing ever
    # drains a pipe here -- letting it fill would eventually deadlock the
    # child on a blocked write. If startup fails, `_wait_until_ready`'s own
    # RuntimeError is the actionable signal; rerun without DEVNULL to watch
    # the binary's own stderr for details.
    proc = subprocess.Popen(
        [str(binary), "serve", "--port", str(port)],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    base_url = f"http://127.0.0.1:{port}"
    try:
        _wait_until_ready(base_url, ready_timeout)
        yield base_url
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

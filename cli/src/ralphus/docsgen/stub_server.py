"""A tiny, fully deterministic HTTP server for docs screenshot generation.

Serves ``librarian/assets/board.html`` verbatim at ``/`` and canned JSON at a
handful of ``/api/*`` routes — no daemon, no SQLite, no scheduler involved.
board.html never needs a real backend for the scenarios docsgen captures:
every interaction it screenshots (selecting a run/session via URL hash,
dragging a queue row, opening the manual-checks dropdown) is client-side only
until a mutating POST fires, which these scenarios never trigger.
"""

from __future__ import annotations

import json
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

__all__ = ["BOARD_HTML", "REPO_ROOT", "Routes", "fixture_server"]

Routes = dict[str, Any]  # route -> JSON body (dict or list)

REPO_ROOT = Path(__file__).resolve().parents[4]
BOARD_HTML = REPO_ROOT / "librarian" / "assets" / "board.html"


class _Handler(BaseHTTPRequestHandler):
    server: _FixtureHTTPServer

    def do_GET(self) -> None:
        if self.path == "/":
            self._send(200, self.server.board_html, "text/html; charset=utf-8")
            return
        if self.path in self.server.routes:
            body = json.dumps(self.server.routes[self.path]).encode("utf-8")
            self._send(200, body, "application/json")
            return
        self._send(404, b"{}", "application/json")

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _FixtureHTTPServer(ThreadingHTTPServer):
    def __init__(self, routes: Routes, board_html: bytes) -> None:
        super().__init__(("127.0.0.1", 0), _Handler)
        self.routes = routes
        self.board_html = board_html


@contextmanager
def fixture_server(routes: Routes) -> Iterator[str]:
    """Start a stub API server bound to ``routes`` on an ephemeral port.

    Yields the server's base URL (``http://127.0.0.1:<port>``). Torn down on
    context exit, including on exception.
    """
    server = _FixtureHTTPServer(routes, BOARD_HTML.read_bytes())
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

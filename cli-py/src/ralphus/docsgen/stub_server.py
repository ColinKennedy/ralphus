"""A tiny, fully deterministic API stub for docs screenshot generation.

Serves canned JSON at a handful of ``/api/*`` routes — no daemon, no SQLite,
no scheduler involved. The real ``librarian/assets/board.html`` page is
served by the real, compiled ``ralphus-librarian`` binary (see
``librarian_server.py``), which proxies its own `/api/*` requests here.
board.html never needs a real backend for the scenarios docsgen captures:
every *scenario-driven* interaction (selecting a squad/cell via URL hash,
dragging a queue row, opening the manual-checks dropdown) is client-side
only. board.html does, on its own, repeatedly ``POST /api/events/ticket`` to
mint an SSE reconnect ticket (RAL-222) regardless of what the scenario is
doing — this stub answers it with a 404 like any other unhandled route,
which is enough: board.html treats a non-2xx ticket response as "try again
later" and never depends on the resulting live event stream for anything a
screenshot captures.
"""

from __future__ import annotations

import json
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

__all__ = ["Routes", "fixture_server"]

Routes = dict[str, Any]  # route -> JSON body (dict or list)


class _Handler(BaseHTTPRequestHandler):
    server: _FixtureHTTPServer

    def do_GET(self) -> None:
        if self.path in self.server.routes:
            body = json.dumps(self.server.routes[self.path]).encode("utf-8")
            self._send(200, body, "application/json")
            return
        self._send(404, b"{}", "application/json")

    def do_POST(self) -> None:
        # No scenario needs a real mutation response — board.html's own
        # unconditional `/api/events/ticket` reconnect loop is the only POST
        # ever seen here (see module docstring), and it treats any non-2xx
        # reply the same. Answering with a plain 404 (instead of falling
        # through to BaseHTTPRequestHandler's default 501 "Unsupported
        # method") avoids spurious "Unsupported method" log spam.
        self._send(404, b"{}", "application/json")

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _FixtureHTTPServer(ThreadingHTTPServer):
    def __init__(self, routes: Routes) -> None:
        super().__init__(("127.0.0.1", 0), _Handler)
        self.routes = routes


@contextmanager
def fixture_server(routes: Routes) -> Iterator[str]:
    """Start a stub API server bound to ``routes`` on an ephemeral port.

    Yields the server's base URL (``http://127.0.0.1:<port>``) — hand it to
    ``librarian_server.librarian_server`` as its ``daemon_url``. Torn down on
    context exit, including on exception.
    """
    server = _FixtureHTTPServer(routes)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

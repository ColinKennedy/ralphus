"""Lint: every daemon HTTP endpoint has a CLI command or an inline exclusion
(RAL-410).

Structural parity check between `daemon/src/server.rs`'s route match and the
CLI leaf surface in `cli/src/help_map.rs`, mirroring the CLI<->MCP parity test
in `mcp/tests/parity.rs` (RAL-301) but for endpoint<->CLI coverage. Stdlib-only
so it runs directly from the repo root in CI without `uv sync`; unit-testable
through the functions below (see `test_check_endpoint_cli_parity.py`).

The check is intentionally *static*: it walks `server.rs`'s
`route_for_user` match block and help_map.rs's `node(...)` tree with a small
string-aware scanner (no cargo build). It verifies, bidirectionally:

- every daemon endpoint either maps to a CLI leaf in `ENDPOINT_TO_CLI` below
  or carries an inline `ralphus[ignore-endpoint-cli]: <reason>` comment above
  its route arm in `server.rs`;
- every CLI leaf either appears as a mapping value or carries the same inline
  marker above its `node(...)` call in `help_map.rs`;
- every mapping key names a real endpoint and every mapping value names a real
  leaf (a renamed/removed route or command breaks the manifest, not silently);
- every inline exclusion names a real endpoint/leaf and carries a substantive
  reason, mirroring `mcp/exclusions.rs`'s documented-reason rule.

`GET /api/events` is routed by `serve()` directly (it bypasses `route()` to
hand the SSE stream its own thread), so the route-arm walk cannot see it; it
is declared in `SPECIAL_ROUTES` instead so the check still holds every real
endpoint accountable.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SERVER_RS = REPO_ROOT / "daemon" / "src" / "server.rs"
HELP_MAP_RS = REPO_ROOT / "cli" / "src" / "help_map.rs"

METHODS = ("GET", "POST", "PUT", "DELETE", "PATCH")

# Endpoints dispatched outside the `route_for_user` match block. Each entry is
# its own built-in, self-documenting exclusion: the value is the substantive
# reason it has no CLI command (mirroring the marker comments used for regular
# route arms -- but there is no route arm for a comment to sit adjacent to).
SPECIAL_ROUTES: dict[str, str] = {
    "GET /api/events": "SSE push stream requires a live, blocked connection \
stream (matches EVENTS_PATH in serve(), bypassing route() so the connection gets \
its own thread, RAL-167/RAL-222); the CLI has no SSE consumer -- the \
librarian's board is its only client.",
}

# One endpoint -> the CLI leaf paths that are its clients, verbatim-spelled
# (`" ".join` of each leaf path). Every real endpoint and every real leaf must
# be accounted for by this table or by an exclusion (an inline
# `// ralphus[ignore-endpoint-cli]: <reason>` marker above the route arm in
# `server.rs` / the `node(...)` call in `help_map.rs`, or a `SPECIAL_ROUTES`
# entry); entries are verified against both sides, so a renamed
# endpoint/command breaks the check instead of silently going stale.
ENDPOINT_TO_CLI: dict[str, list[str]] = {
    # ---- daemon / board ---------------------------------------------------
    "GET /api/daemon": ["check health"],
    "GET /api/tasks": ["status", "squad list"],
    "GET /api/projects": ["project list", "check health"],
    "POST /api/projects": ["project git"],
    "GET /api/projects/{name}": ["project get"],
    "DELETE /api/projects/{name}": ["project remove"],
    "GET /api/projects/{name}/review-settings": ["project review-settings get"],
    "POST /api/projects/{name}/review-settings": ["project review-settings set"],
    "GET /api/project-forks": ["project fork list"],
    "GET /api/projects/{name}/forks": ["project fork list"],
    "POST /api/projects/{name}/forks": ["project fork add"],
    "PATCH /api/projects/{name}/forks": ["project fork set"],
    "PATCH /api/projects/{name}/forks/{user}": ["project fork set"],
    "DELETE /api/projects/{name}/forks": ["project fork remove"],
    "DELETE /api/projects/{name}/forks/{user}": ["project fork remove"],
    "GET /api/users/{user}/forge-tokens": ["user list-forge-tokens"],
    "POST /api/users/{user}/forge-tokens": ["user set-forge-token"],
    "DELETE /api/users/{user}/forge-tokens/{host}": ["user delete-forge-token"],
    "GET /api/internal/fork-credential": ["internal fork-credential-helper"],
    "GET /api/worktree-retirements": ["review worktree-retirements"],
    "GET /api/machines": ["machine list", "check health"],
    "POST /api/machines": ["machine register"],
    "GET /api/machines/{scheme}": ["machine get"],
    "DELETE /api/machines/{scheme}": ["machine remove"],
    "POST /api/machines/cleanup": ["machine cleanup"],
    "GET /api/machines/targets/health": ["check health"],
    "GET /api/triage/types": ["triage type list"],
    "POST /api/triage/types": ["triage type register"],
    "GET /api/triage/types/{name}": ["triage type get"],
    "DELETE /api/triage/types/{name}": ["triage type deregister"],
    "GET /api/triage/pools": ["triage pool list"],
    "POST /api/triage/pools/threshold": ["triage pool threshold"],
    "GET /api/resources": ["resources"],
    "GET /api/health/agent-profiles": ["check health"],
    "GET /api/agent-profiles": ["agent profile list"],
    "GET /api/agent-profiles/{name}": ["agent profile get"],
    "POST /api/agent-profiles": ["agent profile create"],
    "PATCH /api/agent-profiles/{name}": ["agent profile update"],
    "DELETE /api/agent-profiles/{name}": ["agent profile delete"],
    "GET /api/agent-backend-commands": ["agent backend-command list"],
    "POST /api/agent-backend-commands/{backend}": ["agent backend-command set"],
    "DELETE /api/agent-backend-commands/{backend}": ["agent backend-command reset"],
    "GET /api/agent-backend-commands/{backend}/profiles": ["agent backend-command profiles"],
    "GET /api/health/project-forks": ["check health"],
    "POST /api/health/arbiter": ["check health"],
    "GET /api/health/catalog": ["check catalog"],
    "GET /api/cartographer": ["cartographer"],
    "POST /api/mailbox/register": ["mailbox check"],
    "POST /api/mailbox/personal/drain": ["mailbox personal-drain"],
    "POST /api/mailbox/personal/undrain": ["mailbox personal-undrain"],
    "GET /api/mailbox/personal/messages": ["mailbox personal"],
    "GET /api/watches": ["mailbox watches"],
    "POST /api/watches": ["mailbox watch"],
    "DELETE /api/watches/{entity_uri}": ["mailbox unwatch"],

    "GET /api/mailbox/{client_id}/messages": ["mailbox check"],
    "POST /api/mailbox/{client_id}/drain": ["mailbox check"],
    "POST /api/mailbox/{client_id}/undrain": ["mailbox undrain"],
    "GET /api/users/{name}/preferences": ["mailbox preferences"],
    "POST /api/users/{name}/preferences": ["mailbox set-preferences"],
    # ---- squads ------------------------------------------------------------
    "POST /api/squads/validate": ["validate"],
    "POST /api/squads": ["submit"],
    "POST /api/clear": ["clear"],
    "GET /api/queue": ["queue list"],
    "POST /api/queue/reorder": ["queue reorder"],
    "POST /api/queue/set-position": ["queue set-position"],
    "GET /api/graph": ["graph"],
    "GET /api/squads/{id}": [
        "squad show",
        "cell show",
        "cell reviews",
        "cell terminal",
        "task show",
        "proof show",
        "get",
        "listen",
        "status",
    ],
    "GET /api/squads/{id}/worktrees": ["cell worktree"],
    "GET /api/squads/{id}/logs": ["squad logs"],
    "GET /api/squads/{id}/timeline": ["squad timeline"],
    "GET /api/squads/{id}/graph": ["graph"],
    "POST /api/squads/{id}/activate": ["squad activate"],
    "POST /api/squads/{id}/cancel": ["squad cancel"],
    "POST /api/squads/{id}/set-status": [
        "squad set-status",
        "cell set-status",
        "task set-status",
        "proof set-status",
        "queue set-status",
    ],
    "POST /api/squads/{id}/edit": ["squad edit", "squad rename", "cell edit", "task edit", "proof edit"],
    "POST /api/squads/{id}/retry": ["squad retry", "retry"],
    "POST /api/squads/{id}/restart": ["squad restart"],
    "POST /api/squads/{id}/cells/{ti}/{si}/restart": ["cell restart"],
    "POST /api/squads/{id}/cells/{ti}/{si}/proof/{vi}/restart": ["cell restart-proof", "proof restart"],
    "POST /api/squads/{id}/tasks/{ti}/proof/{vi}/restart": ["task restart-proof", "proof restart"],
    # env: read-only resolved views (RAL-324); the POST setters are board-only
    "GET /api/squads/{id}/env": ["squad env"],
    "GET /api/squads/{id}/tasks/{ti}/env": ["task env"],
    "GET /api/squads/{id}/tasks/{ti}/proof/env": ["task env"],
    "GET /api/squads/{id}/tasks/{ti}/proof/{vi}/env": ["proof env"],
    "GET /api/squads/{id}/cells/{ti}/{si}/env": ["cell env"],
    "GET /api/squads/{id}/cells/{ti}/{si}/proof/env": ["cell env"],
    "GET /api/squads/{id}/cells/{ti}/{si}/proof/{vi}/env": ["proof env"],
    "POST /api/squads/{id}/cells/{ti}/{si}/open-terminal": ["cell open-agent"],
    "POST /api/squads/{id}/cells/{ti}/{si}/terminal-ticket": ["cell remote-terminal"],
    "POST /api/squads/{id}/cells/{ti}/{si}/resume-automation": ["cell resume-automation"],
    "GET /api/squads/{id}/cells/{ti}/{si}/pane": ["history"],
    "GET /api/squads/{id}/cells/{ti}/{si}/debug-events": ["history"],
    "GET /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/pane": ["history"],
    "GET /api/squads/{id}/proofs/{task_idx}/{scope}/{cell_idx}/{proof_idx}/debug-events": ["history"],
    "DELETE /api/squads/{id}": ["squad delete"],
    # ---- guardians (reviews) -----------------------------------------------
    "GET /api/guardians": ["review list"],
    "POST /api/guardians": ["review create"],
    "GET /api/guardians/{id}": [
        "review show",
        "review status",
        "review worktrees",
        "review branch terminal",
        "review checks list",
        "review checks run",
        "review checks terminal",
        "review action list",
        "review action run",
        "get",
        "listen",
    ],
    "GET /api/guardians/{id}/logs": ["review logs"],
    "POST /api/guardians/{id}/rename": ["review rename"],
    "POST /api/guardians/{id}/settings": ["review settings"],
    "POST /api/guardians/{id}/squash": ["review squash"],
    "DELETE /api/guardians/{id}": ["review delete"],
    "POST /api/guardians/{id}/branches": ["review add-branch"],
    "POST /api/guardians/{id}/branches/arrange": [
        "review reorder",
        "review branch enable",
        "review branch disable",
    ],
    "POST /api/guardians/{id}/sync-pr": ["review sync-pr"],
    "POST /api/guardians/{id}/branches/{branch_id}/feedback": ["review feedback"],
    "GET /api/guardians/{id}/base-branches": ["review upstream list"],
    "POST /api/guardians/{id}/base": ["review upstream set"],
    "POST /api/guardians/{id}/force_start": ["review force-start"],
    "POST /api/guardians/{id}/branches/{branch_id}/dismiss_reenable": ["review dismiss-reenable"],
    "POST /api/guardians/{id}/branches/{branch_id}/move": ["review move-branch"],
    "POST /api/guardians/{id}/branches/{branch_id}/link_cell": ["review link-cell"],
    "POST /api/guardians/{id}/branches/{branch_id}/env": ["review env"],
    "POST /api/guardians/{id}/build-env": ["review build-env"],
    "GET /api/guardians/{id}/branches/{branch_id}/env": ["review env"],
    "GET /api/guardians/{id}/build-env": ["review env"],
    "GET /api/guardians/{id}/tests-env": ["review env"],
    "GET /api/guardians/{id}/manual-checks-env": ["review env"],
    "POST /api/guardians/{id}/manual-checks-env": ["review manual-checks-env"],
    "POST /api/guardians/{id}/merge": ["review merge"],
    "POST /api/guardians/{id}/stop": ["review stop-merge"],
    "POST /api/guardians/{id}/cancel_and_merge": ["review restart-merge"],
    "POST /api/guardians/{id}/approve": ["review approve"],
    "POST /api/guardians/{id}/cancel": ["review cancel"],
    "POST /api/guardians/{id}/reopen": ["review reopen"],
    "POST /api/guardians/{id}/pull-requests": ["review pr submit"],
    "GET /api/guardians/{id}/pull-requests": ["review pr list"],
    "POST /api/guardians/{id}/pull-requests/unlink": ["review pr unlink"],
    "GET /api/pull-requests": ["review pr find"],
    "GET /api/pull-requests/{pr_id}": ["review pr show"],
    "POST /api/pull-requests/{pr_id}": ["review pr update"],
    "GET /api/pull-requests/{pr_id}/comments": ["review pr comments"],
    "POST /api/pull-requests/{pr_id}/action-feedback": ["review pr pull-feedback"],
    "POST /api/pull-requests/{pr_id}/pull-from-pr": ["review pr pull-from-pr"],
}

# Minimum substance for an exclusion reason (mirrors
# `mcp/tests/parity.rs::every_exclusion_has_a_non_empty_substantive_reason`
# and the other stdlib parity checks' `substantive()`).
MIN_REASON_LEN = 15
MIN_REASON_WORDS = 3


def substantive(reason: str) -> bool:
    """Whether an exclusion reason actually explains the judgment call."""
    text = reason.strip()
    return len(text) >= MIN_REASON_LEN and len(re.findall(r"[A-Za-z0-9]+", text)) >= MIN_REASON_WORDS


IGNORE_MARKER = "ralphus[ignore-endpoint-cli]:"


# ---------------------------------------------------------------------------
# low-level scanning helpers (string- and comment-aware)
# ---------------------------------------------------------------------------

def split_top_level(text: str) -> list[str]:
    """Split on top-level commas, keeping quoted strings and brackets intact."""
    parts: list[str] = []
    depth = 0
    cur: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            cur.append(text[i : j + 1])
            i = j + 1
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
            i += 1
            continue
        cur.append(ch)
        i += 1
    parts.append("".join(cur))
    return parts


def strip_comments(text: str) -> str:
    """Remove `//` and `/* */` comments from Rust source text, keeping string
    literals intact (a `//` inside a string is data, not a comment)."""
    out = []
    in_str = False
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if in_str:
            out.append(ch)
            if ch == "\\" and i + 1 < n:
                out.append(text[i + 1])
                i += 2
                continue
            if ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            out.append(ch)
            i += 1
            continue
        if ch == "/" and text[i + 1 : i + 2] == "/":
            j = text.find("\n", i)
            i = j if j >= 0 else n
            continue
        if ch == "/" and text[i + 1 : i + 2] == "*":
            j = text.find("*/", i + 2)
            i = j + 2 if j >= 0 else n
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def marker_reason(comments: str) -> str | None:
    """The reason text from an exclusion marker in a comment block, if any."""
    for line in comments.splitlines():
        if IGNORE_MARKER in line:
            return line.split(IGNORE_MARKER, 1)[1].strip()
    return None


def attach_comments(lines: list[str], line_index: int) -> str:
    """The contiguous `//` comment block directly above `line_index` (0-based).
    Stops at the first non-comment line (blank lines break the block)."""
    collected: list[str] = []
    i = line_index - 1
    while i >= 0:
        stripped = lines[i].lstrip()
        if stripped.startswith("//"):
            collected.append(stripped[2:])
            i -= 1
        else:
            break
    return "\n".join(reversed(collected))


def balanced_close(text: str, start: int, opener: str, closer: str) -> int:
    """Index just past the balanced group opened at `start` (string-aware)."""
    depth = 0
    in_str = False
    i = start
    n = len(text)
    while i < n:
        ch = text[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_str = False
        else:
            if ch == '"':
                in_str = True
            elif ch == opener:
                depth += 1
            elif ch == closer:
                depth -= 1
                if depth == 0:
                    return i + 1
        i += 1
    raise ValueError(f"unbalanced {opener!r}/{closer!r} from offset {start}")


# ---------------------------------------------------------------------------
# endpoint extraction (daemon/src/server.rs)
# ---------------------------------------------------------------------------

@dataclass
class Endpoint:
    method: str
    segments: list[str]
    segment_vars: list[bool]
    line: int
    comments: str = ""

    @property
    def key(self) -> str:
        """`GET /api/squads/{id}/cells/{ti}/{si}/pane` form: strips the
        leading `api` segment from the route pattern and braces the variable
        segments the way `docs/daemon-api.md` renders them."""
        segs = list(self.segments)
        vars_ = list(self.segment_vars)
        if segs and segs[0] == "api":
            segs = segs[1:]
            vars_ = vars_[1:]
        parts = [f"{{{s}}}" if v else s for s, v in zip(segs, vars_)]
        return f"{self.method} /api/{'/'.join(parts)}"


def extract_route_arms(source: str) -> list[Endpoint]:
    """Every `("METHOD", ["api", ...]) => ...` arm of the route_for_user match.

    Single string-aware pass over the match block, so multi-line tuple
    patterns (the `proofs/.../open-terminal` routes) and multi-line handler
    bodies cannot desync the scan the way line-based parsing does.
    """
    anchor = source.index("match (method, segs.as_slice()) {")
    i = anchor + len("match (method, segs.as_slice()) {")
    n = len(source)
    depth = 1  # inside the match block (its opening brace is at index `anchor`)
    in_str = False
    line = source.count("\n", 0, i) + 1
    arms: list[Endpoint] = []
    while i < n:
        ch = source[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            if ch == '"':
                in_str = False
            elif ch == "\n":
                line += 1
            i += 1
            continue
        if ch == '"':
            in_str = True
            i += 1
            continue
        if ch == "/" and source[i + 1 : i + 2] == "/":
            j = source.find("\n", i)
            if j >= 0:
                line += 1
                i = j + 1
            else:
                i = n
            continue
        if ch == "/" and source[i + 1 : i + 2] == "*":
            j = source.find("*/", i + 2)
            if j < 0:
                raise ValueError("unterminated block comment in route match")
            line += source[i : j].count("\n")
            i = j + 2
            continue
        if ch in " \t\r":
            i += 1
            continue
        if ch == "\n":
            line += 1
            i += 1
            continue
        if ch == "}":
            depth -= 1
            if depth == 0:
                break  # match block closed
            i += 1
            continue
        if ch == "{":
            depth += 1
            i += 1
            continue
        if depth == 1 and ch == "(":
            # a candidate arm pattern at match level
            pattern_line = line
            close = balanced_close(source, i, "(", ")")
            body = source[i + 1 : close - 1]
            line += source[i : close].count("\n")
            i = close
            while i < n and source[i] in " \t":
                i += 1
            if source[i : i + 2] != "=>":
                raise ValueError(f"expected `=>` after pattern at line {pattern_line}")
            i += 2
            method, segments = parse_pattern(body)
            if method is not None:
                arms.append(Endpoint(method, segments[0], segments[1], pattern_line))
            i = consume_handler(source, i)
            line = source.count("\n", 0, i) + 1
            continue
        if ch == ",":
            i += 1
            continue
        if re.match(r"[A-Za-z_]", ch):
            # catch-all `_ => error(...)` or similar literal-pattern arm
            while i < n and re.match(r"[A-Za-z0-9_]", source[i]):
                i += 1
            while i < n and source[i] in " \t":
                i += 1
            if source[i : i + 2] != "=>":
                raise ValueError(f"unexpected pattern at line {line}")
            i += 2
            i = consume_handler(source, i)
            line = source.count("\n", 0, i) + 1
            continue
        raise ValueError(f"unexpected character {ch!r} at line {line} in route match")
    return arms


def consume_handler(source: str, i: int) -> int:
    """Consume an arm's `=>` expression; returns the position after it."""
    n = len(source)
    while i < n and source[i] in " \t\n\r":
        i += 1
    if source[i] == "{":
        return balanced_close(source, i, "{", "}")
    # any other expression: consume balanced units and operators until a
    # top-level `,` or the match's closing `}`
    while i < n:
        ch = source[i]
        if ch in " \t\n\r":
            i += 1
            continue
        if ch == ",":
            return i + 1
        if ch == "}":
            return i
        if ch == '"':
            i = balanced_close(source, i, '"', '"')
            continue
        if ch in "({[":
            closer = {"(": ")", "{": "}", "[": "]"}[ch]
            i = balanced_close(source, i, ch, closer)
            continue
        if ch == "/" and source[i + 1 : i + 2] == "/":
            j = source.find("\n", i)
            i = j + 1 if j >= 0 else n
            continue
        i += 1
    return i


def parse_pattern(body: str) -> tuple[str | None, tuple[list[str], list[bool]]]:
    """Return (method, (segments, is_variable)) from a `"METHOD", [...]` tuple
    body, or (None, ([], [])) when the body is not a method tuple. Quoted
    parts are literal path segments; bare identifiers are variables."""
    m = re.match(r'\s*"([A-Z]+)"\s*,\s*\[(.*)\]', body, re.S)
    if m is None or m.group(1) not in METHODS:
        return None, ([], [])
    segments: list[str] = []
    vars_: list[bool] = []
    for part in split_top_level(m.group(2)):
        part = part.strip()
        if not part:
            continue
        if part.startswith('"') and part.endswith('"'):
            segments.append(part[1:-1])
            vars_.append(False)
        else:
            segments.append(part)  # a bare identifier: a variable segment
            vars_.append(True)
    return m.group(1), (segments, vars_)


# ---------------------------------------------------------------------------
# CLI leaf extraction (cli/src/help_map.rs)
# ---------------------------------------------------------------------------

@dataclass
class Leaf:
    path: tuple[str, ...]
    line: int
    comments: str = ""


def node_call_starts(source: str) -> list[tuple[int, int]]:
    """(line, offset) of every real `node(` call start, skipping the
    `const fn node(` definition, strings, and comments."""
    starts: list[tuple[int, int]] = []
    in_str = False
    line = 1
    i = 0
    n = len(source)
    while i < n:
        ch = source[i]
        if in_str:
            if ch == "\\":
                # a `\` + newline is a Rust string continuation: the newline
                # belongs to the literal and must still advance `line`
                if source[i + 1] == "\n":
                    line += 1
                i += 2
                continue
            if ch == '"':
                in_str = False
            elif ch == "\n":
                line += 1
            i += 1
            continue
        if ch == '"':
            in_str = True
            i += 1
            continue
        if ch == "\n":
            line += 1
            i += 1
            continue
        if ch == "/" and source[i + 1 : i + 2] == "/":
            j = source.find("\n", i)
            if j >= 0:
                line += 1
                i = j + 1
            else:
                i = n
            continue
        if ch == "/" and source[i + 1 : i + 2] == "*":
            j = source.find("*/", i + 2)
            if j < 0:
                raise ValueError("unterminated block comment in help_map.rs")
            line += source[i : j].count("\n")
            i = j + 2
            continue
        if source.startswith("node(", i):
            if (i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_")) and "fn node(" not in source[
                max(0, i - 30) : i + 5
            ]:
                starts.append((line, i))
        i += 1
    return starts


def node_call_info(source: str, offset: int, line: int) -> dict:
    """Classify one `node(...)` call by its offset in `source`: name, leaf
    flag, the children argument (a const name, `&[]`, or an inline
    `&[ ... ]`), and the call's span so inline children can be attributed
    without re-locating text that may have had comments stripped.

    `offset` points at the `n` of `node(`; the argument text starts after
    the call's own opening paren."""
    open_off = source.index("(", offset, offset + 20)
    close = balanced_close(source, open_off, "(", ")")
    text = source[open_off + 1 : close - 1]
    stripped = strip_comments(text)
    args = split_top_level(stripped)
    while args and not args[-1].strip():
        args.pop()
    name_m = re.match(r'\s*"([^"]*)"', args[0])
    name = name_m.group(1) if name_m else "?"
    children = args[-1].strip().rstrip(",").strip() if args else ""
    return {
        "name": name,
        "line": line,
        "leaf": children == "&[]",
        "children": children,
        "open": open_off,
        "close": close,
    }


def const_array_children(source: str, starts: list[tuple[int, int]]) -> dict[str, list[dict]]:
    """`const X: &[HelpNode] = &[ ... ];` -> its direct child node calls."""
    out: dict[str, list[dict]] = {}
    lines = source.splitlines()
    for i, line_text in enumerate(lines):
        m = re.match(r"\s*(?:pub\s+)?const\s+(\w+)\s*:\s*&\[HelpNode\]\s*=\s*&\[", line_text)
        if m is None:
            continue
        name = m.group(1)
        # the value array is the SECOND `&[` -- the first is the
        # `&[HelpNode]` type annotation
        arr_start = line_text.index("[", line_text.rindex("&["))
        line_offset = sum(len(l) + 1 for l in lines[:i])
        opener_off = line_offset + arr_start
        close_off = balanced_close(source, opener_off, "[", "]")
        out[name] = [
            node_call_info(source, off, ln) for (ln, off) in starts if opener_off < off < close_off
        ]
    return out


def extract_leaves(help_source: str) -> list[Leaf]:
    """Recompute `help_map::registered_leaves()`: every leaf reachable from
    ROOT's children plus QUICK_START, with each leaf's source line."""
    starts = node_call_starts(help_source)
    const_arrays = const_array_children(help_source, starts)

    def children_of(node: dict) -> list[dict]:
        raw = node["children"]
        if raw == "&[]":
            return []
        if raw.startswith("&[") and raw.endswith("]"):
            # inline children array (`&[node(...), ...]`); every real `node(`
            # call opening inside the parent call's own span is a direct
            # child (the parent itself is the only call at its offset).
            # Inline children are only used by ROOT/QUICK_START, whose
            # direct children never nest another inline array themselves
            # (they reference `*_CHILDREN` consts), so the span filter is
            # exact.
            return [
                node_call_info(help_source, off, ln)
                for (ln, off) in starts
                if node["open"] < off < node["close"]
            ]
        if raw in const_arrays:
            return const_arrays[raw]
        raise ValueError(
            f"node {node['name']!r} at line {node['line']} has unrecognized children "
            f"reference {raw!r} (expected `&[]`, a `*_CHILDREN` const, or an inline `&[...]`)"
        )

    # ROOT and QUICK_START are the two roots of registered_leaves()
    root_const_lines: dict[str, int] = {}
    for i, line_text in enumerate(help_source.splitlines()):
        m = re.match(r"\s*(?:pub\s+)?const\s+(ROOT|QUICK_START)\s*:\s*HelpNode\s*=\s*node\(", line_text)
        if m is not None:
            root_const_lines[m.group(1)] = i + 1
    by_line = {ln: info for (ln, info) in ((l, node_call_info(help_source, o, l)) for (l, o) in starts)}
    root_nodes: dict[str, dict] = {}
    for const_name, decl_line in root_const_lines.items():
        info = by_line.get(decl_line)
        if info is None:
            raise ValueError(f"cannot find the `node(` call of const {const_name}")
        root_nodes[const_name] = info

    leaves: list[Leaf] = []
    for const_name, root_node in root_nodes.items():
        prefix = [] if const_name == "ROOT" else ["quick-start"]
        for child in children_of(root_node):
            child_path = prefix + [child["name"]]
            if child["leaf"]:
                leaves.append(Leaf(tuple(child_path), child["line"]))
            else:
                walk_leaves(child, child_path, children_of, leaves)
    leaves.sort(key=lambda leaf: leaf.line)
    return leaves


def walk_leaves(node: dict, path: list[str], children_of, leaves: list[Leaf]) -> None:
    for child in children_of(node):
        child_path = path + [child["name"]]
        if child["leaf"]:
            leaves.append(Leaf(tuple(child_path), child["line"]))
        else:
            walk_leaves(child, child_path, children_of, leaves)


# ---------------------------------------------------------------------------
# coverage
# ---------------------------------------------------------------------------

def violations_for_sources(server_source: str, help_source: str) -> list[str]:
    out: list[str] = []
    try:
        endpoints = extract_route_arms(server_source)
        server_lines = server_source.splitlines()
        for ep in endpoints:
            ep.comments = attach_comments(server_lines, ep.line - 1)
    except (ValueError, IndexError) as error:
        return [f"server.rs route parse failed: {error}"]

    try:
        leaves = extract_leaves(help_source)
        help_lines = help_source.splitlines()
        for leaf in leaves:
            leaf.comments = attach_comments(help_lines, leaf.line - 1)
    except (ValueError, IndexError) as error:
        return [f"help_map.rs parse failed: {error}"]

    endpoint_keys = {ep.key for ep in endpoints} | set(SPECIAL_ROUTES)
    mapped_keys = set(ENDPOINT_TO_CLI)
    mapped_leaves: set[tuple[str, ...]] = set()
    for leaf_paths_for_endpoint in ENDPOINT_TO_CLI.values():
        if not leaf_paths_for_endpoint:
            out.append("ENDPOINT_TO_CLI has an empty leaf list -- every entry must name"
                       " at least one CLI leaf")
        for value in leaf_paths_for_endpoint:
            mapped_leaves.add(tuple(value.split()))
    leaf_paths = {leaf.path for leaf in leaves}

    for ep in endpoints:
        if ep.key in mapped_keys:
            continue
        reason = marker_reason(ep.comments)
        if reason is None:
            out.append(
                f"server.rs:{ep.line}: endpoint {ep.key} has no CLI command; map it in "
                "ENDPOINT_TO_CLI (check_endpoint_cli_parity.py), or add "
                "`// ralphus[ignore-endpoint-cli]: <reason>` above its route arm"
            )
        elif not substantive(reason):
            out.append(
                f"server.rs:{ep.line}: {ep.key} exclusion reason is not substantive: {reason!r}"
            )
    for key, reason in SPECIAL_ROUTES.items():
        if not substantive(reason):
            out.append(f"{key} special-route exclusion reason is not substantive: {reason!r}")
        if key in mapped_keys:
            out.append(f"{key} is both a SPECIAL_ROUTES exclusion and mapped in ENDPOINT_TO_CLI -- pick one")

    covered_leaves = mapped_leaves
    for leaf in leaves:
        if leaf.path in covered_leaves:
            continue
        reason = marker_reason(leaf.comments)
        if reason is None:
            out.append(
                f"help_map.rs:{leaf.line}: CLI leaf {leaf.path!r} has no endpoint mapping; add it "
                "as a mapping value in ENDPOINT_TO_CLI, or add "
                "`// ralphus[ignore-endpoint-cli]: <reason>` above its node() call"
            )
        elif not substantive(reason):
            out.append(
                f"help_map.rs:{leaf.line}: {leaf.path!r} exclusion reason is not substantive: {reason!r}"
            )

    for key in mapped_keys - endpoint_keys:
        out.append(f"ENDPOINT_TO_CLI maps {key!r}, but no such endpoint exists (stale or typo)")
    all_leaf_path_strings = {" ".join(p) for p in leaf_paths}
    for value in mapped_leaves:
        value_string = " ".join(value)
        if value_string not in all_leaf_path_strings:
            out.append(
                f"ENDPOINT_TO_CLI maps to CLI leaf {value_string!r}, but no such leaf exists (stale or typo)"
            )

    for ep in endpoints:
        if marker_reason(ep.comments) is not None and ep.key in mapped_keys:
            out.append(
                f"server.rs:{ep.line}: {ep.key} is both mapped and carries an inline exclusion -- pick one"
            )
    for leaf in leaves:
        if marker_reason(leaf.comments) is not None and leaf.path in covered_leaves:
            out.append(
                f"help_map.rs:{leaf.line}: {leaf.path!r} is both mapped and carries an inline exclusion -- pick one"
            )
    return out


def main() -> int:
    server_source = SERVER_RS.read_text(encoding="utf-8")
    help_source = HELP_MAP_RS.read_text(encoding="utf-8")
    violations = violations_for_sources(server_source, help_source)
    for violation in violations:
        print(violation)
    if not violations:
        endpoints = extract_route_arms(server_source)
        leaves = extract_leaves(help_source)
        print(
            f"endpoints: {len(endpoints)} (+{len(SPECIAL_ROUTES)} special), "
            f"CLI leaves: {len(leaves)} - OK"
        )
    return int(bool(violations))


if __name__ == "__main__":
    sys.exit(main())

"""The ralphus URI scheme (RAL-188): one self-describing way to address any
run/task/session/verify or review entity, readable cold by a human or an AI
agent.

Grammar::

    ralphus:/RUN[<label>]                                                        ?id=<run_id>
    ralphus:/RUN[<label>]/TASK[<name>]                                           ?id=<run_id>
    ralphus:/RUN[<label>]/TASK[<name>]/VERIFY[<name-or-~index>]                  ?id=<run_id>
    ralphus:/RUN[<label>]/TASK[<name>]/SESSION[<name>]                           ?id=<run_id>
    ralphus:/RUN[<label>]/TASK[<name>]/SESSION[<name>]/VERIFY[<name-or-~index>]  ?id=<run_id>
    ralphus:/REVIEW[<name>]                                                      ?id=<guardian_id>
    ralphus:/REVIEW[<name>]?id=<guardian_id>&combined
    ralphus:/REVIEW[<name>]?id=<guardian_id>&worktree=<branch-id-or-name-or-~index>

Design decisions this module implements (RAL-188 §C):

1. **Square brackets** delimit a segment's label. ``[``, ``]`` and ``/`` (plus
   the other characters that would confuse a URL parser) are percent-encoded
   inside a label; consumers percent-decode *after* extracting a balanced
   group and never re-split an extracted label's contents.
2. **A segment's contents are the entity's label**, falling back to its id when
   no label is set. Labels are neither unique nor stable.
3. **``?id=`` is the disambiguator** -- formally optional, always emitted by
   anything ralphus itself produces, and authoritative when present (it wins
   over a stale or renamed label). An ambiguous label with no ``?id=`` is an
   error listing the candidates, never a silent "most recent wins".
4. **A positional index uses a `_URI_INDEX_SIGIL_TOKEN` (``~``) sigil** --
   ``VERIFY[~0]``, ``?worktree=~2`` -- so a name and an index can never be
   confused. A bare ``VERIFY[0]`` addresses a verify step *named* ``0``. (This
   is deliberately stricter than `ralphus.selector`'s legacy grammar, where a
   bare integer means an index.)
5. **Balanced groups are extracted before the path is split on ``/``**, so a
   hand-written ``REVIEW[RAL-174/175 batch]`` with a literal ``/`` inside the
   label still parses.

Every character the grammar gives meaning to is named below as a
``_URI_*_TOKEN`` constant, and every parser here compares against those names
rather than a bare literal -- so "is this ``/`` a separator or part of a
label?" is answerable by reading the code, and the Rust/JS twins can be diffed
against this one by name.

Note that **no grammar token is an RFC 3986 reserved character** except the
ones whose RFC meaning ralphus deliberately reuses (``/``, ``?``, ``&``,
``=``, ``[``, ``]``, all used exactly as RFC 3986 uses them). The positional
sigil in particular is ``~``, an *unreserved* character: a ``#`` there would
start a fragment and truncate the URI in any context that parses it as a real
URL.

Parsing here is entirely offline and never touches the network -- resolving a
parsed URI to concrete indices is `ralphus.selector`'s job.
"""

from __future__ import annotations

from dataclasses import dataclass

__all__ = [
    "KINDS",
    "QUERY_KEYS",
    "SCHEME",
    "URI_INDEX_SIGIL_TOKEN",
    "RalphusUri",
    "Segment",
    "UriError",
    "decode_label",
    "encode_label",
    "encode_query_value",
    "is_positional_token",
    "looks_like_uri",
    "parse_uri",
    "review_uri",
    "run_uri",
]

SCHEME = "ralphus:"

# ── Grammar tokens ───────────────────────────────────────────────────────────
# One name per character the grammar reads. Keep in lockstep with
# `core/src/uri.rs` and board.html's `URI_*_TOKEN` constants.

#: Opens a segment's bracketed value: the ``[`` of ``TASK[ral-178]``.
_URI_VALUE_OPEN_TOKEN = "["
#: Closes a segment's bracketed value: the ``]`` of ``TASK[ral-178]``.
_URI_VALUE_CLOSE_TOKEN = "]"
#: Separates one path segment from the next: the ``/`` of ``RUN[r]/TASK[t]``.
_URI_SEGMENT_SEPARATOR_TOKEN = "/"
#: Starts the query string: the ``?`` of ``RUN[r]?id=run-1``.
_URI_QUERY_OPEN_TOKEN = "?"
#: Separates one query pair from the next: the ``&`` of ``?id=g-1&combined``.
_URI_QUERY_PAIR_SEPARATOR_TOKEN = "&"
#: Separates a query key from its value: the ``=`` of ``?id=run-1``.
_URI_QUERY_ASSIGN_TOKEN = "="
#: Introduces a **positional index** rather than a name: the ``~`` of
#: ``VERIFY[~0]`` and ``?worktree=~2``. Deliberately an RFC 3986 *unreserved*
#: character -- the scheme originally used ``#``, the fragment delimiter, which
#: truncates the URI wherever it is parsed as a real URL.
#:
#: The one grammar token that is *public*, because `ralphus.selector` renders
#: and re-reads positional tokens while resolving a URI and must use the same
#: character this module parses. The rest stay module-private.
URI_INDEX_SIGIL_TOKEN = "~"
_URI_INDEX_SIGIL_TOKEN = URI_INDEX_SIGIL_TOKEN
#: Introduces a two-hex-digit percent-escape inside a label or query value.
_URI_PERCENT_TOKEN = "%"

#: RFC 3986's fragment delimiter. **Not** a ralphus grammar token -- listed
#: only so `_MUST_ENCODE` keeps it out of labels, since a raw one would
#: silently truncate an embedded URI.
_URI_FRAGMENT_TOKEN = "#"
#: **Not** a grammar token either: form-encoded query parsers decode a raw
#: ``+`` as a space, so a label containing one must be escaped.
_URI_PLUS_TOKEN = "+"

#: The digits a positional index may be written with. `str.isdigit()` is not
#: a substitute -- it also accepts non-ASCII forms such as ``²``.
_DECIMAL_DIGITS = "0123456789"
#: The digits a percent-escape may be written with, either case.
_HEX_DIGITS = _DECIMAL_DIGITS + "abcdefABCDEF"

#: Every segment type the grammar knows about, in no particular order.
KINDS = ("RUN", "TASK", "SESSION", "VERIFY", "REVIEW")

#: Every query key the grammar knows about. Unknown keys are rejected rather
#: than ignored, so a typo (``?di=``) surfaces instead of silently addressing
#: the wrong thing.
QUERY_KEYS = ("id", "worktree", "combined")

#: The segment-kind sequences that address a real entity. Anything else (e.g.
#: ``SESSION`` without a parent ``TASK``) is a parse error.
_VALID_PATHS: tuple[tuple[str, ...], ...] = (
    ("RUN",),
    ("RUN", "TASK"),
    ("RUN", "TASK", "SESSION"),
    ("RUN", "TASK", "VERIFY"),
    ("RUN", "TASK", "SESSION", "VERIFY"),
    ("REVIEW",),
)

# Characters that must never appear raw inside a label or a query value: the
# grammar's own tokens, plus the ones a URL/query parser would eat.
# `_URI_PERCENT_TOKEN` is first so encoding is not self-ambiguous, and
# `_URI_INDEX_SIGIL_TOKEN` is in here so a label that literally starts with `~`
# can't be mistaken for a positional index. The fragment/plus tokens are not
# grammar tokens at all -- they are escaped purely so an embedded URI survives
# a real URL parser.
_MUST_ENCODE = "".join(
    (
        _URI_PERCENT_TOKEN,
        _URI_VALUE_OPEN_TOKEN,
        _URI_VALUE_CLOSE_TOKEN,
        _URI_SEGMENT_SEPARATOR_TOKEN,
        _URI_QUERY_OPEN_TOKEN,
        _URI_QUERY_PAIR_SEPARATOR_TOKEN,
        _URI_QUERY_ASSIGN_TOKEN,
        _URI_INDEX_SIGIL_TOKEN,
        _URI_FRAGMENT_TOKEN,
        _URI_PLUS_TOKEN,
    )
)


class UriError(Exception):
    """A ralphus URI could not be parsed. Mapped to CLI exit code 2."""


def _empty_segment_message(kind: str) -> str:
    """The one wording for `TASK[]`, shared by the dataclass guard and the
    parser so they can never drift apart."""
    return (
        f"{kind}{_URI_VALUE_OPEN_TOKEN}{_URI_VALUE_CLOSE_TOKEN} is empty -- a segment "
        f"needs a label, an id, or {_URI_INDEX_SIGIL_TOKEN}<index>"
    )


def encode_label(raw: str) -> str:
    """Percent-encode `raw` so it is safe as a bracket label or query value.

    Everything outside printable ASCII is encoded as UTF-8 bytes; inside
    printable ASCII only the scheme/URL delimiters (`_MUST_ENCODE`) are
    touched, so the common case stays readable (`ral-178`, not `%72%61%6c...`).

    A **space is deliberately left raw** -- the ticket's own motivating example
    is `REVIEW[RAL-174/175 batch]`, and legibility to a human/AI reading the
    URI cold is the whole point of the scheme (§B Q16/Q21). It is safe to do
    so because labels are extracted as balanced `[...]` groups, which a space
    cannot break; a producer embedding a URI somewhere that forbids raw spaces
    (a URL query value, say) percent-encodes the *whole* URI at that boundary,
    and `decode_label` still accepts a `%20` written by hand.
    """
    out: list[str] = []
    for ch in raw:
        if ch in _MUST_ENCODE or ch < " " or ch == "\x7f" or ord(ch) > 0x7E:
            out.extend(f"{_URI_PERCENT_TOKEN}{b:02X}" for b in ch.encode("utf-8"))
        else:
            out.append(ch)
    return "".join(out)


def decode_label(raw: str) -> str:
    """Percent-decode a bracket label or query value extracted from a URI.

    A stray `%` that isn't followed by two hex digits is a parse error rather
    than being passed through, so a mis-encoded label fails loudly instead of
    silently addressing something else.
    """
    out = bytearray()
    i = 0
    while i < len(raw):
        ch = raw[i]
        if ch != _URI_PERCENT_TOKEN:
            out.extend(ch.encode("utf-8"))
            i += 1
            continue
        hexits = raw[i + 1 : i + 3]
        if len(hexits) != 2 or any(c not in _HEX_DIGITS for c in hexits):
            raise UriError(f"invalid percent-escape in '{raw}' at position {i}")
        out.append(int(hexits, 16))
        i += 3
    try:
        return out.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise UriError(f"percent-escapes in '{raw}' are not valid UTF-8") from exc


def _parse_index(digits: str) -> int | None:
    """Read the digits after an `_URI_INDEX_SIGIL_TOKEN`, or `None` if they
    aren't one.

    `str.isdigit()` alone would accept a signed `+3` (no) and non-ASCII digit
    forms such as `²` (which `int()` then rejects outright) -- require plain
    ASCII digits, so all three grammars agree on what a positional token is.
    """
    if not digits or any(c not in _DECIMAL_DIGITS for c in digits):
        return None
    return int(digits)


def is_positional_token(raw: str) -> bool:
    """Whether `raw` is a positional token (`~2`) rather than a name.

    The one rule shared by every place a `~N` can appear -- a segment's bracket
    contents and the `?worktree=` query value -- so the two can't disagree
    about what counts as a position.
    """
    return raw.startswith(_URI_INDEX_SIGIL_TOKEN) and (
        _parse_index(raw[len(_URI_INDEX_SIGIL_TOKEN) :]) is not None
    )


def encode_query_value(raw: str) -> str:
    """Percent-encode a query value, leaving a well-formed positional token
    (`~2`) raw.

    `encode_label` escapes `_URI_INDEX_SIGIL_TOKEN` wherever it appears, which
    is what stops a *label* beginning with `~` from being read as a position.
    But `?worktree=` carries positions too, and running one through
    `encode_label` would render `?worktree=%7E2` -- correct, since it decodes
    straight back, but unreadable, and legibility is the entire point of the
    scheme. So a value that is exactly a positional token is emitted as-is and
    everything else is escaped, which keeps the two unambiguous in both
    directions.
    """
    return raw if is_positional_token(raw) else encode_label(raw)


@dataclass(frozen=True)
class Segment:
    """One `TYPE[label]` path segment.

    Exactly one of `name`/`index` is set: `index` for the `~N` positional form,
    `name` for everything else.
    """

    kind: str
    name: str | None = None
    index: int | None = None

    def __post_init__(self) -> None:
        if (self.name is None) == (self.index is None):
            raise UriError(f"{self.kind}: a segment needs exactly one of a name or an index")
        if self.name is not None and not self.name:
            raise UriError(_empty_segment_message(self.kind))
        if self.index is not None and self.index < 0:
            raise UriError(
                f"{self.kind}{_URI_VALUE_OPEN_TOKEN}{_URI_INDEX_SIGIL_TOKEN}{self.index}"
                f"{_URI_VALUE_CLOSE_TOKEN}: a positional index cannot be negative"
            )

    @property
    def label(self) -> str:
        """The canonical (encoded) bracket contents for this segment."""
        if self.index is not None:
            return f"{_URI_INDEX_SIGIL_TOKEN}{self.index}"
        return encode_label(self.name or "")

    def __str__(self) -> str:
        return f"{self.kind}{_URI_VALUE_OPEN_TOKEN}{self.label}{_URI_VALUE_CLOSE_TOKEN}"


def _segment(kind: str, value: str | int) -> Segment:
    """Build a `Segment` from a name (`str`) or a positional index (`int`)."""
    if isinstance(value, int):
        return Segment(kind, index=value)
    return Segment(kind, name=value)


@dataclass(frozen=True)
class RalphusUri:
    """A parsed (or hand-built) ralphus URI.

    `query` keeps insertion order and distinguishes a bare flag (`?combined`,
    value `None`) from an empty-valued key (`?combined=`, value `""`).
    """

    segments: tuple[Segment, ...]
    query: tuple[tuple[str, str | None], ...] = ()

    @property
    def kinds(self) -> tuple[str, ...]:
        """The segment kinds, e.g. `("RUN", "TASK", "SESSION")`."""
        return tuple(s.kind for s in self.segments)

    @property
    def leaf(self) -> Segment:
        """The last (most specific) segment."""
        return self.segments[-1]

    def get(self, key: str) -> str | None:
        """The value of query `key`, or `None` if absent or a bare flag."""
        for k, v in self.query:
            if k == key:
                return v
        return None

    def has(self, key: str) -> bool:
        """Whether query `key` is present at all (with or without a value)."""
        return any(k == key for k, _ in self.query)

    @property
    def id(self) -> str | None:
        """The authoritative `?id=` sidecar, if the URI carries one."""
        value = self.get("id")
        return value or None

    def __str__(self) -> str:
        path = _URI_SEGMENT_SEPARATOR_TOKEN.join(str(s) for s in self.segments)
        parts = [
            k if v is None else f"{k}{_URI_QUERY_ASSIGN_TOKEN}{encode_query_value(v)}"
            for k, v in self.query
        ]
        joined = _URI_QUERY_PAIR_SEPARATOR_TOKEN.join(parts)
        tail = f"{_URI_QUERY_OPEN_TOKEN}{joined}" if parts else ""
        return f"{SCHEME}{_URI_SEGMENT_SEPARATOR_TOKEN}{path}{tail}"


def looks_like_uri(raw: str) -> bool:
    """Whether `raw` should be parsed as a ralphus URI rather than a legacy
    `ralphus.selector` string.

    Requires a `[` in both accepted forms so the unrelated
    ``ralphus:new-review/<key>`` TOML link syntax isn't captured here.
    """
    if _URI_VALUE_OPEN_TOKEN not in raw:
        return False
    if raw.startswith(SCHEME):
        return True
    head = raw.split(_URI_VALUE_OPEN_TOKEN, 1)[0]
    return bool(head) and head.isalpha() and head.isupper()


def _split_at_query(raw: str) -> tuple[str, str | None]:
    """Split `raw` at its first bracket-depth-zero `_URI_QUERY_OPEN_TOKEN`, so
    a `?` inside a label stays part of the label."""
    depth = 0
    for i, ch in enumerate(raw):
        if ch == _URI_VALUE_OPEN_TOKEN:
            depth += 1
        elif ch == _URI_VALUE_CLOSE_TOKEN:
            depth = max(0, depth - 1)
        elif ch == _URI_QUERY_OPEN_TOKEN and depth == 0:
            return raw[:i], raw[i + 1 :]
    return raw, None


def _scan_segments(path: str) -> list[tuple[str, str]]:
    """Extract balanced `TYPE[...]` groups *before* splitting on
    `_URI_SEGMENT_SEPARATOR_TOKEN` (§C.5).

    Returns the raw (still-encoded) `(kind, label)` pairs.
    """
    out: list[tuple[str, str]] = []
    i = 0
    while i < len(path):
        open_at = path.find(_URI_VALUE_OPEN_TOKEN, i)
        if open_at < 0:
            raise UriError(
                f"segment '{path[i:]}' is missing its "
                f"'{_URI_VALUE_OPEN_TOKEN}label{_URI_VALUE_CLOSE_TOKEN}'"
            )
        kind = path[i:open_at]
        depth = 1
        j = open_at + 1
        while j < len(path) and depth:
            if path[j] == _URI_VALUE_OPEN_TOKEN:
                depth += 1
            elif path[j] == _URI_VALUE_CLOSE_TOKEN:
                depth -= 1
            j += 1
        if depth:
            raise UriError(f"unterminated '{_URI_VALUE_OPEN_TOKEN}' in '{path}'")
        out.append((kind, path[open_at + 1 : j - 1]))
        if j == len(path):
            break
        if path[j] != _URI_SEGMENT_SEPARATOR_TOKEN:
            raise UriError(f"unexpected '{path[j]}' after '{_URI_VALUE_CLOSE_TOKEN}' in '{path}'")
        i = j + 1
        if i == len(path):
            raise UriError(f"trailing '{_URI_SEGMENT_SEPARATOR_TOKEN}' in '{path}'")
    return out


def _parse_segment(kind: str, encoded_label: str) -> Segment:
    if kind not in KINDS:
        raise UriError(f"unknown segment type '{kind}' (expected one of {', '.join(KINDS)})")
    if not encoded_label:
        raise UriError(_empty_segment_message(kind))
    if encoded_label.startswith(_URI_INDEX_SIGIL_TOKEN):
        index = _parse_index(encoded_label[len(_URI_INDEX_SIGIL_TOKEN) :])
        if index is None:
            raise UriError(
                f"{kind}{_URI_VALUE_OPEN_TOKEN}{encoded_label}{_URI_VALUE_CLOSE_TOKEN}: "
                f"'{_URI_INDEX_SIGIL_TOKEN}' must be followed by a number"
            )
        return Segment(kind, index=index)
    return Segment(kind, name=decode_label(encoded_label))


def _parse_query(raw: str) -> tuple[tuple[str, str | None], ...]:
    out: list[tuple[str, str | None]] = []
    for part in raw.split(_URI_QUERY_PAIR_SEPARATOR_TOKEN):
        if not part:
            continue
        key, sep, value = part.partition(_URI_QUERY_ASSIGN_TOKEN)
        if key not in QUERY_KEYS:
            raise UriError(f"unknown query key '{key}' (expected one of {', '.join(QUERY_KEYS)})")
        if any(k == key for k, _ in out):
            raise UriError(f"query key '{key}' given more than once")
        out.append((key, decode_label(value) if sep else None))
    return tuple(out)


def parse_uri(raw: str) -> RalphusUri:
    """Parse a ralphus URI. Offline only -- names stay unresolved.

    Accepts both the full ``ralphus:/RUN[...]`` form and the bare
    ``RUN[...]`` shorthand; `str()` always renders the full form back.
    """
    text = raw.strip()
    if not text:
        raise UriError("empty URI")
    if text.startswith(SCHEME):
        text = text[len(SCHEME) :]
    text = text.lstrip(_URI_SEGMENT_SEPARATOR_TOKEN)
    if not text:
        raise UriError(f"'{raw}' has no path segments")

    path, query = _split_at_query(text)
    segments = tuple(_parse_segment(k, label) for k, label in _scan_segments(path))
    parsed = RalphusUri(segments, _parse_query(query or ""))

    separator = _URI_SEGMENT_SEPARATOR_TOKEN
    kinds = parsed.kinds
    if kinds not in _VALID_PATHS:
        shapes = ", ".join(separator.join(p) for p in _VALID_PATHS)
        raise UriError(f"'{raw}' addresses {separator.join(kinds)}; valid shapes are: {shapes}")
    if kinds[0] != "REVIEW":
        for key in ("worktree", "combined"):
            if parsed.has(key):
                raise UriError(
                    f"'{_URI_QUERY_OPEN_TOKEN}{key}' is only valid on a "
                    f"REVIEW{_URI_VALUE_OPEN_TOKEN}...{_URI_VALUE_CLOSE_TOKEN} URI"
                )
    if parsed.has("worktree") and parsed.has("combined"):
        raise UriError("'?worktree=' and '?combined' address different things -- pick one")
    if parsed.has("worktree") and not parsed.get("worktree"):
        raise UriError(
            f"'?worktree=' needs a branch id, a branch name, or {_URI_INDEX_SIGIL_TOKEN}<position>"
        )
    return parsed


def run_uri(
    *,
    run: str,
    run_id: str | None = None,
    task: str | int | None = None,
    session: str | int | None = None,
    verify: str | int | None = None,
    verify_scope: str = "",
) -> str:
    """Build the canonical URI for a run/task/session/verify entity.

    `run` is the run's label (or its id when it has no label); `run_id` is the
    `?id=` sidecar, which ralphus always emits (§C.3). `verify_scope` is only
    consulted to reject a task-level verify addressed under a session.
    """
    segments = [_segment("RUN", run)]
    if task is not None:
        segments.append(_segment("TASK", task))
        if session is not None and verify_scope != "task":
            segments.append(_segment("SESSION", session))
        if verify is not None:
            segments.append(_segment("VERIFY", verify))
    query: list[tuple[str, str | None]] = [("id", run_id)] if run_id else []
    return str(RalphusUri(tuple(segments), tuple(query)))


def review_uri(
    *,
    review: str,
    guardian_id: str | None = None,
    worktree: str | int | None = None,
    combined: bool = False,
) -> str:
    """Build the canonical URI for a review, one of its branch worktrees
    (`worktree`), or its combined worktree (`combined`).

    A `str` `worktree` is the branch's label (its feature branch name) or its
    stable `branch-...` id; an `int` is a position, rendered with the
    `_URI_INDEX_SIGIL_TOKEN` sigil. ralphus itself always emits the label.
    """
    query: list[tuple[str, str | None]] = [("id", guardian_id)] if guardian_id else []
    if worktree is not None:
        query.append(
            (
                "worktree",
                f"{_URI_INDEX_SIGIL_TOKEN}{worktree}" if isinstance(worktree, int) else worktree,
            )
        )
    elif combined:
        query.append(("combined", None))
    return str(RalphusUri((_segment("REVIEW", review),), tuple(query)))

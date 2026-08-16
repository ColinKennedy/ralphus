"""Tests for the ralphus URI grammar (RAL-188) -- parsing, rendering, and the
escaping rules that keep a hostile label from breaking either.

Resolution (a URI -> concrete indices) lives in `test_selector.py`; everything
here is offline and never touches a client.
"""

from __future__ import annotations

import pytest

from ralphus.uri import (
    URI_INDEX_SIGIL_TOKEN,
    RalphusUri,
    Segment,
    UriError,
    decode_label,
    encode_label,
    encode_query_value,
    is_positional_token,
    looks_like_uri,
    parse_uri,
    review_uri,
    run_uri,
)

# ── encode/decode ────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("raw", "encoded"),
    [
        ("ral-178", "ral-178"),
        ("RAL-174/175 batch", "RAL-174%2F175 batch"),
        ("foo[bar]", "foo%5Bbar%5D"),
        ("a?b&c=d", "a%3Fb%26c%3Dd"),
        ("#0", "%230"),
        ("~0", "%7E0"),
        ("100%", "100%25"),
        ("a+b", "a%2Bb"),
        ("café", "caf%C3%A9"),
        ("tab\there", "tab%09here"),
    ],
)
def test_encode_decode_round_trip(raw: str, encoded: str) -> None:
    assert encode_label(raw) == encoded
    assert decode_label(encoded) == raw


def test_decode_rejects_a_truncated_escape() -> None:
    with pytest.raises(UriError, match="invalid percent-escape"):
        decode_label("a%2")


def test_decode_rejects_a_non_hex_escape() -> None:
    with pytest.raises(UriError, match="invalid percent-escape"):
        decode_label("a%zz")


def test_decode_rejects_escapes_that_are_not_utf8() -> None:
    with pytest.raises(UriError, match="not valid UTF-8"):
        decode_label("%FF%FE")


# ── looks_like_uri (the accept-either dispatch, RAL-188 §C.6) ────────────────


@pytest.mark.parametrize(
    "raw",
    ["ralphus:/RUN[a]", "RUN[a]", "REVIEW[a]?id=g", "RUN[a]/TASK[b]"],
)
def test_looks_like_uri_accepts_the_uri_form(raw: str) -> None:
    assert looks_like_uri(raw)


@pytest.mark.parametrize(
    "raw",
    [
        "run-000000000151",
        "run-1/build/compile/verify/1",
        "@my-review#feature/a",
        "guardian-1#0",
        # RAL-188 must not capture the unrelated `ralphus:new-review/<key>`
        # TOML link syntax -- it has no bracket.
        "ralphus:new-review/batch",
        "",
    ],
)
def test_looks_like_uri_rejects_the_legacy_and_unrelated_forms(raw: str) -> None:
    assert not looks_like_uri(raw)


# ── parsing ──────────────────────────────────────────────────────────────────


def test_parse_full_path_with_id_sidecar() -> None:
    uri = parse_uri("ralphus:/RUN[nightly]/TASK[ral-178]/SESSION[work]?id=run-000000000151")
    assert uri.kinds == ("RUN", "TASK", "SESSION")
    assert [s.name for s in uri.segments] == ["nightly", "ral-178", "work"]
    assert uri.id == "run-000000000151"
    assert uri.leaf == Segment("SESSION", name="work")


def test_parse_accepts_the_bare_shorthand_and_renders_the_full_form() -> None:
    assert str(parse_uri("RUN[a]?id=run-1")) == "ralphus:/RUN[a]?id=run-1"


def test_parse_extracts_balanced_groups_before_splitting_on_slash() -> None:
    """§C.5: the ticket's own `REVIEW[RAL-174/175 batch]` example."""
    uri = parse_uri("ralphus:/REVIEW[RAL-174/175 batch]?id=guardian-1")
    assert uri.segments == (Segment("REVIEW", name="RAL-174/175 batch"),)


def test_parse_keeps_a_question_mark_inside_a_label_out_of_the_query() -> None:
    uri = parse_uri("ralphus:/REVIEW[why?]?id=guardian-1")
    assert uri.leaf.name == "why?"
    assert uri.id == "guardian-1"


def test_parse_decodes_an_escaped_delimiter_without_re_splitting_it() -> None:
    uri = parse_uri("ralphus:/RUN[foo%5Dbar%2Fbaz]?id=run-1")
    assert uri.leaf.name == "foo]bar/baz"


def test_parse_positional_index_uses_the_sigil() -> None:
    uri = parse_uri("ralphus:/RUN[a]/TASK[b]/VERIFY[~3]?id=run-1")
    assert uri.leaf == Segment("VERIFY", index=3)


def test_parse_a_bare_number_is_a_name_not_an_index() -> None:
    uri = parse_uri("ralphus:/RUN[a]/TASK[b]/VERIFY[3]?id=run-1")
    assert uri.leaf == Segment("VERIFY", name="3")


def test_parse_a_label_that_literally_starts_with_the_sigil_is_escaped() -> None:
    uri = parse_uri("ralphus:/RUN[a]/TASK[b]/VERIFY[%7E0]?id=run-1")
    assert uri.leaf == Segment("VERIFY", name="~0")
    assert encode_label("~0") == "%7E0"


def test_parse_a_hash_is_an_ordinary_label_character_not_a_sigil() -> None:
    """`#` lost its sigil meaning to `~` (RFC 3986: `#` starts a fragment), so
    it is now escaped like any other label character."""
    uri = parse_uri("ralphus:/RUN[a]/TASK[b]/VERIFY[%230]?id=run-1")
    assert uri.leaf == Segment("VERIFY", name="#0")
    assert encode_label("#0") == "%230"


def test_parse_bare_flag_is_distinct_from_an_empty_value() -> None:
    assert parse_uri("REVIEW[a]?combined").query == (("combined", None),)
    assert parse_uri("REVIEW[a]?id=").query == (("id", ""),)
    assert parse_uri("REVIEW[a]?id=").id is None


@pytest.mark.parametrize(
    ("raw", "message"),
    [
        ("", "empty URI"),
        ("ralphus:/", "no path segments"),
        ("ralphus:/RUN[a", "unterminated"),
        ("ralphus:/RUN[]", "empty"),
        ("ralphus:/RUN[a]x/TASK[b]", "unexpected 'x'"),
        ("ralphus:/RUN[a]/", "trailing '/'"),
        ("ralphus:/RUN[a]/TASK", "missing its '\\[label\\]'"),
        ("ralphus:/NOPE[a]", "unknown segment type"),
        ("ralphus:/RUN[a]/VERIFY[~-1]", "'~' must be followed by a number"),
        # A non-ASCII digit passes `str.isdigit()` but is not a position.
        ("ralphus:/RUN[a]/TASK[b]/VERIFY[~²]", "'~' must be followed by a number"),
        ("ralphus:/TASK[a]", "valid shapes are"),
        ("ralphus:/RUN[a]/SESSION[b]", "valid shapes are"),
        ("ralphus:/RUN[a]?di=x", "unknown query key"),
        ("ralphus:/RUN[a]?id=1&id=2", "given more than once"),
        ("ralphus:/RUN[a]?combined", "only valid on a REVIEW"),
        ("ralphus:/RUN[a]?worktree=x", "only valid on a REVIEW"),
        ("ralphus:/REVIEW[a]?worktree=x&combined", "pick one"),
        ("ralphus:/REVIEW[a]?worktree=", "needs a branch id, a branch name"),
    ],
)
def test_parse_rejects_malformed_input(raw: str, message: str) -> None:
    with pytest.raises(UriError, match=message):
        parse_uri(raw)


# ── building ─────────────────────────────────────────────────────────────────


def test_run_uri_builds_every_level() -> None:
    assert run_uri(run="nightly", run_id="run-1") == "ralphus:/RUN[nightly]?id=run-1"
    assert (
        run_uri(run="nightly", run_id="run-1", task="build", session="compile")
        == "ralphus:/RUN[nightly]/TASK[build]/SESSION[compile]?id=run-1"
    )
    assert (
        run_uri(run="n", run_id="run-1", task="build", verify=0, verify_scope="task")
        == "ralphus:/RUN[n]/TASK[build]/VERIFY[~0]?id=run-1"
    )


def test_run_uri_omits_the_session_for_a_task_scoped_verify() -> None:
    """A task-level verify is not under any session, even though the caller
    may pass one through from a shared resolved selector."""
    uri = run_uri(
        run="n", run_id="run-1", task="build", session="compile", verify="t", verify_scope="task"
    )
    assert uri == "ralphus:/RUN[n]/TASK[build]/VERIFY[t]?id=run-1"


def test_review_uri_worktree_and_combined() -> None:
    assert review_uri(review="batch", guardian_id="g1") == "ralphus:/REVIEW[batch]?id=g1"
    assert (
        review_uri(review="batch", guardian_id="g1", combined=True)
        == "ralphus:/REVIEW[batch]?id=g1&combined"
    )
    # A positional worktree stays readable -- escaping the sigil to `%7E2`
    # would round-trip but defeat the point of the scheme.
    assert (
        review_uri(review="batch", guardian_id="g1", worktree=2)
        == "ralphus:/REVIEW[batch]?id=g1&worktree=~2"
    )
    assert (
        review_uri(review="batch", guardian_id="g1", worktree="branch-000000000278")
        == "ralphus:/REVIEW[batch]?id=g1&worktree=branch-000000000278"
    )
    assert (
        review_uri(review="batch", guardian_id="g1", worktree="feature/a")
        == "ralphus:/REVIEW[batch]?id=g1&worktree=feature%2Fa"
    )


def test_every_built_uri_parses_back_to_the_same_thing() -> None:
    for built in (
        run_uri(run="a/b]c", run_id="run-1", task="t?t", session="s&s", verify=7),
        review_uri(review="RAL-174/175 batch", guardian_id="g1", worktree="feature/a"),
        review_uri(review="x", guardian_id="g1", combined=True),
    ):
        assert str(parse_uri(built)) == built


def test_a_segment_needs_exactly_one_of_a_name_or_an_index() -> None:
    with pytest.raises(UriError, match="exactly one"):
        Segment("RUN")
    with pytest.raises(UriError, match="exactly one"):
        Segment("RUN", name="a", index=0)
    with pytest.raises(UriError, match="cannot be negative"):
        Segment("RUN", index=-1)


def test_uri_query_accessors() -> None:
    uri = RalphusUri((Segment("REVIEW", name="a"),), (("id", "g1"), ("combined", None)))
    assert uri.get("id") == "g1"
    assert uri.get("combined") is None
    assert uri.has("combined")
    assert not uri.has("worktree")


# ── grammar tokens ───────────────────────────────────────────────────────────


def test_the_index_sigil_is_an_rfc_3986_unreserved_character() -> None:
    """The whole reason it isn't `#`: every RFC 3986 *reserved* character has a
    parsing meaning some consumer will act on -- `#` starts a fragment, so a
    URI carrying one is truncated wherever it meets a real URL parser."""
    assert URI_INDEX_SIGIL_TOKEN not in "!$&'()*+,;=:/?#[]@"
    assert URI_INDEX_SIGIL_TOKEN in "-._~"


def test_every_grammar_token_is_escaped_inside_a_label() -> None:
    """A grammar token the encoder let through raw would re-parse as structure
    the next time the URI is read."""
    for token in "[]/?&=~%":
        assert encode_label(token) != token, token
        assert decode_label(encode_label(token)) == token, token


def test_a_positional_query_value_is_emitted_raw_and_a_name_is_escaped() -> None:
    """`?worktree=~2` must stay readable -- escaping the sigil to `%7E2` would
    round-trip correctly but defeat the point of the scheme."""
    assert encode_query_value("~2") == "~2"
    # Only an exact `~<digits>` is spared; anything else is a name.
    assert encode_query_value("~2x") == "%7E2x"
    assert encode_query_value("feature/a") == "feature%2Fa"
    assert is_positional_token("~0")
    assert not is_positional_token("~")
    assert not is_positional_token("2")

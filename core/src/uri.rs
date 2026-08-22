//! The ralphus URI scheme (RAL-188): one self-describing way to address any
//! squad/task/cell/proof or review entity, readable cold by a human or an AI
//! agent.
//!
//! ```text
//! ralphus:/SQUAD[<label>]                                                     ?id=<squad_id>
//! ralphus:/SQUAD[<label>]/TASK[<name>]                                        ?id=<squad_id>
//! ralphus:/SQUAD[<label>]/TASK[<name>]/PROOF[<name-or-~index>]                ?id=<squad_id>
//! ralphus:/SQUAD[<label>]/TASK[<name>]/CELL[<name>]                           ?id=<squad_id>
//! ralphus:/SQUAD[<label>]/TASK[<name>]/CELL[<name>]/PROOF[<name-or-~index>]   ?id=<squad_id>
//! ralphus:/REVIEW[<name>]                                                      ?id=<guardian_id>
//! ralphus:/REVIEW[<name>]?id=<guardian_id>&combined
//! ralphus:/REVIEW[<name>]?id=<guardian_id>&worktree=<branch-id-or-name-or-~index>
//! ```
//!
//! This is the Rust twin of `cli/src/ralphus/uri.py`; the two implement the
//! same grammar and must stay in lockstep. Parsing is pure and offline —
//! resolving a parsed URI to concrete indices needs the store, so that lives in
//! the daemon (`GET /api/resolve?uri=…`, RAL-188 §C.7).
//!
//! Design decisions implemented here (RAL-188 §C):
//!
//! 1. **Square brackets** delimit a segment's label. `[`, `]` and `/` (plus the
//!    characters that would confuse a URL parser) are percent-encoded inside a
//!    label; consumers percent-decode *after* extracting a balanced group and
//!    never re-split an extracted label's contents.
//! 2. **A segment's contents are the entity's label**, falling back to its id
//!    when no label is set. Labels are neither unique nor stable.
//! 3. **`?id=` is the disambiguator** — formally optional, always emitted by
//!    anything ralphus itself produces, and authoritative when present.
//! 4. **A positional index uses a [`INDEX_SIGIL_TOKEN`] (`~`) sigil**
//!    (`PROOF[~0]`, `?worktree=~2`), so a name and an index can never be
//!    confused. A bare `PROOF[0]` addresses a proof step *named* `0`.
//! 5. **Balanced groups are extracted before the path is split on `/`**, so
//!    `REVIEW[RAL-174/175 batch]` — a literal `/` inside a label — still parses.
//!
//! Every character the grammar gives meaning to is named below as a `…_TOKEN`
//! constant, and every parser here compares against those names rather than
//! against a bare literal — so "is this `/` a separator or part of a label?" is
//! answerable by reading the code, and the three implementations can be diffed
//! against each other by name.
//!
//! Note that **no grammar token is an RFC 3986 *reserved* character** except
//! the ones whose RFC meaning ralphus deliberately reuses (`/`, `?`, `&`, `=`,
//! `[`, `]`, all used exactly as RFC 3986 uses them). The positional sigil in
//! particular is `~`, an *unreserved* character: a `#` there would start a
//! fragment and truncate the URI in any context that parses it as a real URL.

use std::fmt;

/// The scheme prefix every canonical ralphus URI carries.
pub const SCHEME: &str = "ralphus:";

// ── Grammar tokens ───────────────────────────────────────────────────────────
// One name per character the grammar reads. Keep in lockstep with
// `cli/src/ralphus/uri.py` and board.html's `URI_*_TOKEN` constants.

/// Opens a segment's bracketed value: the `[` of `TASK[ral-178]`.
pub const VALUE_OPEN_TOKEN: char = '[';
/// Closes a segment's bracketed value: the `]` of `TASK[ral-178]`.
pub const VALUE_CLOSE_TOKEN: char = ']';
/// Separates one path segment from the next: the `/` of `SQUAD[r]/TASK[t]`.
pub const SEGMENT_SEPARATOR_TOKEN: char = '/';
/// Starts the query string: the `?` of `SQUAD[r]?id=squad-1`.
pub const QUERY_OPEN_TOKEN: char = '?';
/// Separates one query pair from the next: the `&` of `?id=g-1&combined`.
pub const QUERY_PAIR_SEPARATOR_TOKEN: char = '&';
/// Separates a query key from its value: the `=` of `?id=squad-1`.
pub const QUERY_ASSIGN_TOKEN: char = '=';
/// Introduces a **positional index** rather than a name: the `~` of
/// `PROOF[~0]` and `?worktree=~2`.
///
/// Deliberately an RFC 3986 *unreserved* character. The scheme originally used
/// `#`, which is the fragment delimiter — a raw one truncates the URI wherever
/// it is parsed as a real URL, and forced every consumer to special-case it.
pub const INDEX_SIGIL_TOKEN: char = '~';
/// Introduces a two-hex-digit percent-escape inside a label or query value.
pub const PERCENT_TOKEN: char = '%';

/// RFC 3986's fragment delimiter. **Not** a ralphus grammar token — it is
/// listed here only so [`MUST_ENCODE`] keeps it out of labels, since a raw one
/// would silently truncate an embedded URI.
const FRAGMENT_TOKEN: char = '#';
/// **Not** a ralphus grammar token either: form-encoded query parsers decode a
/// raw `+` as a space, so a label containing one must be escaped.
const PLUS_TOKEN: char = '+';

// Byte twins of the tokens the balanced-group scanners walk. Those scanners
// index `str::as_bytes()`, and every token is ASCII, so the two always agree.
const VALUE_OPEN_BYTE: u8 = VALUE_OPEN_TOKEN as u8;
const VALUE_CLOSE_BYTE: u8 = VALUE_CLOSE_TOKEN as u8;
const SEGMENT_SEPARATOR_BYTE: u8 = SEGMENT_SEPARATOR_TOKEN as u8;
const PERCENT_BYTE: u8 = PERCENT_TOKEN as u8;

/// Every segment type the grammar knows about.
pub const KINDS: [&str; 5] = ["SQUAD", "TASK", "CELL", "PROOF", "REVIEW"];

/// Every query key the grammar knows about. Unknown keys are rejected rather
/// than ignored, so a typo (`?di=`) surfaces instead of silently addressing the
/// wrong thing.
pub const QUERY_KEYS: [&str; 3] = ["id", "worktree", "combined"];

/// The segment-kind sequences that address a real entity. Anything else (e.g.
/// `CELL` without a parent `TASK`) is a parse error.
const VALID_PATHS: [&[&str]; 6] = [
    &["SQUAD"],
    &["SQUAD", "TASK"],
    &["SQUAD", "TASK", "CELL"],
    &["SQUAD", "TASK", "PROOF"],
    &["SQUAD", "TASK", "CELL", "PROOF"],
    &["REVIEW"],
];

/// Characters that must never appear raw inside a label or a query value: the
/// grammar's own tokens, plus the ones a URL/query parser would eat.
///
/// [`PERCENT_TOKEN`] is first so encoding is not self-ambiguous, and
/// [`INDEX_SIGIL_TOKEN`] is in here so a label that literally starts with `~`
/// can't be mistaken for a positional index. [`FRAGMENT_TOKEN`] and
/// [`PLUS_TOKEN`] are not grammar tokens at all — they are escaped purely so an
/// embedded URI survives a real URL parser.
const MUST_ENCODE: [char; 10] = [
    PERCENT_TOKEN,
    VALUE_OPEN_TOKEN,
    VALUE_CLOSE_TOKEN,
    SEGMENT_SEPARATOR_TOKEN,
    QUERY_OPEN_TOKEN,
    QUERY_PAIR_SEPARATOR_TOKEN,
    QUERY_ASSIGN_TOKEN,
    INDEX_SIGIL_TOKEN,
    FRAGMENT_TOKEN,
    PLUS_TOKEN,
];

/// A ralphus URI could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriError(pub String);

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UriError {}

fn err<T>(message: impl Into<String>) -> Result<T, UriError> {
    Err(UriError(message.into()))
}

/// Percent-encode `raw` so it is safe as a bracket label or query value.
///
/// Everything outside printable ASCII is encoded as UTF-8 bytes; inside
/// printable ASCII only the scheme/URL delimiters ([`MUST_ENCODE`]) are
/// touched, so the common case stays readable (`ral-178`, not `%72%61%6c…`).
///
/// A **space is deliberately left raw** — the ticket's own motivating example
/// is `REVIEW[RAL-174/175 batch]`, and legibility to a human/AI reading the URI
/// cold is the whole point of the scheme. It is safe because labels are
/// extracted as balanced `[…]` groups, which a space cannot break; a producer
/// embedding a URI where raw spaces are forbidden percent-encodes the *whole*
/// URI at that boundary, and [`decode_label`] still accepts a hand-written
/// `%20`.
#[must_use]
pub fn encode_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if MUST_ENCODE.contains(&ch) || ch < ' ' || ch == '\x7f' || ch > '\x7e' {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("{PERCENT_TOKEN}{byte:02X}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Percent-decode a bracket label or query value extracted from a URI.
///
/// A stray `%` that isn't followed by two hex digits is an error rather than
/// being passed through, so a mis-encoded label fails loudly instead of
/// silently addressing something else.
///
/// # Errors
/// Returns [`UriError`] on an invalid percent-escape or on escapes that do not
/// decode to valid UTF-8.
pub fn decode_label(raw: &str) -> Result<String, UriError> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != PERCENT_BYTE {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // `u8::from_str_radix` would happily accept a signed "+2"; the Python
        // and JS twins reject it, so require two literal hex digits.
        let hexits = raw
            .get(i + 1..i + 3)
            .filter(|h| h.len() == 2 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let value = hexits.and_then(|h| u8::from_str_radix(h, 16).ok());
        match value {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => return err(format!("invalid percent-escape in '{raw}' at position {i}")),
        }
    }
    String::from_utf8(out)
        .map_err(|_| UriError(format!("percent-escapes in '{raw}' are not valid UTF-8")))
}

/// Whether `raw` is a positional token (`~2`) rather than a name.
///
/// The one rule shared by every place a `~N` can appear — a segment's bracket
/// contents and the `?worktree=` query value — so the two can't disagree about
/// what counts as a position.
#[must_use]
pub fn is_positional_token(raw: &str) -> bool {
    raw.strip_prefix(INDEX_SIGIL_TOKEN)
        .and_then(parse_index)
        .is_some()
}

/// Percent-encode a query value, leaving a well-formed positional token (`~2`)
/// raw.
///
/// [`encode_label`] escapes [`INDEX_SIGIL_TOKEN`] wherever it appears, which is
/// what stops a *label* beginning with `~` from being read as a position. But
/// `?worktree=` carries positions too, and running one through [`encode_label`]
/// would render `?worktree=%7E2` — correct, since it decodes straight back, but
/// unreadable, and legibility is the entire point of the scheme. So a value
/// that is exactly a positional token is emitted as-is and everything else is
/// escaped, which keeps the two unambiguous in both directions.
#[must_use]
pub fn encode_query_value(raw: &str) -> String {
    if is_positional_token(raw) {
        raw.to_string()
    } else {
        encode_label(raw)
    }
}

/// One `TYPE[label]` path segment. Exactly one of `name`/`index` is set:
/// `index` for the `~N` positional form, `name` for everything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment type, one of [`KINDS`].
    pub kind: String,
    /// The decoded label, when the segment addresses by name.
    pub name: Option<String>,
    /// The positional index, when the segment used the `~N` form.
    pub index: Option<usize>,
}

impl Segment {
    /// A by-name segment, e.g. `TASK[ral-178]`.
    #[must_use]
    pub fn named(kind: &str, name: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            name: Some(name.into()),
            index: None,
        }
    }

    /// A positional segment, e.g. `PROOF[~0]`.
    #[must_use]
    pub fn positional(kind: &str, index: usize) -> Self {
        Self {
            kind: kind.to_string(),
            name: None,
            index: Some(index),
        }
    }

    /// How this segment addresses its entity — by name, or by position.
    ///
    /// A parsed segment always carries exactly one of the two; a hand-built
    /// one with neither degrades to an empty name, which never matches (see
    /// [`Token::resolve`]).
    #[must_use]
    pub fn token(&self) -> Token {
        match self.index {
            Some(index) => Token::Index(index),
            None => Token::Name(self.name.clone().unwrap_or_default()),
        }
    }

    /// The canonical (encoded) bracket contents for this segment.
    #[must_use]
    pub fn label(&self) -> String {
        match (&self.index, &self.name) {
            (Some(i), _) => format!("{INDEX_SIGIL_TOKEN}{i}"),
            (None, Some(name)) => encode_label(name),
            (None, None) => String::new(),
        }
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{VALUE_OPEN_TOKEN}{}{VALUE_CLOSE_TOKEN}",
            self.kind,
            self.label()
        )
    }
}

/// A parsed (or hand-built) ralphus URI.
///
/// `query` keeps insertion order and distinguishes a bare flag (`?combined`,
/// value `None`) from an empty-valued key (`?combined=`, value `Some("")`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RalphusUri {
    /// The `TYPE[label]` path segments, outermost first.
    pub segments: Vec<Segment>,
    /// The `?key[=value]` pairs, in the order they appeared.
    pub query: Vec<(String, Option<String>)>,
}

impl RalphusUri {
    /// The segment kinds, e.g. `["SQUAD", "TASK", "CELL"]`.
    #[must_use]
    pub fn kinds(&self) -> Vec<&str> {
        self.segments.iter().map(|s| s.kind.as_str()).collect()
    }

    /// The first segment whose kind is `kind`, if any.
    #[must_use]
    pub fn segment(&self, kind: &str) -> Option<&Segment> {
        self.segments.iter().find(|s| s.kind == kind)
    }

    /// The value of query `key`, or `None` if absent or a bare flag.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Whether query `key` is present at all (with or without a value).
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.query.iter().any(|(k, _)| k == key)
    }

    /// The authoritative `?id=` sidecar, if the URI carries one.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.get("id").filter(|v| !v.is_empty())
    }
}

impl fmt::Display for RalphusUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let separator = SEGMENT_SEPARATOR_TOKEN.to_string();
        let path = self
            .segments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(&separator);
        write!(f, "{SCHEME}{SEGMENT_SEPARATOR_TOKEN}{path}")?;
        if self.query.is_empty() {
            return Ok(());
        }
        let parts: Vec<String> = self
            .query
            .iter()
            .map(|(k, v)| match v {
                Some(value) => format!("{k}{QUERY_ASSIGN_TOKEN}{}", encode_query_value(value)),
                None => k.clone(),
            })
            .collect();
        write!(
            f,
            "{QUERY_OPEN_TOKEN}{}",
            parts.join(&QUERY_PAIR_SEPARATOR_TOKEN.to_string())
        )
    }
}

/// Whether `raw` should be parsed as a ralphus URI rather than a legacy
/// selector string.
///
/// Requires a `[` in both accepted forms so the unrelated
/// `ralphus:new-review/<key>` TOML link syntax isn't captured here.
#[must_use]
pub fn looks_like_uri(raw: &str) -> bool {
    if !raw.contains(VALUE_OPEN_TOKEN) {
        return false;
    }
    if raw.starts_with(SCHEME) {
        return true;
    }
    let head = raw.split(VALUE_OPEN_TOKEN).next().unwrap_or("");
    !head.is_empty() && head.chars().all(|c| c.is_ascii_uppercase())
}

/// Split `raw` at its first bracket-depth-zero [`QUERY_OPEN_TOKEN`], so a `?`
/// inside a label stays part of the label.
fn split_at_query(raw: &str) -> (&str, Option<&str>) {
    let mut depth = 0usize;
    for (i, ch) in raw.char_indices() {
        match ch {
            VALUE_OPEN_TOKEN => depth += 1,
            VALUE_CLOSE_TOKEN => depth = depth.saturating_sub(1),
            QUERY_OPEN_TOKEN if depth == 0 => return (&raw[..i], Some(&raw[i + 1..])),
            _ => {}
        }
    }
    (raw, None)
}

/// Extract balanced `TYPE[…]` groups *before* splitting on
/// [`SEGMENT_SEPARATOR_TOKEN`] (§C.5), returning the raw (still-encoded)
/// `(kind, label)` pairs.
fn scan_segments(path: &str) -> Result<Vec<(&str, &str)>, UriError> {
    let bytes = path.as_bytes();
    let mut out: Vec<(&str, &str)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(open_at) = path[i..].find(VALUE_OPEN_TOKEN).map(|o| i + o) else {
            return err(format!(
                "segment '{}' is missing its '{VALUE_OPEN_TOKEN}label{VALUE_CLOSE_TOKEN}'",
                &path[i..]
            ));
        };
        let kind = &path[i..open_at];
        let mut depth = 1usize;
        let mut j = open_at + 1;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                VALUE_OPEN_BYTE => depth += 1,
                VALUE_CLOSE_BYTE => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        if depth > 0 {
            return err(format!("unterminated '{VALUE_OPEN_TOKEN}' in '{path}'"));
        }
        out.push((kind, &path[open_at + 1..j - 1]));
        if j == bytes.len() {
            break;
        }
        if bytes[j] != SEGMENT_SEPARATOR_BYTE {
            return err(format!(
                "unexpected '{}' after '{VALUE_CLOSE_TOKEN}' in '{path}'",
                bytes[j] as char
            ));
        }
        i = j + 1;
        if i == bytes.len() {
            return err(format!("trailing '{SEGMENT_SEPARATOR_TOKEN}' in '{path}'"));
        }
    }
    Ok(out)
}

/// Read the digits after an [`INDEX_SIGIL_TOKEN`]. `str::parse::<usize>` would
/// accept a signed `+3`, which the Python and JS twins reject — require plain
/// digits so all three grammars agree on what a positional token is.
fn parse_index(digits: &str) -> Option<usize> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()
}

fn parse_segment(kind: &str, encoded_label: &str) -> Result<Segment, UriError> {
    if !KINDS.contains(&kind) {
        return err(format!(
            "unknown segment type '{kind}' (expected one of {})",
            KINDS.join(", ")
        ));
    }
    if encoded_label.is_empty() {
        return err(format!(
            "{kind}{VALUE_OPEN_TOKEN}{VALUE_CLOSE_TOKEN} is empty -- a segment needs \
             a label, an id, or {INDEX_SIGIL_TOKEN}<index>"
        ));
    }
    if let Some(digits) = encoded_label.strip_prefix(INDEX_SIGIL_TOKEN) {
        return match parse_index(digits) {
            Some(index) => Ok(Segment::positional(kind, index)),
            None => err(format!(
                "{kind}{VALUE_OPEN_TOKEN}{encoded_label}{VALUE_CLOSE_TOKEN}: \
                 '{INDEX_SIGIL_TOKEN}' must be followed by a number"
            )),
        };
    }
    Ok(Segment::named(kind, decode_label(encoded_label)?))
}

fn parse_query(raw: &str) -> Result<Vec<(String, Option<String>)>, UriError> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for part in raw.split(QUERY_PAIR_SEPARATOR_TOKEN) {
        if part.is_empty() {
            continue;
        }
        let (key, value) = match part.split_once(QUERY_ASSIGN_TOKEN) {
            Some((k, v)) => (k, Some(decode_label(v)?)),
            None => (part, None),
        };
        if !QUERY_KEYS.contains(&key) {
            return err(format!(
                "unknown query key '{key}' (expected one of {})",
                QUERY_KEYS.join(", ")
            ));
        }
        if out.iter().any(|(k, _)| k == key) {
            return err(format!("query key '{key}' given more than once"));
        }
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// Parse a ralphus URI. Offline only — names stay unresolved.
///
/// Accepts both the full `ralphus:/SQUAD[…]` form and the bare `SQUAD[…]`
/// shorthand; [`RalphusUri::to_string`] always renders the full form back.
///
/// # Errors
/// Returns [`UriError`] when the string is not a well-formed ralphus URI:
/// unbalanced brackets, an unknown segment type or query key, a bad
/// percent-escape, or a segment sequence that addresses nothing real.
pub fn parse_uri(raw: &str) -> Result<RalphusUri, UriError> {
    let mut text = raw.trim();
    if text.is_empty() {
        return err("empty URI");
    }
    if let Some(rest) = text.strip_prefix(SCHEME) {
        text = rest;
    }
    text = text.trim_start_matches(SEGMENT_SEPARATOR_TOKEN);
    if text.is_empty() {
        return err(format!("'{raw}' has no path segments"));
    }

    let (path, query) = split_at_query(text);
    let mut segments = Vec::new();
    for (kind, label) in scan_segments(path)? {
        segments.push(parse_segment(kind, label)?);
    }
    let parsed = RalphusUri {
        segments,
        query: parse_query(query.unwrap_or(""))?,
    };

    let kinds = parsed.kinds();
    if !VALID_PATHS.contains(&kinds.as_slice()) {
        let separator = SEGMENT_SEPARATOR_TOKEN.to_string();
        let shapes: Vec<String> = VALID_PATHS.iter().map(|p| p.join(&separator)).collect();
        return err(format!(
            "'{raw}' addresses {}; valid shapes are: {}",
            kinds.join(&separator),
            shapes.join(", ")
        ));
    }
    if kinds.first() != Some(&"REVIEW") {
        for key in ["worktree", "combined"] {
            if parsed.has(key) {
                return err(format!(
                    "'{QUERY_OPEN_TOKEN}{key}' is only valid on a \
                     REVIEW{VALUE_OPEN_TOKEN}...{VALUE_CLOSE_TOKEN} URI"
                ));
            }
        }
    }
    if parsed.has("worktree") && parsed.has("combined") {
        return err("'?worktree=' and '?combined' address different things -- pick one");
    }
    if parsed.has("worktree") && parsed.get("worktree").unwrap_or("").is_empty() {
        return err(format!(
            "'?worktree=' needs a branch id, a branch name, or {INDEX_SIGIL_TOKEN}<position>"
        ));
    }
    Ok(parsed)
}

/// Either a name or a positional index — how a segment or a `?worktree=` value
/// addresses its entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Address by name, e.g. `TASK[ral-178]`.
    Name(String),
    /// Address by position, e.g. `PROOF[~0]`.
    Index(usize),
}

impl Token {
    /// Read a raw token (`~3` or a name) the way the URI grammar does: only an
    /// [`INDEX_SIGIL_TOKEN`]-prefixed run of digits is positional (§C.4).
    ///
    /// # Errors
    /// Returns [`UriError`] when the sigil is not followed by a number.
    pub fn parse(raw: &str) -> Result<Self, UriError> {
        match raw.strip_prefix(INDEX_SIGIL_TOKEN) {
            Some(digits) => match parse_index(digits) {
                Some(index) => Ok(Self::Index(index)),
                None => err(format!(
                    "'{raw}': '{INDEX_SIGIL_TOKEN}' must be followed by a number"
                )),
            },
            None => Ok(Self::Name(raw.to_string())),
        }
    }

    /// Resolve this token to an index into `candidates`.
    ///
    /// An empty candidate means "this entity has no name of its own" — it is
    /// addressable only positionally and never matches a name lookup (§C.4).
    /// An ambiguous name is an error listing the candidates, never a silent
    /// "first one wins" (§C.3).
    ///
    /// # Errors
    /// Returns [`UriError`] when the index is out of range, no candidate has
    /// that name, or more than one does.
    pub fn resolve(&self, candidates: &[String], what: &str) -> Result<usize, UriError> {
        match self {
            Self::Index(i) => {
                if *i >= candidates.len() {
                    return err(format!(
                        "{what} index {i} out of range (have {})",
                        candidates.len()
                    ));
                }
                Ok(*i)
            }
            Self::Name(name) => {
                let matches: Vec<usize> = candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !c.is_empty() && *c == name)
                    .map(|(i, _)| i)
                    .collect();
                match matches.as_slice() {
                    [] => {
                        let options: Vec<String> = candidates
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                if c.is_empty() {
                                    format!("{INDEX_SIGIL_TOKEN}{i}")
                                } else {
                                    c.clone()
                                }
                            })
                            .collect();
                        let options = if options.is_empty() {
                            "(none)".to_string()
                        } else {
                            options.join(", ")
                        };
                        err(format!("no {what} named '{name}' (available: {options})"))
                    }
                    [only] => Ok(*only),
                    many => err(format!(
                        "'{name}' matches {} {what}s at positions {many:?} -- use \
                         {INDEX_SIGIL_TOKEN}{} (a positional index) instead",
                        many.len(),
                        many[0]
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> RalphusUri {
        parse_uri(raw).expect("should parse")
    }

    #[test]
    fn parses_a_bare_squad_uri() {
        let uri = parsed("ralphus:/SQUAD[my squad]?id=squad-000000000151");
        assert_eq!(uri.kinds(), vec!["SQUAD"]);
        assert_eq!(uri.segments[0].name.as_deref(), Some("my squad"));
        assert_eq!(uri.id(), Some("squad-000000000151"));
    }

    #[test]
    fn accepts_the_bare_shorthand_without_the_scheme() {
        assert_eq!(parsed("SQUAD[r]/TASK[t]").kinds(), vec!["SQUAD", "TASK"]);
    }

    #[test]
    fn round_trips_through_display() {
        let raw = "ralphus:/SQUAD[my squad]/TASK[ral-178]/CELL[work]?id=squad-000000000151";
        assert_eq!(parsed(raw).to_string(), raw);
    }

    #[test]
    fn a_slash_inside_a_review_label_survives_because_groups_are_extracted_first() {
        let uri = parsed("ralphus:/REVIEW[RAL-174/175 batch]?id=guardian-000000000003");
        assert_eq!(uri.segments.len(), 1);
        assert_eq!(uri.segments[0].name.as_deref(), Some("RAL-174/175 batch"));
    }

    #[test]
    fn a_produced_slash_label_is_percent_encoded_and_decodes_back() {
        let uri = RalphusUri {
            segments: vec![Segment::named("REVIEW", "RAL-174/175 batch")],
            query: vec![("id".to_string(), Some("guardian-000000000003".to_string()))],
        };
        let text = uri.to_string();
        assert!(text.contains("RAL-174%2F175 batch"), "{text}");
        assert_eq!(parsed(&text), uri);
    }

    #[test]
    fn a_sigil_token_is_positional_and_a_bare_token_is_a_name() {
        let uri = parsed("ralphus:/SQUAD[r]/TASK[t]/PROOF[~0]");
        assert_eq!(uri.segment("PROOF").and_then(|s| s.index), Some(0));
        let named = parsed("ralphus:/SQUAD[r]/TASK[t]/PROOF[0]");
        assert_eq!(
            named.segment("PROOF").and_then(|s| s.name.clone()),
            Some("0".to_string())
        );
    }

    /// The sigil is `~`, not `#`: a raw `#` starts an RFC 3986 fragment, so it
    /// must be percent-encoded like any other label character and can never be
    /// read as a positional index.
    #[test]
    fn the_index_sigil_is_unreserved_and_a_hash_is_just_a_label_character() {
        assert!(
            !"!$&'()*+,;=:/?#[]@".contains(INDEX_SIGIL_TOKEN),
            "reserved"
        );
        let named = parsed("ralphus:/SQUAD[r]/TASK[t]/PROOF[%230]");
        assert_eq!(
            named.segment("PROOF").and_then(|s| s.name.clone()),
            Some("#0".to_string())
        );
        assert_eq!(encode_label("#0"), "%230");
        // A label that literally starts with the sigil is encoded, so it can
        // never be mistaken for an index on the way back in.
        assert_eq!(encode_label("~0"), "%7E0");
        let literal = parsed("ralphus:/SQUAD[r]/TASK[t]/PROOF[%7E0]");
        assert_eq!(
            literal.segment("PROOF").and_then(|s| s.name.clone()),
            Some("~0".to_string())
        );
    }

    /// Every character the grammar reads must also be one `encode_label` hides
    /// inside a label, or a label containing it would re-parse as structure.
    #[test]
    fn every_grammar_token_is_escaped_inside_a_label() {
        for token in [
            VALUE_OPEN_TOKEN,
            VALUE_CLOSE_TOKEN,
            SEGMENT_SEPARATOR_TOKEN,
            QUERY_OPEN_TOKEN,
            QUERY_PAIR_SEPARATOR_TOKEN,
            QUERY_ASSIGN_TOKEN,
            INDEX_SIGIL_TOKEN,
            PERCENT_TOKEN,
        ] {
            assert!(
                MUST_ENCODE.contains(&token),
                "grammar token '{token}' is not in MUST_ENCODE"
            );
            assert_eq!(
                decode_label(&encode_label(&token.to_string())).unwrap(),
                token.to_string()
            );
        }
    }

    #[test]
    fn rejects_an_unknown_segment_type_and_query_key() {
        assert!(parse_uri("ralphus:/BOGUS[x]").is_err());
        assert!(parse_uri("ralphus:/SQUAD[r]?di=x").is_err());
    }

    #[test]
    fn rejects_a_shape_that_addresses_nothing() {
        assert!(parse_uri("ralphus:/SQUAD[r]/CELL[s]").is_err());
        assert!(parse_uri("ralphus:/TASK[t]").is_err());
    }

    #[test]
    fn rejects_worktree_and_combined_together_and_on_a_squad() {
        assert!(parse_uri("ralphus:/REVIEW[r]?worktree=a&combined").is_err());
        assert!(parse_uri("ralphus:/SQUAD[r]?combined").is_err());
    }

    #[test]
    fn rejects_unbalanced_and_malformed_brackets() {
        assert!(parse_uri("ralphus:/SQUAD[r").is_err());
        assert!(parse_uri("ralphus:/SQUAD[]").is_err());
        assert!(parse_uri("ralphus:/SQUAD[r]x").is_err());
        assert!(parse_uri("ralphus:/SQUAD[r]/").is_err());
    }

    #[test]
    fn rejects_a_bad_percent_escape_instead_of_passing_it_through() {
        assert!(parse_uri("ralphus:/SQUAD[a%zz]").is_err());
        assert!(parse_uri("ralphus:/SQUAD[a%2]").is_err());
        // Rust's own `from_str_radix`/`parse` accept a signed "+2"; the Python
        // and JS twins don't, so neither may this.
        assert!(parse_uri("ralphus:/SQUAD[a%+2]").is_err());
        assert!(parse_uri("ralphus:/SQUAD[r]/TASK[t]/PROOF[~+2]").is_err());
        assert!(Token::parse("~+2").is_err());
    }

    #[test]
    fn a_question_mark_inside_a_label_is_part_of_the_label() {
        let uri = parsed("ralphus:/SQUAD[why%3F]?id=squad-1");
        assert_eq!(uri.segments[0].name.as_deref(), Some("why?"));
        assert_eq!(uri.id(), Some("squad-1"));
    }

    #[test]
    fn encode_label_leaves_the_readable_case_alone() {
        assert_eq!(encode_label("ral-178"), "ral-178");
        assert_eq!(encode_label("RAL-174/175 batch"), "RAL-174%2F175 batch");
        assert_eq!(encode_label("a[b]c"), "a%5Bb%5Dc");
    }

    /// A `?worktree=~2` must stay readable: escaping the sigil to `%7E2` would
    /// round-trip correctly but defeat the point of the scheme.
    #[test]
    fn a_positional_query_value_is_emitted_raw_and_a_name_is_escaped() {
        let positional = RalphusUri {
            segments: vec![Segment::named("REVIEW", "batch")],
            query: vec![
                ("id".to_string(), Some("guardian-1".to_string())),
                ("worktree".to_string(), Some("~2".to_string())),
            ],
        };
        assert_eq!(
            positional.to_string(),
            "ralphus:/REVIEW[batch]?id=guardian-1&worktree=~2"
        );
        assert_eq!(parsed(&positional.to_string()), positional);
        // Only an exact `~<digits>` is spared -- anything else is a name.
        assert_eq!(encode_query_value("~2x"), "%7E2x");
        assert_eq!(encode_query_value("feature/a"), "feature%2Fa");
        assert!(is_positional_token("~0"));
        assert!(!is_positional_token("~"));
        assert!(!is_positional_token("2"));
    }

    #[test]
    fn encode_and_decode_round_trip_non_ascii() {
        let raw = "réview ✓";
        assert_eq!(decode_label(&encode_label(raw)).unwrap(), raw);
    }

    #[test]
    fn looks_like_uri_ignores_legacy_selectors_and_toml_links() {
        assert!(looks_like_uri("ralphus:/SQUAD[r]"));
        assert!(looks_like_uri("REVIEW[r]"));
        assert!(!looks_like_uri("squad-000000000151/t0/s0"));
        assert!(!looks_like_uri("ralphus:new-review/my-key"));
        assert!(!looks_like_uri("@my review#2"));
    }

    #[test]
    fn token_resolution_matches_names_and_positions() {
        let candidates = vec!["lint".to_string(), String::new(), "test".to_string()];
        assert_eq!(
            Token::parse("test")
                .unwrap()
                .resolve(&candidates, "proof")
                .unwrap(),
            2
        );
        assert_eq!(
            Token::parse("~1")
                .unwrap()
                .resolve(&candidates, "proof")
                .unwrap(),
            1
        );
        // An anonymous step never matches a name lookup, and ~N is bounds-checked.
        assert!(
            Token::parse("")
                .unwrap()
                .resolve(&candidates, "proof")
                .is_err()
        );
        assert!(
            Token::parse("~9")
                .unwrap()
                .resolve(&candidates, "proof")
                .is_err()
        );
        assert!(Token::parse("~x").is_err());
        // A `#`-prefixed token is now an ordinary name, not an index.
        assert!(matches!(Token::parse("#1"), Ok(Token::Name(_))));
    }

    #[test]
    fn an_ambiguous_name_is_an_error_listing_positions() {
        let candidates = vec!["dup".to_string(), "dup".to_string()];
        let e = Token::parse("dup")
            .unwrap()
            .resolve(&candidates, "task")
            .unwrap_err();
        assert!(e.to_string().contains("matches 2 tasks"), "{e}");
    }
}

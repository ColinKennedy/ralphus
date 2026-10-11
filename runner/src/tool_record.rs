//! Structured per-tool records over the `RALPHUS_TOOL:` stderr marker.
//!
//! Every agent backend still prints its human-readable `[tool.*]` /
//! `[result]` lines to the pane; alongside them it emits one of these
//! records per tool call and per tool result so the board's Agent Log tab
//! reads exact tool names, tool-use ids, full inputs and full outputs
//! instead of re-parsing the truncated text tape.
//!
//! A record is one JSON object on one line. The call half carries
//! `tool`/`sub`/`tag`/`input`, the result half carries `output`/`is_error`,
//! and both carry the same `id`; a consumer merges halves by `id`. A backend
//! that only learns about a call once it has finished (Codex) puts both
//! halves in one record.
//!
//! The marker must match `librarian/assets/board/30-live-view.js`'s
//! `RALPHUS_TAPE_TOOL_PREFIX` and `daemon/src/server.rs`'s pane stripping.

use serde_json::{Value, json};

/// Must match the board's `RALPHUS_TAPE_TOOL_PREFIX` and the daemon's `/pane` stripping.
pub const TOOL_MARKER: &str = "RALPHUS_TOOL: ";

/// Longest `input` / `output` kept in a record, in characters. Generous
/// enough to hold real command output, bounded so one huge tool result
/// cannot bloat the transcript.
pub const MAX_FIELD_CHARS: usize = 64 * 1024;

/// Splits a `tool.<name>[.<sub>...]` pane tag into `(tool, sub)`.
#[must_use]
pub fn split_tag(tag: &str) -> (&str, &str) {
    let rest = tag.strip_prefix("tool.").unwrap_or(tag);
    match rest.split_once('.') {
        Some((tool, sub)) => (tool, sub),
        None => (rest, ""),
    }
}

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_FIELD_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_FIELD_CHARS).collect();
    format!("{kept}…")
}

/// Builds the call half of a record.
#[must_use]
pub fn call_record(backend: &str, id: &str, tag: &str, input: &str) -> Value {
    let (tool, sub) = split_tag(tag);
    json!({
        "v": 1,
        "backend": backend,
        "id": id,
        "tool": tool,
        "sub": sub,
        "tag": tag,
        "input": clip(input),
    })
}

/// Builds the result half of a record.
#[must_use]
pub fn result_record(backend: &str, id: &str, output: &str, is_error: bool) -> Value {
    json!({
        "v": 1,
        "backend": backend,
        "id": id,
        "output": clip(output),
        "is_error": is_error,
    })
}

/// Builds a record carrying both halves, for a backend that only reports a
/// call once it has completed.
#[must_use]
pub fn complete_record(
    backend: &str,
    id: &str,
    tag: &str,
    input: &str,
    output: &str,
    is_error: bool,
) -> Value {
    let mut rec = call_record(backend, id, tag, input);
    rec["output"] = json!(clip(output));
    rec["is_error"] = json!(is_error);
    rec
}

/// Writes one record to stderr as a marker line.
pub fn emit(record: &Value) {
    eprintln!("{TOOL_MARKER}{record}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tag_separates_tool_and_sub() {
        assert_eq!(split_tag("tool.Bash.git"), ("Bash", "git"));
        assert_eq!(split_tag("tool.Read"), ("Read", ""));
        assert_eq!(split_tag("tool.mcp.srv.name"), ("mcp", "srv.name"));
    }

    #[test]
    fn complete_record_carries_both_halves_on_one_line() {
        let rec = complete_record(
            "codex",
            "i1",
            "tool.exec.cargo",
            "cargo test",
            "a\nb",
            false,
        );
        assert_eq!(rec["tool"], "exec");
        assert_eq!(rec["sub"], "cargo");
        assert_eq!(rec["output"], "a\nb");
        assert!(!rec.to_string().contains('\n'));
    }

    #[test]
    fn oversized_fields_are_clipped() {
        let rec = result_record("x", "1", &"a".repeat(MAX_FIELD_CHARS + 5), true);
        assert_eq!(
            rec["output"].as_str().unwrap().chars().count(),
            MAX_FIELD_CHARS + 1
        );
        assert_eq!(rec["is_error"], true);
    }
}

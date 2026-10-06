//! Parser for the `RALPHUS_APPRAISAL:` marker a scored proof step (a prompt
//! proof with a resolved `pass_score`) ends its reply with.
//!
//! Unlike [`crate::prophecy`], which collects every single-line marker, an
//! appraisal is one multi-line JSON value: [`parse_appraisal`] takes the
//! **last** marker in the reply, reads the first complete JSON value after it
//! with a streaming deserializer (so trailing prose is ignored), and keeps
//! only `score`, `summary`, and `sections`. Over-cap text is truncated with a
//! visible [`TRUNCATION_MARK`] rather than rejected.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The literal marker the agent writes before the JSON object.
pub const MARKER: &str = "RALPHUS_APPRAISAL:";
/// Lowest accepted appraisal score.
pub const MIN_SCORE: u8 = 1;
/// Highest accepted appraisal score.
pub const MAX_SCORE: u8 = 10;
/// Cap on `summary`, in characters.
pub const MAX_SUMMARY_CHARS: usize = 1_500;
/// Cap on the number of `sections` kept.
pub const MAX_SECTIONS: usize = 8;
/// Cap on one section `title`, in characters.
pub const MAX_TITLE_CHARS: usize = 100;
/// Cap on one section `body`, in characters.
pub const MAX_BODY_CHARS: usize = 4_000;
/// Cap on summary + every title + every body, in characters.
pub const MAX_TOTAL_CHARS: usize = 20_000;
/// Appended to any text cut short by a cap.
pub const TRUNCATION_MARK: &str = "…(truncated)";

/// One titled markdown section of an appraisal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppraisalSection {
    pub title: String,
    pub body: String,
}

/// The wire shape carried on `CellResult.appraisal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppraisalMarker {
    pub score: u8,
    pub summary: String,
    pub sections: Vec<AppraisalSection>,
}

/// Cuts `text` to at most `max` characters, appending [`TRUNCATION_MARK`]
/// when anything was removed.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(TRUNCATION_MARK);
    out
}

/// Parses the last `RALPHUS_APPRAISAL:` marker in `text`. The `Err` string is
/// the human-readable reason the reply carries no usable appraisal.
pub fn parse_appraisal(text: &str) -> Result<AppraisalMarker, String> {
    let idx = text
        .rfind(MARKER)
        .ok_or_else(|| format!("no {MARKER} marker found"))?;
    let rest = text[idx + MARKER.len()..].trim_start();
    let value = serde_json::Deserializer::from_str(rest)
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| format!("{MARKER} marker has no JSON after it"))?
        .map_err(|e| format!("{MARKER} JSON is not parseable: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| format!("{MARKER} JSON must be an object"))?;
    let score = obj
        .get("score")
        .ok_or_else(|| "appraisal is missing `score`".to_string())?
        .as_u64()
        .ok_or_else(|| "appraisal `score` must be an integer".to_string())?;
    let score = u8::try_from(score)
        .ok()
        .filter(|s| (MIN_SCORE..=MAX_SCORE).contains(s))
        .ok_or_else(|| format!("appraisal `score` {score} is outside {MIN_SCORE}-{MAX_SCORE}"))?;

    let summary = truncate(
        obj.get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
        MAX_SUMMARY_CHARS,
    );
    let mut remaining = MAX_TOTAL_CHARS.saturating_sub(summary.chars().count());
    let mut sections = Vec::new();
    let raw_sections = obj.get("sections").and_then(Value::as_array);
    for raw in raw_sections.into_iter().flatten().take(MAX_SECTIONS) {
        let (Some(title), Some(body)) = (
            raw.get("title").and_then(Value::as_str),
            raw.get("body").and_then(Value::as_str),
        ) else {
            continue;
        };
        if remaining == 0 {
            break;
        }
        let title = truncate(title.trim(), MAX_TITLE_CHARS.min(remaining));
        remaining = remaining.saturating_sub(title.chars().count());
        let body = truncate(body.trim(), MAX_BODY_CHARS.min(remaining));
        remaining = remaining.saturating_sub(body.chars().count());
        sections.push(AppraisalSection { title, body });
    }
    Ok(AppraisalMarker {
        score,
        summary,
        sections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_marker_wins() {
        let a = parse_appraisal(
            "RALPHUS_APPRAISAL: {\"score\": 2}\nthen\nRALPHUS_APPRAISAL: {\"score\": 9, \"summary\": \"ok\"}",
        )
        .unwrap();
        assert_eq!(a.score, 9);
        assert_eq!(a.summary, "ok");
    }

    #[test]
    fn multi_line_json_and_trailing_chatter() {
        let a = parse_appraisal(
            "x\nRALPHUS_APPRAISAL: {\n  \"score\": 4,\n  \"summary\": \"s\",\n  \"sections\": [\n    {\"title\": \"T\", \"body\": \"line1\\nline2\"}\n  ]\n}\nHope that helps!\n",
        )
        .unwrap();
        assert_eq!(a.score, 4);
        assert_eq!(a.sections.len(), 1);
        assert_eq!(a.sections[0].body, "line1\nline2");
    }

    #[test]
    fn unknown_keys_are_dropped() {
        let a = parse_appraisal(
            "RALPHUS_APPRAISAL: {\"score\": 5, \"extra\": 1, \"sections\": [{\"title\": \"a\", \"body\": \"b\", \"x\": 1}]}",
        )
        .unwrap();
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert_eq!(a.summary, "");
    }

    #[test]
    fn caps_truncate_visibly() {
        let long = "a".repeat(MAX_SUMMARY_CHARS + 10);
        let a = parse_appraisal(&format!(
            "RALPHUS_APPRAISAL: {{\"score\": 5, \"summary\": \"{long}\"}}"
        ))
        .unwrap();
        assert!(a.summary.ends_with(TRUNCATION_MARK));
        assert_eq!(
            a.summary.chars().count(),
            MAX_SUMMARY_CHARS + TRUNCATION_MARK.chars().count()
        );

        let title = "t".repeat(MAX_TITLE_CHARS + 1);
        let body = "b".repeat(MAX_BODY_CHARS + 1);
        let a = parse_appraisal(&format!(
            "RALPHUS_APPRAISAL: {{\"score\": 5, \"sections\": [{{\"title\": \"{title}\", \"body\": \"{body}\"}}]}}"
        ))
        .unwrap();
        assert!(a.sections[0].title.ends_with(TRUNCATION_MARK));
        assert!(a.sections[0].body.ends_with(TRUNCATION_MARK));
    }

    #[test]
    fn section_count_and_total_are_capped() {
        let sections = (0..12)
            .map(|i| format!("{{\"title\": \"s{i}\", \"body\": \"x\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let a = parse_appraisal(&format!(
            "RALPHUS_APPRAISAL: {{\"score\": 5, \"sections\": [{sections}]}}"
        ))
        .unwrap();
        assert_eq!(a.sections.len(), MAX_SECTIONS);

        let body = "b".repeat(MAX_BODY_CHARS);
        let sections = (0..MAX_SECTIONS)
            .map(|i| format!("{{\"title\": \"s{i}\", \"body\": \"{body}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let a = parse_appraisal(&format!(
            "RALPHUS_APPRAISAL: {{\"score\": 5, \"sections\": [{sections}]}}"
        ))
        .unwrap();
        let total: usize = a
            .sections
            .iter()
            .map(|s| s.title.chars().count() + s.body.chars().count())
            .sum::<usize>()
            + a.summary.chars().count();
        assert!(total <= MAX_TOTAL_CHARS + 2 * MAX_SECTIONS * TRUNCATION_MARK.chars().count());
    }

    #[test]
    fn every_fail_closed_case_is_an_error() {
        for bad in [
            "no marker at all",
            "RALPHUS_APPRAISAL:",
            "RALPHUS_APPRAISAL: not json",
            "RALPHUS_APPRAISAL: [1, 2]",
            "RALPHUS_APPRAISAL: {\"summary\": \"x\"}",
            "RALPHUS_APPRAISAL: {\"score\": \"7\"}",
            "RALPHUS_APPRAISAL: {\"score\": 7.5}",
            "RALPHUS_APPRAISAL: {\"score\": 0}",
            "RALPHUS_APPRAISAL: {\"score\": 11}",
            "RALPHUS_APPRAISAL: {\"score\": -3}",
            "RALPHUS_APPRAISAL: {\"score\": 999999}",
            "RALPHUS_APPRAISAL: {\"score\": 5",
        ] {
            assert!(parse_appraisal(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn boundary_scores_are_accepted() {
        assert_eq!(
            parse_appraisal("RALPHUS_APPRAISAL: {\"score\": 1}")
                .unwrap()
                .score,
            1
        );
        assert_eq!(
            parse_appraisal("RALPHUS_APPRAISAL: {\"score\": 10}")
                .unwrap()
                .score,
            10
        );
    }
}

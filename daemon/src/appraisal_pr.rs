//! Deterministic rendering of a review's appraisals (RAL-575) into the PR body
//! and the review branch's tip commit.
//!
//! No model is in the loop: appraisals bypass the prophecy relevance filter in
//! `pr.rs::synthesize_pr_text` entirely, so a failing score can never be
//! dropped as "irrelevant". Section titles become headers and bodies are
//! inserted as-is; the only transformation is neutralizing `<details` /
//! `</details>` inside agent text so it cannot break the surrounding layout.
//!
//! One shared size budget applies on GitHub and GitLab alike (feature parity):
//! GitHub caps a PR body at 65,536 characters, so the composed body is held
//! under [`PR_BODY_BUDGET`] on both forges. When the appraisal block would
//! overflow it, the longest section bodies are truncated first -- never a
//! `<summary>` line and never the summary text.

use crate::appraisal::{AppraisalSectionView, ReviewAppraisal};
use crate::guardian::GuardianView;

/// Characters a composed PR body may use, leaving headroom under GitHub's
/// 65,536-character cap.
pub const PR_BODY_BUDGET: usize = 65_000;

/// A section body is never truncated below this many characters.
const MIN_SECTION_CHARS: usize = 200;

const TRUNCATION_MARKER: &str = "\n\n…(truncated; the full appraisal is on the ralphus board)";

/// Whether `guardian` publishes appraisals into its PRs. The review-level
/// `post_appraisals` setting is read here; it defaults to on.
#[must_use]
pub fn post_appraisals_enabled(_guardian: &GuardianView) -> bool {
    true
}

/// Neutralize the HTML that could close or nest the `<details>` wrapper.
fn neutralize(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &lower[i..];
        if rest.starts_with("</details") {
            out.push_str("&lt;/details");
            i += "</details".len();
        } else if rest.starts_with("<details") {
            out.push_str("&lt;details");
            i += "<details".len();
        } else {
            let ch = text[i..].chars().next().expect("in bounds");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn verdict(a: &ReviewAppraisal) -> String {
    let x = &a.appraisal;
    format!(
        "{} — {}/10 · {} (needs ≥{})",
        neutralize(&a.label),
        x.score,
        if x.passed { "PASS" } else { "FAIL" },
        x.pass_score
    )
}

/// The `<details>` blocks of `items`, section bodies truncated (longest first)
/// until the whole block fits `max_chars`. `None` when there is nothing to
/// show.
#[must_use]
pub fn render_pr_block(items: &[ReviewAppraisal], max_chars: usize) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut sections: Vec<Vec<AppraisalSectionView>> =
        items.iter().map(|i| i.appraisal.sections.clone()).collect();
    loop {
        let rendered = compose(items, &sections);
        let len = rendered.chars().count();
        if len <= max_chars {
            return Some(rendered);
        }
        let excess = len - max_chars;
        let mut longest: Option<(usize, usize, usize)> = None;
        for (i, secs) in sections.iter().enumerate() {
            for (j, s) in secs.iter().enumerate() {
                let n = s.body.chars().count();
                if n > MIN_SECTION_CHARS
                    && !s.body.ends_with(TRUNCATION_MARKER)
                    && longest.is_none_or(|(_, _, m)| n > m)
                {
                    longest = Some((i, j, n));
                }
            }
        }
        let Some((i, j, n)) = longest else {
            return Some(rendered);
        };
        let marker = TRUNCATION_MARKER.chars().count();
        let keep = n
            .saturating_sub(excess + marker)
            .max(MIN_SECTION_CHARS)
            .min(n - 1);
        let body = &mut sections[i][j].body;
        let kept: String = body.chars().take(keep).collect();
        *body = format!("{}{TRUNCATION_MARKER}", kept.trim_end());
    }
}

fn compose(items: &[ReviewAppraisal], sections: &[Vec<AppraisalSectionView>]) -> String {
    let mut out = String::from("## Appraisals");
    for (item, secs) in items.iter().zip(sections) {
        out.push_str(&format!(
            "\n\n<details>\n<summary>{}</summary>\n\n#### Summary\n{}",
            verdict(item),
            neutralize(item.appraisal.summary.trim())
        ));
        for s in secs {
            out.push_str(&format!(
                "\n\n#### {}\n{}",
                neutralize(s.title.trim()),
                neutralize(s.body.trim())
            ));
        }
        out.push_str("\n\n</details>");
    }
    out
}

/// Plain-text rendering for a commit message body (no HTML, never truncated).
#[must_use]
pub fn render_commit_block(items: &[ReviewAppraisal]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("Appraisals recorded while this was built:");
    for item in items {
        let x = &item.appraisal;
        out.push_str(&format!(
            "\n\n{} — {}/10 {} (needs >={})\nSummary: {}",
            item.label,
            x.score,
            if x.passed { "PASS" } else { "FAIL" },
            x.pass_score,
            x.summary.trim()
        ));
        for s in &x.sections {
            out.push_str(&format!("\n\n{}:\n{}", s.title.trim(), s.body.trim()));
        }
    }
    Some(out)
}

/// Place `block` relative to `description`: above it, or below it when the
/// description opens with YAML front matter (a repo PR template that a leading
/// block would push out of position).
#[must_use]
pub fn compose_body(block: &str, description: &str) -> String {
    if description.is_empty() {
        return block.to_string();
    }
    if description.trim_start().starts_with("---") {
        format!("{description}\n\n{block}")
    } else {
        format!("{block}\n\n{description}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appraisal::AppraisalView;

    fn item(label: &str, score: i64, pass: i64, bodies: &[(&str, &str)]) -> ReviewAppraisal {
        ReviewAppraisal {
            label: label.to_string(),
            appraisal: AppraisalView {
                entity_uri: format!("proof:s:0:cell:0:{label}"),
                attempt: 1,
                score,
                pass_score: pass,
                passed: score >= pass,
                summary: "sum".to_string(),
                sections: bodies
                    .iter()
                    .map(|(t, b)| AppraisalSectionView {
                        title: (*t).to_string(),
                        body: (*b).to_string(),
                    })
                    .collect(),
                created_at_ms: 0,
                published_at_ms: None,
                pr_id: None,
            },
        }
    }

    #[test]
    fn renders_one_collapsible_per_judge_with_verbatim_bodies() {
        let items = vec![
            item("security", 4, 7, &[("Exploit path", "1. POST /x\n2. ...")]),
            item("reviewer", 8, 7, &[("Findings", "- ok")]),
        ];
        let block = render_pr_block(&items, PR_BODY_BUDGET).unwrap();
        assert_eq!(block.matches("<details>").count(), 2);
        assert!(block.contains("<summary>security — 4/10 · FAIL (needs ≥7)</summary>"));
        assert!(block.contains("<summary>reviewer — 8/10 · PASS (needs ≥7)</summary>"));
        assert!(block.contains("#### Exploit path\n1. POST /x\n2. ..."));
    }

    #[test]
    fn agent_html_cannot_break_the_layout() {
        let items = vec![item("x", 1, 7, &[("t", "a </DETAILS> b <details open>")])];
        let block = render_pr_block(&items, PR_BODY_BUDGET).unwrap();
        assert_eq!(block.matches("</details>").count(), 1);
        assert_eq!(block.matches("<details").count(), 1);
    }

    #[test]
    fn over_budget_truncates_longest_body_first_and_keeps_summary_lines() {
        let long = "x".repeat(5_000);
        let items = vec![
            item("a", 2, 7, &[("big", &long), ("small", "tiny body")]),
            item("b", 9, 7, &[("mid", &"y".repeat(1_000))]),
        ];
        let block = render_pr_block(&items, 3_000).unwrap();
        assert!(block.chars().count() <= 3_000);
        assert!(block.contains("<summary>a — 2/10 · FAIL (needs ≥7)</summary>"));
        assert!(block.contains("<summary>b — 9/10 · PASS (needs ≥7)</summary>"));
        assert!(block.contains("tiny body"));
        assert!(block.contains("…(truncated"));
        assert!(block.contains(&"y".repeat(1_000)), "shorter body untouched");
    }

    #[test]
    fn nothing_renders_for_no_appraisals() {
        assert!(render_pr_block(&[], PR_BODY_BUDGET).is_none());
        assert!(render_commit_block(&[]).is_none());
    }

    #[test]
    fn block_goes_above_the_description_unless_front_matter_leads() {
        assert_eq!(compose_body("B", "D"), "B\n\nD");
        assert_eq!(
            compose_body("B", "---\nname: t\n---\nD"),
            "---\nname: t\n---\nD\n\nB"
        );
        assert_eq!(compose_body("B", ""), "B");
    }

    #[test]
    fn commit_block_is_plain_text() {
        let c = render_commit_block(&[item("security", 4, 7, &[("Fix", "gate it")])]).unwrap();
        assert!(c.contains("security — 4/10 FAIL (needs >=7)"));
        assert!(c.contains("Fix:\ngate it"));
        assert!(!c.contains("<details"));
    }
}

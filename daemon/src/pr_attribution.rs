//! Ralphus attribution line for newly created PR/MR descriptions (RAL-673).
//!
//! Insertion is attempted deterministically first: the line goes at the end of
//! the first overview-like section (`Overview`, `Summary`, ...). When no such
//! section can be located safely, [`place_in_intro`] reports that (`None`) so
//! the caller can have the review agent place the line, and only if that also
//! fails does [`with_attribution`] append it at the bottom. Every path is
//! idempotent: a description that already carries the line is returned
//! untouched.

/// The exact attribution line.
pub const ATTRIBUTION_LINE: &str =
    "> This PR was created by **Ralphus**, your friendly neighborhood Software Factory";

/// Headings (case-insensitive, trailing `:` ignored) that open an introductory section.
const INTRO_HEADINGS: &[&str] = &[
    "overview",
    "summary",
    "introduction",
    "description",
    "about",
    "about this pr",
    "what",
    "what does this pr do",
    "what this pr does",
    "tl;dr",
    "tldr",
];

/// `Some(fence char, length)` when `line` opens or closes a code fence.
fn fence_of(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// The heading text when `line` is an ATX heading.
fn heading_text(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &t[hashes..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim())
}

/// Per-line "inside a code fence" flags (the fence lines themselves count as
/// inside), plus whether a fence is still open at the end.
fn fence_flags(lines: &[&str]) -> (Vec<bool>, bool) {
    let mut open: Option<(char, usize)> = None;
    let mut flags = Vec::with_capacity(lines.len());
    for line in lines {
        match (open, fence_of(line)) {
            (None, Some(f)) => {
                open = Some(f);
                flags.push(true);
            }
            (Some((c, n)), Some((c2, n2)))
                if c == c2 && n2 >= n && line.trim().chars().all(|x| x == c) =>
            {
                open = None;
                flags.push(true);
            }
            (o, _) => flags.push(o.is_some()),
        }
    }
    (flags, open.is_some())
}

fn append_at_bottom(description: &str, unclosed_fence: Option<(char, usize)>) -> String {
    let base = description.trim_end();
    let mut out = String::from(base);
    if let Some((c, n)) = unclosed_fence {
        out.push('\n');
        out.push_str(&c.to_string().repeat(n));
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(ATTRIBUTION_LINE);
    out
}

/// Whether `description` already carries the attribution line outside a code fence.
#[must_use]
pub fn has_attribution(description: &str) -> bool {
    let text = description.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let (in_fence, _) = fence_flags(&lines);
    lines
        .iter()
        .zip(&in_fence)
        .any(|(l, f)| !f && l.trim() == ATTRIBUTION_LINE)
}

/// Return `description` carrying the attribution line exactly once, at the
/// bottom when it cannot be placed in an intro section.
#[must_use]
pub fn with_attribution(description: &str) -> String {
    place_in_intro(description).unwrap_or_else(|| {
        let text = description.replace("\r\n", "\n");
        let lines: Vec<&str> = text.split('\n').collect();
        let (in_fence, unclosed) = fence_flags(&lines);
        append_at_bottom(&text, unclosed_fence(&lines, &in_fence, unclosed))
    })
}

fn unclosed_fence(lines: &[&str], in_fence: &[bool], unclosed: bool) -> Option<(char, usize)> {
    if !unclosed {
        return None;
    }
    lines
        .iter()
        .zip(in_fence)
        .rev()
        .find(|(l, f)| **f && fence_of(l).is_some())
        .and_then(|(l, _)| fence_of(l))
}

/// Whether an agent's rewrite of `original` is acceptable: it carries the line
/// exactly once outside a fence, and removing that line gives back `original`
/// (ignoring blank-line and trailing-space differences).
#[must_use]
pub fn agent_placement_is_valid(original: &str, candidate: &str) -> bool {
    let normalize = |text: &str| -> Vec<String> {
        text.replace("\r\n", "\n")
            .split('\n')
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };
    if !has_attribution(candidate)
        || candidate.matches(ATTRIBUTION_LINE).count()
            != original.matches(ATTRIBUTION_LINE).count() + 1
    {
        return false;
    }
    let mut kept = normalize(candidate);
    let Some(at) = kept.iter().position(|l| l.trim() == ATTRIBUTION_LINE) else {
        return false;
    };
    kept.remove(at);
    kept == normalize(original)
}

/// Deterministically insert the line at the end of the first intro section.
/// `Some` when the description already carries the line (returned unchanged)
/// or the line was placed; `None` when no intro section can be used safely
/// (none exists, an unclosed fence, or unbalanced HTML in the section).
#[must_use]
pub fn place_in_intro(description: &str) -> Option<String> {
    if has_attribution(description) {
        return Some(description.to_string());
    }
    let text = description.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let (in_fence, unclosed) = fence_flags(&lines);
    if unclosed {
        return None;
    }

    let heading_at = |i: usize| -> bool { !in_fence[i] && heading_text(lines[i]).is_some() };
    let intro = (0..lines.len()).find(|&i| {
        heading_at(i)
            && heading_text(lines[i]).is_some_and(|h| {
                let h = h.trim_end_matches(':').trim().to_ascii_lowercase();
                INTRO_HEADINGS.contains(&h.as_str())
            })
    });
    let start = intro?;
    let end = (start + 1..lines.len())
        .find(|&i| heading_at(i))
        .unwrap_or(lines.len());
    let body = &lines[start + 1..end];
    let joined = body.join("\n");
    // Unbalanced HTML constructs would swallow the line.
    if joined.matches("<!--").count() != joined.matches("-->").count()
        || joined.to_ascii_lowercase().matches("<details").count()
            != joined.to_ascii_lowercase().matches("</details>").count()
    {
        return None;
    }
    // Insert after the last non-blank line of the section.
    let last = (start..end)
        .rev()
        .find(|&i| !lines[i].trim().is_empty())
        .unwrap_or(start);
    let mut out: Vec<String> = lines[..=last].iter().map(|l| (*l).to_string()).collect();
    out.push(String::new());
    out.push(ATTRIBUTION_LINE.to_string());
    if last + 1 < lines.len() {
        out.push(String::new());
        out.extend(lines[last + 1..].iter().map(|l| (*l).to_string()));
        // Collapse the blank lines that followed the section into one.
        let mut collapsed: Vec<String> = Vec::with_capacity(out.len());
        for l in out {
            if l.trim().is_empty() && collapsed.last().is_some_and(|p| p.trim().is_empty()) {
                continue;
            }
            collapsed.push(l);
        }
        return Some(collapsed.join("\n"));
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_overview_section() {
        let d = "## Overview\n\nDoes a thing.\n\n## Details\n\nMore.";
        assert_eq!(
            with_attribution(d),
            format!("## Overview\n\nDoes a thing.\n\n{ATTRIBUTION_LINE}\n\n## Details\n\nMore.")
        );
    }

    #[test]
    fn summary_at_end_of_text() {
        let d = "# Summary\n\nText.\n";
        assert_eq!(
            with_attribution(d),
            format!("# Summary\n\nText.\n\n{ATTRIBUTION_LINE}\n")
        );
    }

    #[test]
    fn no_intro_appends_bottom() {
        assert_eq!(
            with_attribution("## Changes\n\n- a"),
            format!("## Changes\n\n- a\n\n{ATTRIBUTION_LINE}")
        );
        assert_eq!(with_attribution(""), ATTRIBUTION_LINE);
    }

    #[test]
    fn place_in_intro_declines_when_it_cannot_place_safely() {
        assert_eq!(place_in_intro("## Changes\n\n- a"), None);
        assert_eq!(place_in_intro("## Summary\n\n```rust\ncode"), None);
        assert_eq!(place_in_intro("## Summary\n\n<!-- x\n\n## N\n\n-->"), None);
        assert!(place_in_intro("## Summary\n\nx").is_some());
        let done = format!("## Changes\n\n{ATTRIBUTION_LINE}");
        assert_eq!(place_in_intro(&done), Some(done.clone()));
    }

    #[test]
    fn agent_placement_validation() {
        let original = "## Changes\n\n- a";
        let good = format!("## Changes\n\n- a\n\n{ATTRIBUTION_LINE}");
        assert!(agent_placement_is_valid(original, &good));
        assert!(!agent_placement_is_valid(original, original));
        assert!(!agent_placement_is_valid(
            original,
            &format!("rewritten\n{ATTRIBUTION_LINE}")
        ));
        let twice = format!("{good}\n\n{ATTRIBUTION_LINE}");
        assert!(!agent_placement_is_valid(original, &twice));
    }

    #[test]
    fn idempotent() {
        let once = with_attribution("## Summary\n\nx\n\n## More\n\ny");
        assert_eq!(with_attribution(&once), once);
        let bottom = with_attribution("plain");
        assert_eq!(with_attribution(&bottom), bottom);
    }

    #[test]
    fn heading_inside_fence_is_ignored() {
        let d = "```\n## Summary\n```\n\n## Changes\n\nx";
        assert_eq!(with_attribution(d), format!("{d}\n\n{ATTRIBUTION_LINE}"));
    }

    #[test]
    fn existing_line_in_fence_does_not_count() {
        let d = format!("```\n{ATTRIBUTION_LINE}\n```");
        assert_eq!(with_attribution(&d).matches(ATTRIBUTION_LINE).count(), 2);
    }

    #[test]
    fn unclosed_fence_is_closed_before_line() {
        let out = with_attribution("## Summary\n\n```rust\ncode");
        assert_eq!(
            out,
            format!("## Summary\n\n```rust\ncode\n```\n\n{ATTRIBUTION_LINE}")
        );
    }

    #[test]
    fn unbalanced_html_comment_falls_back_to_bottom() {
        let d = "## Summary\n\n<!-- note\n\n## Next\n\nx -->";
        assert!(with_attribution(d).ends_with(ATTRIBUTION_LINE));
    }

    #[test]
    fn list_in_section_keeps_blank_separation() {
        let d = "## Summary\n\n- a\n- b\n\n## Next";
        assert_eq!(
            with_attribution(d),
            format!("## Summary\n\n- a\n- b\n\n{ATTRIBUTION_LINE}\n\n## Next")
        );
    }
}

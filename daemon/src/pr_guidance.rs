//! PR guidance: free-text instructions appended to the prompt of the agent that
//! writes a PR/MR title and description.
//!
//! Guidance comes from several layers and they stack: the global config, then
//! the review's own setting (the per-review `pr_guidance`, or the project's
//! review settings when the review has none), then every `.ralphus.toml` from
//! the repository root down to the leaf directories the PR touches. Each entry
//! may carry `paths` globs; an entry participates only when one of the PR's
//! changed files matches, and an entry with no `paths` always participates.
//! File-sourced entries are truncated to [`MAX_PR_GUIDANCE_CHARS`] with a
//! visible marker; settings entries are rejected over that size when set.

use std::path::{Path, PathBuf};

use ralphus_core::project_hints::path_matches;
pub use ralphus_core::schema::MAX_PR_GUIDANCE_CHARS;
use serde::{Deserialize, Serialize};

/// One layer's guidance text and its optional path filter.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GuidanceEntry {
    pub text: String,
    /// Globs over repo-relative changed paths; empty means "all changes".
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Build an entry from a layer's raw values; `None` when the text is blank.
#[must_use]
pub fn entry(text: Option<&str>, paths: Option<&[String]>) -> Option<GuidanceEntry> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    Some(GuidanceEntry {
        text: text.to_string(),
        paths: paths.map(<[String]>::to_vec).unwrap_or_default(),
    })
}

/// Reject guidance over the size cap (used when it is set).
///
/// # Errors
/// Returns a message naming the cap when `text` is too long.
pub fn check_size(text: &str) -> Result<(), String> {
    let n = text.chars().count();
    if n > MAX_PR_GUIDANCE_CHARS {
        return Err(format!(
            "pr_guidance is {n} characters; the limit is {MAX_PR_GUIDANCE_CHARS}"
        ));
    }
    Ok(())
}

/// Truncate file-sourced guidance to the cap, marking the cut visibly.
#[must_use]
pub fn truncate_with_marker(text: &str) -> String {
    if text.chars().count() <= MAX_PR_GUIDANCE_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_PR_GUIDANCE_CHARS).collect();
    format!("{kept}\n[... guidance truncated at {MAX_PR_GUIDANCE_CHARS} characters ...]")
}

fn applies(entry: &GuidanceEntry, changed: &[String]) -> bool {
    entry.paths.is_empty()
        || changed
            .iter()
            .any(|p| entry.paths.iter().any(|g| path_matches(g, p)))
}

/// `.ralphus.toml` entries for a PR touching `changed` (repo-relative paths) in
/// the checkout at `root`, ordered root to leaf. A file in a subdirectory
/// participates only when a changed file lives under it, and its globs match
/// paths relative to its own directory.
#[must_use]
pub fn file_entries(root: &Path, changed: &[String]) -> Vec<GuidanceEntry> {
    let mut configs: Vec<(PathBuf, Option<String>)> = Vec::new();
    for p in crate::config::find_all_project_configs(root) {
        configs.push((p, None));
    }
    let mut dirs: Vec<String> = Vec::new();
    for f in changed {
        let mut cur = Path::new(f).parent();
        while let Some(d) = cur {
            let s = d.to_string_lossy().replace('\\', "/");
            if !s.is_empty() && !dirs.contains(&s) {
                dirs.push(s);
            }
            cur = d.parent();
        }
    }
    dirs.sort_by_key(|d| d.matches('/').count());
    for d in dirs {
        let p = root.join(&d).join(".ralphus.toml");
        if p.is_file() && !configs.iter().any(|(c, _)| c == &p) {
            configs.push((p, Some(d)));
        }
    }
    let mut out = Vec::new();
    for (path, sub) in configs {
        let cfg = crate::config::load_file(&path);
        let Some(mut e) = entry(cfg.pr_guidance.as_deref(), cfg.pr_guidance_paths.as_deref())
        else {
            continue;
        };
        e.text = truncate_with_marker(&e.text);
        let rel: Vec<String> = match &sub {
            None => changed.to_vec(),
            Some(d) => {
                let prefix = format!("{d}/");
                changed
                    .iter()
                    .filter_map(|c| c.strip_prefix(&prefix).map(str::to_string))
                    .collect()
            }
        };
        if sub.is_some() && rel.is_empty() {
            continue;
        }
        if applies(&e, &rel) {
            e.paths.clear();
            out.push(e);
        }
    }
    out
}

/// The prompt section for the entries that apply, in the order given; empty
/// when none apply so the prompt stays unchanged.
#[must_use]
pub fn guidance_note(entries: &[GuidanceEntry], changed: &[String]) -> String {
    let texts: Vec<&str> = entries
        .iter()
        .filter(|e| applies(e, changed))
        .map(|e| e.text.as_str())
        .collect();
    if texts.is_empty() {
        return String::new();
    }
    format!(
        "\nProject guidance for PR descriptions (additional instructions, applied in \
         order; the PR template's headings and structure still take precedence):\n{}\n",
        texts.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(text: &str, paths: &[&str]) -> GuidanceEntry {
        GuidanceEntry {
            text: text.into(),
            paths: paths.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn empty_when_nothing_applies() {
        assert_eq!(guidance_note(&[], &["a.rs".into()]), "");
        assert_eq!(guidance_note(&[e("x", &["docs/**"])], &["a.rs".into()]), "");
    }

    #[test]
    fn entries_stack_in_order_and_filter_by_path() {
        let es = [
            e("global", &[]),
            e("svc only", &["svc/**"]),
            e("docs-entry", &["docs/**"]),
        ];
        let note = guidance_note(&es, &["svc/a.rs".into()]);
        let g = note.find("global").unwrap();
        let s = note.find("svc only").unwrap();
        assert!(g < s);
        assert!(!note.contains("docs-entry"));
    }

    #[test]
    fn blank_text_makes_no_entry() {
        assert!(entry(Some("  "), None).is_none());
        assert!(entry(None, None).is_none());
    }

    #[test]
    fn size_cap_rejects_and_truncates() {
        let long = "x".repeat(MAX_PR_GUIDANCE_CHARS + 1);
        assert!(check_size(&long).is_err());
        assert!(check_size(&"x".repeat(MAX_PR_GUIDANCE_CHARS)).is_ok());
        assert!(truncate_with_marker(&long).contains("truncated"));
        assert_eq!(truncate_with_marker("short"), "short");
    }

    #[test]
    fn subfolder_file_stacks_after_parent() {
        let dir = ralphus_test_support::scratch(format!("pr-guid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("svc")).unwrap();
        std::fs::write(
            dir.join("svc/.ralphus.toml"),
            "[review]\npr_guidance = \"leaf\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(".ralphus.toml"),
            "[review]\npr_guidance = \"root\"\n",
        )
        .unwrap();
        let got = file_entries(&dir, &["svc/a.rs".into()]);
        let texts: Vec<&str> = got.iter().map(|g| g.text.as_str()).collect();
        let r = texts.iter().position(|t| *t == "root");
        let l = texts.iter().position(|t| *t == "leaf");
        assert!(r.is_some() && l.is_some() && r < l, "{texts:?}");
        let other = file_entries(&dir, &["other/a.rs".into()]);
        assert!(!other.iter().any(|g| g.text == "leaf"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

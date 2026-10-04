//! RAL-565: manual-check *hints* in a project's `.ralphus.toml`.
//!
//! A project file is not a task submission. Its `[[review.action.hint]]` tables
//! describe manual checks the project's maintainers recommend, plus guidance on
//! when to include each one. They inform whoever writes the real submission --
//! the auto-review agent, `ralphus tutor`, a person -- and are never run, merged
//! into a submission, or read by the daemon's scheduler. A submission that
//! defines its own `[[review.action]]` checks is authoritative; layering project
//! hints on top of it would break "Run all" with a minimal set of checks
//! whenever the submission and the project differ even slightly.
//!
//! ```toml
//! [[review.action.hint]]
//! label = "GUI smoke test"
//! command = "npm test"
//! auto_run = true
//! hint.include_when = "Any change under librarian/assets/ should include this check."
//! hint.paths = ["librarian/assets/**"]
//!
//! [[review.action.hint.prepare]]
//! command = "npm ci"
//! ```
//!
//! Hints are gathered from the nearest `.ralphus.toml` up through every parent
//! directory. For one `label`, the file nearest the start directory wins and a
//! parent only fills labels the nearer files did not mention.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::schema::PreparationStepDef;

/// Guidance on when a hint's check belongs in a review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HintGuidance {
    /// Plain-language rule, e.g. "Any change to X GUI should include this
    /// check." Read by an agent or a person.
    #[serde(default)]
    pub include_when: Option<String>,
    /// Optional path globs (`*` within a segment, `**` across segments) that
    /// make the rule checkable against a diff.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// One suggested manual check.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionHint {
    /// Identity of the hint. A nearer file's hint replaces a parent's with the
    /// same label.
    pub label: String,
    /// Suggested shell command; the submitter copies it into a real
    /// `[[review.action]]`.
    #[serde(default)]
    pub command: Option<String>,
    /// Suggested prompt, as an alternative to `command`.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub success: Option<String>,
    /// Whether the check suits auto-run. `false` means it should not be
    /// offered for auto-run (it is interactive, destructive, or heavy).
    #[serde(default)]
    pub auto_run: Option<bool>,
    /// Suggested build commands, written as `[[review.action.hint.prepare]]`.
    #[serde(default)]
    pub prepare: Vec<PreparationStepDef>,
    /// When the check applies, written as `hint.include_when` / `hint.paths`.
    #[serde(default)]
    pub hint: Option<HintGuidance>,
}

/// A hint and the file it came from.
#[derive(Debug, Clone)]
pub struct CollectedHint {
    pub hint: ActionHint,
    pub source: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct ProjectFile {
    review: Option<ReviewSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ReviewSection {
    action: Option<ActionSection>,
}

#[derive(Debug, Default, Deserialize)]
struct ActionSection {
    #[serde(default)]
    hint: Vec<ActionHint>,
}

/// Parse the hints out of one `.ralphus.toml`'s text. A file with no
/// `[[review.action.hint]]` yields an empty list; any other section is ignored.
///
/// # Errors
/// Returns a message when the TOML is malformed, a hint has an unknown key, an
/// empty `label`, both a `command` and a `prompt`, or a repeated `label`.
pub fn parse_project_hints(text: &str) -> Result<Vec<ActionHint>, String> {
    let file: ProjectFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let hints = file
        .review
        .and_then(|review| review.action)
        .map(|action| action.hint)
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for hint in &hints {
        if hint.label.trim().is_empty() {
            return Err("a [[review.action.hint]] has an empty `label`".to_string());
        }
        if hint.command.is_some() && hint.prompt.is_some() {
            return Err(format!(
                "hint \"{}\" sets both `command` and `prompt`; set at most one",
                hint.label
            ));
        }
        if !seen.insert(hint.label.clone()) {
            return Err(format!(
                "hint label \"{}\" appears more than once in this file",
                hint.label
            ));
        }
    }
    Ok(hints)
}

/// Gather hints from the `.ralphus.toml` at or above `start`, nearest file
/// first, keeping the nearest hint for each label. A file that cannot be read or
/// parsed contributes a warning instead of hiding the files above it.
#[must_use]
pub fn collect_project_hints(start: &Path) -> (Vec<CollectedHint>, Vec<String>) {
    let mut collected: Vec<CollectedHint> = Vec::new();
    let mut warnings = Vec::new();
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".ralphus.toml");
        if candidate.is_file() {
            match std::fs::read_to_string(&candidate) {
                Err(e) => warnings.push(format!("{}: {e}", candidate.display())),
                Ok(text) => match parse_project_hints(&text) {
                    Err(e) => warnings.push(format!("{}: {e}", candidate.display())),
                    Ok(hints) => {
                        for hint in hints {
                            if !collected.iter().any(|c| c.hint.label == hint.label) {
                                collected.push(CollectedHint {
                                    hint,
                                    source: candidate.clone(),
                                });
                            }
                        }
                    }
                },
            }
        }
        dir = d.parent();
    }
    (collected, warnings)
}

/// Render hints as a block of guidance for an agent or a person. Empty when
/// there are no hints. The wording states that hints inform the submission and
/// are not part of it.
#[must_use]
pub fn render_hints(hints: &[CollectedHint]) -> String {
    if hints.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Project manual-check hints. These are recommendations from the project's \
         .ralphus.toml, nearest file first; they inform your [[review.action]] entries and \
         are not part of a submission themselves. Include a hint's check when its rule \
         applies to the change, copy its command and prepare steps into a real \
         [[review.action]], and do not set auto_run = true on a check whose hint says \
         auto_run = false.\n",
    );
    for collected in hints {
        let hint = &collected.hint;
        out.push_str(&format!("\n- {}", hint.label));
        if let Some(command) = &hint.command {
            out.push_str(&format!("\n  command: {command}"));
        }
        if let Some(prompt) = &hint.prompt {
            out.push_str(&format!("\n  prompt: {prompt}"));
        }
        for step in &hint.prepare {
            if let Some(command) = &step.command {
                out.push_str(&format!("\n  prepare: {}", command.commands().join(" && ")));
            }
        }
        if let Some(auto_run) = hint.auto_run {
            out.push_str(&format!("\n  auto_run: {auto_run}"));
        }
        if let Some(description) = &hint.description {
            out.push_str(&format!("\n  description: {description}"));
        }
        if let Some(success) = &hint.success {
            out.push_str(&format!("\n  success: {success}"));
        }
        if let Some(guidance) = &hint.hint {
            if let Some(rule) = &guidance.include_when {
                out.push_str(&format!("\n  include when: {rule}"));
            }
            if !guidance.paths.is_empty() {
                out.push_str(&format!("\n  paths: {}", guidance.paths.join(", ")));
            }
        }
        out.push_str(&format!("\n  from: {}", collected.source.display()));
    }
    out
}

/// Whether `path` (forward-slash separated, relative) matches `glob`, where `*`
/// matches within one path segment and `**` matches across segments.
#[must_use]
pub fn path_matches(glob: &str, path: &str) -> bool {
    fn go(glob: &[u8], path: &[u8]) -> bool {
        match (glob.first(), path.first()) {
            (None, None) => true,
            (Some(b'*'), _) if glob.get(1) == Some(&b'*') => {
                let rest = &glob[2..];
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=path.len()).any(|i| go(rest, &path[i..]))
            }
            (Some(b'*'), _) => {
                let rest = &glob[1..];
                (0..=path.len())
                    .take_while(|&i| i == 0 || path[i - 1] != b'/')
                    .any(|i| go(rest, &path[i..]))
            }
            (Some(g), Some(p)) if g == p => go(&glob[1..], &path[1..]),
            _ => false,
        }
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// The hints whose `hint.paths` match at least one of `changed_paths`. A hint
/// with no `paths` is never selected here -- it carries only prose for an agent
/// to judge.
#[must_use]
pub fn hints_for_paths<'a>(
    hints: &'a [CollectedHint],
    changed_paths: &[&str],
) -> Vec<&'a CollectedHint> {
    hints
        .iter()
        .filter(|collected| {
            collected.hint.hint.as_ref().is_some_and(|guidance| {
                guidance
                    .paths
                    .iter()
                    .any(|glob| changed_paths.iter().any(|path| path_matches(glob, path)))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[review]
auto_run = false

[[review.action.hint]]
label = "GUI smoke test"
command = "npm test"
auto_run = true
hint.include_when = "Any change under librarian/assets/ should include this check."
hint.paths = ["librarian/assets/**"]

[[review.action.hint.prepare]]
command = "npm ci"

[[review.action.hint]]
label = "Slow soak"
prompt = "Run the soak test"
auto_run = false
"#;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ralphus-hints-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_hints_with_prepare_and_guidance() {
        let hints = parse_project_hints(SAMPLE).unwrap();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].label, "GUI smoke test");
        assert_eq!(hints[0].command.as_deref(), Some("npm test"));
        assert_eq!(hints[0].auto_run, Some(true));
        assert_eq!(hints[0].prepare.len(), 1);
        let guidance = hints[0].hint.as_ref().unwrap();
        assert_eq!(guidance.paths, vec!["librarian/assets/**".to_string()]);
        assert!(
            guidance
                .include_when
                .as_deref()
                .unwrap()
                .contains("librarian")
        );
        assert_eq!(hints[1].auto_run, Some(false));
    }

    #[test]
    fn a_file_without_hints_yields_none_and_other_sections_are_ignored() {
        assert!(
            parse_project_hints("[review]\nauto_run = true\n")
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_project_hints("[agent.profiles.x]\nbackend = \"codex\"\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_unknown_keys_empty_labels_both_command_and_prompt_and_repeats() {
        let bad = |body: &str| {
            parse_project_hints(&format!("[[review.action.hint]]\n{body}")).unwrap_err()
        };
        assert!(bad("label = \"a\"\ncomand = \"x\"\n").contains("comand"));
        assert!(bad("label = \" \"\n").contains("empty"));
        assert!(bad("label = \"a\"\ncommand = \"x\"\nprompt = \"y\"\n").contains("at most one"));
        let twice =
            "[[review.action.hint]]\nlabel = \"a\"\n[[review.action.hint]]\nlabel = \"a\"\n";
        assert!(
            parse_project_hints(twice)
                .unwrap_err()
                .contains("more than once")
        );
    }

    #[test]
    fn the_nearest_file_wins_per_label_and_parents_fill_the_rest() {
        let root = scratch("walk");
        let leaf = root.join("pkg").join("ui");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[[review.action.hint]]\nlabel = \"shared\"\ncommand = \"parent-shared\"\n\
             [[review.action.hint]]\nlabel = \"only-parent\"\ncommand = \"parent-only\"\n",
        )
        .unwrap();
        std::fs::write(
            leaf.join(".ralphus.toml"),
            "[[review.action.hint]]\nlabel = \"shared\"\ncommand = \"leaf-shared\"\n",
        )
        .unwrap();
        let (hints, warnings) = collect_project_hints(&leaf);
        assert!(warnings.is_empty(), "{warnings:?}");
        let by_label = |label: &str| {
            hints
                .iter()
                .find(|c| c.hint.label == label)
                .and_then(|c| c.hint.command.clone())
        };
        assert_eq!(by_label("shared").as_deref(), Some("leaf-shared"));
        assert_eq!(by_label("only-parent").as_deref(), Some("parent-only"));
        assert_eq!(hints.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_nearer_file_warns_but_does_not_hide_its_parent() {
        let root = scratch("broken");
        let leaf = root.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(
            root.join(".ralphus.toml"),
            "[[review.action.hint]]\nlabel = \"ok\"\n",
        )
        .unwrap();
        std::fs::write(
            leaf.join(".ralphus.toml"),
            "[[review.action.hint]]\nlabel = \n",
        )
        .unwrap();
        let (hints, warnings) = collect_project_hints(&leaf);
        assert_eq!(hints.len(), 1);
        assert_eq!(warnings.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rendered_hints_say_they_inform_a_submission() {
        let hints = parse_project_hints(SAMPLE).unwrap();
        let collected: Vec<_> = hints
            .into_iter()
            .map(|hint| CollectedHint {
                hint,
                source: PathBuf::from("/repo/.ralphus.toml"),
            })
            .collect();
        let text = render_hints(&collected);
        assert!(text.contains("not part of a submission"));
        assert!(text.contains("GUI smoke test"));
        assert!(text.contains("include when: Any change under librarian/assets/"));
        assert!(text.contains("auto_run: false"));
        assert_eq!(render_hints(&[]), "");
    }

    #[test]
    fn ralphus_own_project_file_parses_and_every_hint_says_when_it_applies() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../.ralphus.toml");
        let text = std::fs::read_to_string(&path).expect("the repository's .ralphus.toml");
        let hints = parse_project_hints(&text).unwrap();
        assert!(!hints.is_empty());
        for hint in &hints {
            let guidance = hint.hint.as_ref().unwrap_or_else(|| {
                panic!(
                    "hint \"{}\" has no hint.include_when / hint.paths",
                    hint.label
                )
            });
            assert!(
                guidance.include_when.is_some() && !guidance.paths.is_empty(),
                "hint \"{}\" must say when it applies, in prose and as path globs",
                hint.label
            );
            assert!(
                hint.command.is_some() || hint.prompt.is_some(),
                "hint \"{}\" suggests nothing to run",
                hint.label
            );
            assert!(
                hint.auto_run.is_some(),
                "hint \"{}\" must state whether it suits auto_run",
                hint.label
            );
        }
        let (collected, warnings) = collect_project_hints(path.parent().unwrap());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(collected.len(), hints.len());
    }

    #[test]
    fn path_globs_match_within_and_across_segments() {
        assert!(path_matches(
            "librarian/assets/**",
            "librarian/assets/board/65-reviews.js"
        ));
        assert!(path_matches(
            "librarian/assets/board/*.js",
            "librarian/assets/board/65-reviews.js"
        ));
        assert!(!path_matches(
            "librarian/assets/board/*.js",
            "librarian/assets/board/sub/x.js"
        ));
        assert!(path_matches("**/schema.rs", "core/src/schema.rs"));
        assert!(!path_matches("daemon/**", "core/src/schema.rs"));
    }

    #[test]
    fn hints_are_selected_by_changed_paths() {
        let hints = parse_project_hints(SAMPLE).unwrap();
        let collected: Vec<_> = hints
            .into_iter()
            .map(|hint| CollectedHint {
                hint,
                source: PathBuf::from(".ralphus.toml"),
            })
            .collect();
        let hit = hints_for_paths(&collected, &["librarian/assets/board/65-reviews.js"]);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].hint.label, "GUI smoke test");
        assert!(hints_for_paths(&collected, &["daemon/src/server.rs"]).is_empty());
    }
}
